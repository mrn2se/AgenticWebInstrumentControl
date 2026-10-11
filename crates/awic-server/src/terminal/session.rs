//! [`Session`] (one open terminal source, possibly shared by several
//! connections) and [`SessionManager`] (keyed lookup, creation, teardown).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify};

use awic_core::EventBus;
use awic_proto::{
    DecodeOpenSpec, Event, SerialSettings, TerminalKind, TerminalSourceInfo, TerminalState,
    PROTOCOL_VERSION,
};

use super::{SCROLLBACK_CAPACITY, SUBSCRIBER_QUEUE_CAPACITY};

/// Source-specific settings `term.open` may carry (SPEC §11.2's `serial`
/// field, and this task's `decode`). A single bag rather than widening
/// `connect`'s parameter list per source kind: `serial` is read only by the
/// `serial` provider, `decode` only by the `decode` provider, and adding a
/// future source's own settings extends this struct rather than every
/// provider's signature.
#[derive(Debug, Clone, Default)]
pub struct OpenSettings {
    pub serial: Option<SerialSettings>,
    pub decode: Option<DecodeOpenSpec>,
}

impl OpenSettings {
    pub fn serial(settings: SerialSettings) -> Self {
        Self {
            serial: Some(settings),
            ..Default::default()
        }
    }

    pub fn decode(spec: DecodeOpenSpec) -> Self {
        Self {
            decode: Some(spec),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerminalError {
    #[error("unknown terminal source {0:?}")]
    UnknownSource(String),
    #[error("source {0:?} is not available: {1}")]
    Unavailable(String, String),
    #[error("serial settings (baud, data_bits, parity, stop_bits) are required to open {0:?}")]
    SettingsRequired(String),
    #[error("no open session {0}")]
    UnknownSession(u16),
    #[error("session {0} is read-only")]
    ReadOnly(u16),
    #[error("term.write must set exactly one of data or data_b64")]
    BadWritePayload,
    #[error("data_b64 is not valid base64: {0}")]
    BadBase64(String),
    #[error("{0} is open in another program; AWIC opens serial ports exclusively, so close it there first")]
    Busy(String),
    #[error("{port} is already open at {open}; join it with those settings (or none), or close it everywhere and reopen at {requested}")]
    SettingsConflict {
        port: String,
        open: String,
        requested: String,
    },
    #[error("{0}")]
    Io(String),
}

/// What a provider hands the manager once a session is opened: a live
/// connection driven until every subscriber leaves, plus whatever should
/// seed a *brand-new* session's scrollback (the log source's buffered
/// lines; empty for serial, which has nothing to replay before the port
/// itself starts producing bytes). Ignored when joining an already-open
/// session — its scrollback already holds this source's history.
pub struct Connected {
    pub connection: Box<dyn SourceConnection>,
    pub initial_backlog: Vec<u8>,
    pub label: String,
}

/// One thing the terminal module can list and open sessions against.
#[async_trait]
pub trait TerminalSourceProvider: Send + Sync {
    fn kind(&self) -> TerminalKind;
    /// Discovered fresh every call — a serial source list changes with the
    /// bus, so nothing here is cached.
    fn list(&self) -> Vec<TerminalSourceInfo>;
    /// Resolves one specific source id to its info. Default: look it up in
    /// [`Self::list`] — the right answer for `serial`/`log`/`scpi`, whose
    /// every valid id is enumerable in advance. The `decode` provider
    /// overrides this: a `decode:<n>` id is minted fresh per `term.open`
    /// call (SPEC §11.2 — never shared/joined like `serial`), so it can
    /// never appear in `list()`.
    fn describe(&self, source_id: &str) -> Option<TerminalSourceInfo> {
        self.list().into_iter().find(|s| s.source == source_id)
    }
    /// The id `term.open` should actually be opened against, given the id a
    /// client asked for. Default: unchanged — every `serial`/`log`/`scpi` id
    /// is already specific enough to be the `by_source` join key as-is. The
    /// `decode` provider overrides this to mint a fresh, never-reused id
    /// regardless of `base` (SPEC §11.2: unlike `serial`, two `decode` opens
    /// are never joined even against the same recording, since they can
    /// carry different decoder configurations).
    fn mint_fresh_id(&self, base: &str) -> String {
        base.to_string()
    }
    async fn connect(
        &self,
        source_id: &str,
        settings: OpenSettings,
    ) -> Result<Connected, TerminalError>;
    /// Whether a session against this provider's sources outlives every
    /// subscriber leaving (the `log` source: one persistent stream for the
    /// server's whole lifetime) or should be torn down, closing the
    /// underlying resource, once the last connection leaves (`serial`,
    /// SPEC §11.2.1).
    fn persistent(&self) -> bool {
        false
    }
}

/// One chunk of output, or a lifecycle transition, from an open
/// [`SourceConnection`].
pub enum SourceEvent {
    Data(Vec<u8>),
    /// The device disappeared; scrollback is kept, session -> `waiting`.
    Waiting(String),
    /// The device came back after `Waiting`; session -> `open` again.
    Resumed,
    /// The source will produce nothing more, ever (unlike `Waiting`, not
    /// recoverable).
    Closed(Option<String>),
}

/// A live, driven connection to one terminal source. Reads and writes are
/// interleaved by [`run_session`] calling `recv`/`write` from one task, so
/// an implementation never needs its own internal locking between them.
#[async_trait]
pub trait SourceConnection: Send {
    async fn recv(&mut self) -> SourceEvent;
    async fn write(&mut self, data: &[u8]) -> Result<(), TerminalError>;
}

struct Subscriber {
    tx: mpsc::Sender<Vec<u8>>,
    dropped: Arc<AtomicUsize>,
}

/// `9600 8N1`, `115200 7E2`: a frame format the way a person writes it.
pub fn frame_format(s: &SerialSettings) -> String {
    let parity = match s.parity {
        awic_proto::Parity::None => 'N',
        awic_proto::Parity::Even => 'E',
        awic_proto::Parity::Odd => 'O',
    };
    let stop = match s.stop_bits {
        awic_proto::StopBits::One => 1,
        awic_proto::StopBits::Two => 2,
    };
    format!("{} {}{}{}", s.baud, s.data_bits, parity, stop)
}

/// One open terminal source, shared by every connection that has joined it.
pub struct Session {
    pub id: u16,
    pub source_id: String,
    pub kind: TerminalKind,
    pub label: String,
    /// `serial` sessions: the frame format the port was opened with, which
    /// every later joiner shares.
    pub serial: Option<SerialSettings>,
    state: AsyncMutex<TerminalState>,
    scrollback: AsyncMutex<VecDeque<u8>>,
    subscribers: AsyncMutex<HashMap<u64, Subscriber>>,
    next_subscriber_id: AtomicU64,
    write_tx: mpsc::Sender<Vec<u8>>,
    shutdown: Arc<Notify>,
}

impl Session {
    pub async fn current_state(&self) -> TerminalState {
        *self.state.lock().await
    }

    /// Registers a new subscriber and immediately hands back the session's
    /// current scrollback as its first queued chunk (SPEC §11.2.1: replayed
    /// on join) plus the receiver it should read output from from then on.
    async fn subscribe(&self) -> (u64, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE_CAPACITY);
        let backlog = self.scrollback.lock().await;
        if !backlog.is_empty() {
            // A fresh channel; this send cannot fail or block.
            let _ = tx.try_send(backlog.iter().copied().collect());
        }
        drop(backlog);
        let id = self.next_subscriber_id.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().await.insert(
            id,
            Subscriber {
                tx,
                dropped: Arc::new(AtomicUsize::new(0)),
            },
        );
        (id, rx)
    }

    /// Removes a subscriber; returns `true` if that was the last one.
    async fn unsubscribe(&self, subscriber_id: u64) -> bool {
        let mut subs = self.subscribers.lock().await;
        subs.remove(&subscriber_id);
        subs.is_empty()
    }

    /// Appends to scrollback (capped at [`SCROLLBACK_CAPACITY`]) and fans
    /// `data` out to every subscriber, applying the drop-and-count policy
    /// (SPEC §3.5) per subscriber: a chunk that can't be enqueued for a slow
    /// subscriber is dropped and counted for *that* subscriber only, with a
    /// `[<n> bytes dropped]` marker prepended to its next delivered chunk.
    async fn append_and_broadcast(&self, data: Vec<u8>) {
        {
            let mut backlog = self.scrollback.lock().await;
            backlog.extend(data.iter().copied());
            let over = backlog.len().saturating_sub(SCROLLBACK_CAPACITY);
            if over > 0 {
                backlog.drain(0..over);
            }
        }
        let subs = self.subscribers.lock().await;
        for sub in subs.values() {
            let dropped = sub.dropped.swap(0, Ordering::AcqRel);
            let chunk = if dropped > 0 {
                let mut marked = format!("[... {dropped} bytes dropped ...]").into_bytes();
                marked.extend_from_slice(&data);
                marked
            } else {
                data.clone()
            };
            let len = chunk.len();
            if sub.tx.try_send(chunk).is_err() {
                sub.dropped.fetch_add(len, Ordering::AcqRel);
            }
        }
    }

    async fn set_state(&self, new_state: TerminalState) {
        *self.state.lock().await = new_state;
    }
}

/// Drives one session's underlying [`SourceConnection`] for its whole
/// lifetime: forwards output into the session (and scrollback), applies
/// writes handed to it via `write_rx`, and reflects `Waiting`/`Resumed`/
/// `Closed` transitions as both session state and a `term.state` event.
/// Exits (and lets the manager drop the session) on `Closed` or on
/// `shutdown` — the signal the manager sends once the last subscriber of a
/// non-persistent (serial) session leaves.
async fn run_session(
    session: Arc<Session>,
    mut connection: Box<dyn SourceConnection>,
    mut write_rx: mpsc::Receiver<Vec<u8>>,
    event_bus: EventBus,
) {
    loop {
        tokio::select! {
            biased;
            _ = session.shutdown.notified() => {
                tracing::info!(session = session.id, source = %session.source_id, "closing terminal session: last subscriber left");
                break;
            }
            written = write_rx.recv() => {
                let Some(data) = written else { break };
                if let Err(err) = connection.write(&data).await {
                    tracing::warn!(session = session.id, %err, "terminal write failed");
                }
            }
            event = connection.recv() => {
                match event {
                    SourceEvent::Data(bytes) => session.append_and_broadcast(bytes).await,
                    SourceEvent::Waiting(msg) => {
                        session.set_state(TerminalState::Waiting).await;
                        event_bus.publish(Event::TermState {
                            v: PROTOCOL_VERSION,
                            session: session.id,
                            state: TerminalState::Waiting,
                            msg: Some(msg),
                        });
                    }
                    SourceEvent::Resumed => {
                        session.set_state(TerminalState::Open).await;
                        event_bus.publish(Event::TermState {
                            v: PROTOCOL_VERSION,
                            session: session.id,
                            state: TerminalState::Open,
                            msg: None,
                        });
                    }
                    SourceEvent::Closed(msg) => {
                        session.set_state(TerminalState::Closed).await;
                        event_bus.publish(Event::TermState {
                            v: PROTOCOL_VERSION,
                            session: session.id,
                            state: TerminalState::Closed,
                            msg,
                        });
                        break;
                    }
                }
            }
        }
    }
}

struct Inner {
    providers: Vec<Arc<dyn TerminalSourceProvider>>,
    /// Keyed by source id (`"serial:<intf>"`, `"log"`); the join point for
    /// "opening an open source joins it" (SPEC §11.2.1).
    by_source: AsyncMutex<HashMap<String, Arc<Session>>>,
    next_session_id: AtomicU16,
    event_bus: EventBus,
}

/// Owns every open terminal session and the providers that can create new
/// ones. Cheap to clone (an `Arc` underneath), like [`crate::state::AppState`].
#[derive(Clone)]
pub struct SessionManager(Arc<Inner>);

impl SessionManager {
    pub fn new(providers: Vec<Arc<dyn TerminalSourceProvider>>, event_bus: EventBus) -> Self {
        Self(Arc::new(Inner {
            providers,
            by_source: AsyncMutex::new(HashMap::new()),
            next_session_id: AtomicU16::new(1),
            event_bus,
        }))
    }

    /// `term.sources`: every provider's current list, concatenated.
    pub fn sources(&self) -> Vec<TerminalSourceInfo> {
        self.0.providers.iter().flat_map(|p| p.list()).collect()
    }

    /// Resolves the id `term.open`'s `source` should actually be opened
    /// against (`dispatch.rs` calls this before [`Self::open`]) — identity
    /// for every source except the `decode` picker entry, which mints a
    /// fresh id per call (see [`TerminalSourceProvider::mint_fresh_id`]).
    /// An id `find` doesn't recognize passes through unchanged; `open` is
    /// what actually reports `UnknownSource` for it.
    pub fn resolve_open_id(&self, source_id: &str) -> String {
        self.find(source_id)
            .map(|(provider, _)| provider.mint_fresh_id(source_id))
            .unwrap_or_else(|| source_id.to_string())
    }

    fn find(
        &self,
        source_id: &str,
    ) -> Option<(Arc<dyn TerminalSourceProvider>, TerminalSourceInfo)> {
        for provider in &self.0.providers {
            if let Some(info) = provider.describe(source_id) {
                return Some((provider.clone(), info));
            }
        }
        None
    }

    /// `term.open`: joins `source_id`'s session if one is already live,
    /// otherwise connects a fresh one. Returns the session, this
    /// connection's own subscriber id (for a later `term.close`), and the
    /// receiver it should read `0xA7` output frame payloads from.
    pub async fn open(
        &self,
        source_id: &str,
        settings: OpenSettings,
    ) -> Result<(Arc<Session>, u64, mpsc::Receiver<Vec<u8>>), TerminalError> {
        {
            let sessions = self.0.by_source.lock().await;
            if let Some(session) = sessions.get(source_id) {
                if session.current_state().await != TerminalState::Closed {
                    // A port has one frame format. Joining at another one used
                    // to succeed silently on the old one: an agent asking for
                    // 115200 sent at the 9600 a browser tab had opened, and was
                    // never told (2026-10-09).
                    if let (Some(open), Some(requested)) = (session.serial, settings.serial) {
                        if open != requested {
                            return Err(TerminalError::SettingsConflict {
                                port: session.label.clone(),
                                open: frame_format(&open),
                                requested: frame_format(&requested),
                            });
                        }
                    }
                    let (sub_id, rx) = session.subscribe().await;
                    return Ok((session.clone(), sub_id, rx));
                }
            }
        }

        let (provider, info) = self
            .find(source_id)
            .ok_or_else(|| TerminalError::UnknownSource(source_id.to_string()))?;
        if !info.available {
            return Err(TerminalError::Unavailable(
                source_id.to_string(),
                info.unavailable_reason
                    .unwrap_or_else(|| "unavailable".into()),
            ));
        }
        if provider.kind() == TerminalKind::Serial && settings.serial.is_none() {
            return Err(TerminalError::SettingsRequired(source_id.to_string()));
        }
        if provider.kind() == TerminalKind::Decode && settings.decode.is_none() {
            return Err(TerminalError::SettingsRequired(source_id.to_string()));
        }

        let serial = settings.serial;
        let connected = provider.connect(source_id, settings).await?;
        let session_id = self.0.next_session_id.fetch_add(1, Ordering::Relaxed);
        let (write_tx, write_rx) = mpsc::channel(SUBSCRIBER_QUEUE_CAPACITY);
        let mut scrollback = VecDeque::with_capacity(connected.initial_backlog.len());
        scrollback.extend(connected.initial_backlog);
        let session = Arc::new(Session {
            id: session_id,
            source_id: source_id.to_string(),
            kind: provider.kind(),
            label: connected.label,
            serial,
            state: AsyncMutex::new(TerminalState::Open),
            scrollback: AsyncMutex::new(scrollback),
            subscribers: AsyncMutex::new(HashMap::new()),
            next_subscriber_id: AtomicU64::new(0),
            write_tx,
            shutdown: Arc::new(Notify::new()),
        });

        let (sub_id, rx) = session.subscribe().await;
        self.0
            .by_source
            .lock()
            .await
            .insert(source_id.to_string(), session.clone());

        let manager = self.clone();
        let task_session = session.clone();
        let persistent = provider.persistent();
        let event_bus = self.0.event_bus.clone();
        tokio::spawn(async move {
            run_session(
                task_session.clone(),
                connected.connection,
                write_rx,
                event_bus,
            )
            .await;
            if !persistent {
                manager
                    .0
                    .by_source
                    .lock()
                    .await
                    .remove(&task_session.source_id);
            }
        });

        Ok((session, sub_id, rx))
    }

    async fn session_by_id(&self, session_id: u16) -> Option<Arc<Session>> {
        self.0
            .by_source
            .lock()
            .await
            .values()
            .find(|s| s.id == session_id)
            .cloned()
    }

    /// `term.write`. `data` is the already-decoded byte payload (dispatch.rs
    /// resolves `data`/`data_b64` before calling this).
    pub async fn write(&self, session_id: u16, data: Vec<u8>) -> Result<(), TerminalError> {
        let session = self
            .session_by_id(session_id)
            .await
            .ok_or(TerminalError::UnknownSession(session_id))?;
        if session.kind == TerminalKind::Log || session.kind == TerminalKind::Decode {
            return Err(TerminalError::ReadOnly(session_id));
        }
        session
            .write_tx
            .send(data)
            .await
            .map_err(|_| TerminalError::Io("session's connection has already closed".into()))
    }

    /// `term.close`: leaves `session_id` for one connection. If that was the
    /// last connection and the source isn't persistent (a `log` session
    /// stays up for the server's life), the underlying connection is torn
    /// down — closing a serial port — and the session forgotten.
    pub async fn close(&self, session_id: u16, subscriber_id: u64) -> Result<(), TerminalError> {
        let session = self
            .session_by_id(session_id)
            .await
            .ok_or(TerminalError::UnknownSession(session_id))?;
        let last = session.unsubscribe(subscriber_id).await;
        if last {
            session.shutdown.notify_one();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::time::{timeout, Duration};

    /// An in-memory source for exercising the manager without any real I/O:
    /// `connect` hands back a channel-backed [`SourceConnection`] the test
    /// drives directly by pushing [`SourceEvent`]s and reading writes back.
    struct MemorySource {
        info: TerminalSourceInfo,
        persistent: bool,
        events_tx: StdMutex<Option<mpsc::Sender<SourceEvent>>>,
        writes_tx: mpsc::Sender<Vec<u8>>,
    }

    struct MemoryConnection {
        events_rx: mpsc::Receiver<SourceEvent>,
        writes_tx: mpsc::Sender<Vec<u8>>,
    }

    #[async_trait]
    impl SourceConnection for MemoryConnection {
        async fn recv(&mut self) -> SourceEvent {
            match self.events_rx.recv().await {
                Some(event) => event,
                None => SourceEvent::Closed(Some("test source dropped".into())),
            }
        }
        async fn write(&mut self, data: &[u8]) -> Result<(), TerminalError> {
            self.writes_tx
                .send(data.to_vec())
                .await
                .map_err(|_| TerminalError::Io("closed".into()))
        }
    }

    #[async_trait]
    impl TerminalSourceProvider for MemorySource {
        fn kind(&self) -> TerminalKind {
            self.info.kind
        }
        fn list(&self) -> Vec<TerminalSourceInfo> {
            vec![self.info.clone()]
        }
        fn persistent(&self) -> bool {
            self.persistent
        }
        async fn connect(
            &self,
            _source_id: &str,
            _settings: OpenSettings,
        ) -> Result<Connected, TerminalError> {
            let (events_tx, events_rx) = mpsc::channel(16);
            *self.events_tx.lock().unwrap() = Some(events_tx);
            Ok(Connected {
                connection: Box::new(MemoryConnection {
                    events_rx,
                    writes_tx: self.writes_tx.clone(),
                }),
                initial_backlog: vec![],
                label: self.info.label.clone(),
            })
        }
    }

    fn memory_source(
        id: &str,
        kind: TerminalKind,
        persistent: bool,
    ) -> (Arc<MemorySource>, mpsc::Receiver<Vec<u8>>) {
        let (writes_tx, writes_rx) = mpsc::channel(16);
        let source = Arc::new(MemorySource {
            info: TerminalSourceInfo {
                source: id.to_string(),
                kind,
                label: id.to_string(),
                device: None,
                available: true,
                unavailable_reason: None,
            },
            persistent,
            events_tx: StdMutex::new(None),
            writes_tx,
        });
        (source, writes_rx)
    }

    async fn recv_timeout(rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
        timeout(Duration::from_secs(2), rx.recv()).await.unwrap()
    }

    #[tokio::test]
    async fn open_write_and_receive_output_round_trips() {
        let (source, mut writes_rx) = memory_source("mem:1", TerminalKind::Serial, false);
        let manager = SessionManager::new(vec![source.clone()], EventBus::default());

        let (session, _sub_id, mut out_rx) = manager
            .open(
                "mem:1",
                OpenSettings::serial(SerialSettings {
                    baud: 9600,
                    data_bits: 8,
                    parity: awic_proto::Parity::None,
                    stop_bits: awic_proto::StopBits::One,
                }),
            )
            .await
            .unwrap();

        let events_tx = source.events_tx.lock().unwrap().clone().unwrap();
        events_tx
            .send(SourceEvent::Data(b"hello\n".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_timeout(&mut out_rx).await.unwrap(), b"hello\n");

        manager
            .write(session.id, b"input\n".to_vec())
            .await
            .unwrap();
        assert_eq!(writes_rx.recv().await.unwrap(), b"input\n");
    }

    #[tokio::test]
    async fn a_second_open_joins_the_same_session_and_replays_scrollback() {
        let (source, _writes_rx) = memory_source("mem:2", TerminalKind::Serial, false);
        let manager = SessionManager::new(vec![source.clone()], EventBus::default());
        let settings = SerialSettings {
            baud: 9600,
            data_bits: 8,
            parity: awic_proto::Parity::None,
            stop_bits: awic_proto::StopBits::One,
        };

        let (session_a, _sub_a, mut out_a) = manager
            .open("mem:2", OpenSettings::serial(settings))
            .await
            .unwrap();
        let events_tx = source.events_tx.lock().unwrap().clone().unwrap();
        events_tx
            .send(SourceEvent::Data(b"before join".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_timeout(&mut out_a).await.unwrap(), b"before join");

        let (session_b, _sub_b, mut out_b) = manager
            .open("mem:2", OpenSettings::serial(settings))
            .await
            .unwrap();
        assert_eq!(
            session_a.id, session_b.id,
            "joining must reuse the session id"
        );
        // The joiner immediately sees the scrollback accumulated so far.
        assert_eq!(recv_timeout(&mut out_b).await.unwrap(), b"before join");
    }

    /// A port has one frame format: joining at another one is refused and
    /// names both, instead of quietly sending at the first one's (an agent
    /// asking for 115200 wrote at a browser tab's 9600, 2026-10-09).
    #[tokio::test]
    async fn joining_at_other_settings_is_refused_naming_both() {
        let (source, _writes_rx) = memory_source("mem:9", TerminalKind::Serial, false);
        let manager = SessionManager::new(vec![source.clone()], EventBus::default());
        let at = |baud| SerialSettings {
            baud,
            data_bits: 8,
            parity: awic_proto::Parity::None,
            stop_bits: awic_proto::StopBits::One,
        };
        let (first, _, _rx) = manager
            .open("mem:9", OpenSettings::serial(at(9600)))
            .await
            .unwrap();
        assert_eq!(first.serial, Some(at(9600)));

        let err = manager
            .open("mem:9", OpenSettings::serial(at(115_200)))
            .await
            .err()
            .expect("a join at 115200 onto 9600 must be refused");
        assert!(
            matches!(err, TerminalError::SettingsConflict { .. }),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("9600 8N1") && msg.contains("115200 8N1"),
            "{msg}"
        );

        // The same settings, or none at all, still join.
        let (same, _, _rx2) = manager
            .open("mem:9", OpenSettings::serial(at(9600)))
            .await
            .unwrap();
        assert_eq!(same.id, first.id);
        let (bare, _, _rx3) = manager
            .open("mem:9", OpenSettings::default())
            .await
            .unwrap();
        assert_eq!(bare.id, first.id);
    }

    #[tokio::test]
    async fn closing_the_last_subscriber_of_a_non_persistent_session_tears_it_down() {
        let (source, _writes_rx) = memory_source("mem:3", TerminalKind::Serial, false);
        let manager = SessionManager::new(vec![source.clone()], EventBus::default());
        let settings = SerialSettings {
            baud: 9600,
            data_bits: 8,
            parity: awic_proto::Parity::None,
            stop_bits: awic_proto::StopBits::One,
        };

        let (session, sub_id, _out) = manager
            .open("mem:3", OpenSettings::serial(settings))
            .await
            .unwrap();
        manager.close(session.id, sub_id).await.unwrap();

        // Give the spawned task a beat to observe the shutdown notification
        // and remove the session.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            manager.session_by_id(session.id).await.is_none(),
            "a non-persistent session must be forgotten once its last subscriber leaves"
        );
    }

    #[tokio::test]
    async fn log_sessions_are_read_only() {
        let (source, _writes_rx) = memory_source("log", TerminalKind::Log, true);
        let manager = SessionManager::new(vec![source], EventBus::default());
        let (session, _sub, _out) = manager.open("log", OpenSettings::default()).await.unwrap();
        let err = manager
            .write(session.id, b"nope".to_vec())
            .await
            .unwrap_err();
        assert_eq!(err, TerminalError::ReadOnly(session.id));
    }

    #[tokio::test]
    async fn opening_serial_without_settings_is_refused() {
        let (source, _writes_rx) = memory_source("mem:4", TerminalKind::Serial, false);
        let manager = SessionManager::new(vec![source], EventBus::default());
        let Err(err) = manager.open("mem:4", OpenSettings::default()).await else {
            panic!("expected an error");
        };
        assert_eq!(err, TerminalError::SettingsRequired("mem:4".to_string()));
    }

    #[tokio::test]
    async fn a_dropped_chunk_is_marked_on_the_next_delivered_one() {
        // A subscriber queue of capacity 1 that never drains: the second and
        // third chunks must be dropped-and-counted, and the marker must show
        // up on whichever chunk finally gets through.
        let (source, _writes_rx) = memory_source("mem:5", TerminalKind::Serial, false);
        let manager = SessionManager::new(vec![source.clone()], EventBus::default());
        let settings = SerialSettings {
            baud: 9600,
            data_bits: 8,
            parity: awic_proto::Parity::None,
            stop_bits: awic_proto::StopBits::One,
        };
        let (_session, _sub, mut out_rx) = manager
            .open("mem:5", OpenSettings::serial(settings))
            .await
            .unwrap();
        let events_tx = source.events_tx.lock().unwrap().clone().unwrap();

        // Fill the subscriber's queue without draining it — everything past
        // capacity is dropped-and-counted, not delivered.
        for i in 0..(SUBSCRIBER_QUEUE_CAPACITY + 5) {
            events_tx
                .send(SourceEvent::Data(format!("{i}\n").into_bytes()))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Free up room, then send one more chunk: this is "the next
        // delivered frame" the drop-and-count policy marks (SPEC §3.5) —
        // it can only carry the marker once there's actually room for it.
        out_rx.recv().await.unwrap();
        out_rx.recv().await.unwrap();
        events_tx
            .send(SourceEvent::Data(b"after the gap\n".to_vec()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut saw_marker = false;
        while let Ok(chunk) = out_rx.try_recv() {
            if chunk.starts_with(b"[...") {
                saw_marker = true;
            }
        }
        assert!(saw_marker, "an overrun subscriber must see a drop marker");
    }
}
