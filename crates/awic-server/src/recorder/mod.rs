//! The recorder (SPECIFICATION.md §2.8, §2.9, §3.4): server-owned,
//! continuous, compressed, searchable recordings built on `awic-record`
//! (storage) and `awic-pattern` (search/wait). This module owns everything
//! `dispatch.rs`'s `rec.*` arms delegate to; `state.rs`/`ws.rs`/`main.rs`
//! carry only the minimal hooks (a field on `AppState`, a connection id, a
//! new `WsOut` variant), so shared files take only small, additive edits
//! (DEVELOPMENT.md §2).
//!
//! One [`RecordingEntry`] per recording, holding an `awic_record::Recording`
//! behind a `std::sync::RwLock`: ingest ([`ingest`]) appends under a short
//! write lock (off the async executor — DEVELOPMENT.md risk R11); views ([`views`])
//! and search ([`search`]) take read locks. `awic-record`/`awic-pattern` are
//! pure, no-tokio crates (DEVELOPMENT.md §2); this module is where their
//! data meets the server's tasks and the wire protocol.

pub mod decode;
pub mod decode_feed;
pub mod ingest;
pub mod search;
pub mod snapshot;
pub mod survey;
pub mod views;
pub mod wait;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Instant;

use awic_core::{ChannelCalibration, CoreError, DeviceId, EventBus};
use awic_proto::{
    Event, RecChannel as WireRecChannel, RecKind as WireRecKind,
    RecordingState as WireRecordingState, RecordingSummary, PROTOCOL_VERSION,
};
use awic_record::{Recording, RecordingKind, RecordingSpec};

pub use views::ConnViews;
pub use wait::WaitOutcome;

use crate::state::SharedInstrument;

/// What kind of data a recording holds, plus the storage-shape parameters
/// `awic_record::RecordingKind` needs — this module's own copy so callers
/// here don't have to match on `awic_record::RecordingKind` (which is
/// `Copy` but lives in a crate that knows nothing of the wire protocol) at
/// every use site.
#[derive(Debug, Clone, Copy)]
pub enum RecordingKindLocal {
    Digital,
    Analog { channel_count: u8, sample_bits: u8 },
}

impl RecordingKindLocal {
    fn to_record_kind(self) -> RecordingKind {
        match self {
            RecordingKindLocal::Digital => RecordingKind::Digital,
            RecordingKindLocal::Analog {
                channel_count,
                sample_bits,
            } => RecordingKind::Analog {
                channel_count,
                sample_bits,
            },
        }
    }

    fn wire_kind(self) -> WireRecKind {
        match self {
            RecordingKindLocal::Digital => WireRecKind::Digital,
            RecordingKindLocal::Analog { .. } => WireRecKind::Analog,
        }
    }
}

/// A recording's lifecycle (SPEC §3.4's `rec.state`).
#[derive(Debug, Clone)]
enum Lifecycle {
    Recording,
    Stopped,
    Error(String),
}

/// One channel's identity within a recording: hardware channel number,
/// pattern/UI name (`D0`..`D7`, `CH1`, `CH2` — SPEC §2.9.1), and calibration.
#[derive(Debug, Clone)]
pub struct RecChannel {
    pub channel: u8,
    pub name: String,
    pub calibration: ChannelCalibration,
}

/// Names channels the way `awic-pattern` and the graph module do (SPEC
/// §2.9.1): `D<n>` for digital bits, `CH<n+1>` (1-based) for analog.
fn channel_name(kind: RecordingKindLocal, channel: u8) -> String {
    match kind {
        RecordingKindLocal::Digital => format!("D{channel}"),
        RecordingKindLocal::Analog { .. } => format!("CH{}", channel + 1),
    }
}

/// One recording (SPEC §2.8): storage plus the bookkeeping the server needs
/// that `awic-record` itself has no business knowing (device identity,
/// lifecycle, waiters). `recording` is a plain `std::sync::RwLock`, not a
/// tokio one: every critical section here is synchronous CPU work (append,
/// query), so a blocking lock held across no `.await` is both correct and
/// cheaper than an async one.
pub struct RecordingEntry {
    pub id: u16,
    pub device: String,
    pub device_id: DeviceId,
    pub rate_hz: u64,
    pub kind: RecordingKindLocal,
    pub channels: Vec<RecChannel>,
    pub recording: StdRwLock<Recording>,
    lifecycle: StdMutex<Lifecycle>,
    pub instrument: SharedInstrument,
    /// Set by `rec.stop`/`rec.delete`'s stop path *before* calling
    /// `Instrument::stop()`, so the ingest task (`ingest.rs`) can tell a
    /// requested stop from the device disappearing or erroring on its own —
    /// both end the same channel, so this flag is the only way to tell them
    /// apart, exactly as `crate::dispatch::run_acquisition_stream` does for
    /// finite acquisitions.
    stop_requested: AtomicBool,
    waiters: StdMutex<Vec<Arc<wait::Waiter>>>,
    decode_listeners: StdMutex<Vec<Arc<decode_feed::DecodeListener>>>,
    started_at: Instant,
    /// Spans the device was being reset in after a stall, filled with the
    /// last level (B-25); see `ingest.rs`.
    gaps: StdMutex<Vec<awic_proto::RecGap>>,
    /// How often this recording has resumed after a stall.
    resumes: AtomicU32,
}

impl RecordingEntry {
    pub fn state(&self) -> WireRecordingState {
        match &*self.lifecycle.lock().unwrap() {
            Lifecycle::Recording => WireRecordingState::Recording,
            Lifecycle::Stopped => WireRecordingState::Stopped,
            Lifecycle::Error(_) => WireRecordingState::Error,
        }
    }

    pub fn is_recording(&self) -> bool {
        matches!(&*self.lifecycle.lock().unwrap(), Lifecycle::Recording)
    }

    /// `None` while recording; otherwise when the recording ended and why,
    /// for results over it (`rec.search`, `rec.decode`, `survey`). An agent
    /// once searched a recording that had died 40 s earlier, got an empty
    /// list, and took it for "nothing happened" (live test B-24).
    pub fn not_live_note(&self) -> Option<String> {
        let ended_at = self.read_rec().head_sample() as f64 / self.rate_hz.max(1) as f64;
        match &*self.lifecycle.lock().unwrap() {
            Lifecycle::Recording => None,
            Lifecycle::Stopped => Some(format!(
                "recording {} is not live: it was stopped at {ended_at:.3} s; nothing after \
                 that was recorded",
                self.id
            )),
            Lifecycle::Error(msg) => Some(format!(
                "recording {} is not live: it ended with an error at {ended_at:.3} s ({msg}); \
                 nothing after that was recorded — start a new recording to see what \
                 happened since",
                self.id
            )),
        }
    }

    /// What a result over `[start, end)` must say besides its data: that the
    /// recording is no longer live (see [`Self::not_live_note`]), and any gap
    /// in that range where the device was being reset (B-25) — the span holds
    /// the last level repeated, not what the line did.
    pub fn range_note(&self, start: u64, end: u64) -> Option<String> {
        let rate = self.rate_hz.max(1) as f64;
        let gaps: Vec<String> = self
            .gaps
            .lock()
            .unwrap()
            .iter()
            .filter(|g| g.start_sample < end && g.end_sample > start)
            .map(|g| {
                format!(
                    "{:.3}-{:.3} s",
                    g.start_sample as f64 / rate,
                    g.end_sample as f64 / rate
                )
            })
            .collect();
        let gap_note = (!gaps.is_empty()).then(|| {
            format!(
                "recording {} has no data in {}: the device stalled and was reset, and the \
                 recording resumed after it; those spans hold the last level before them, \
                 repeated, so nothing in them was seen on the line",
                self.id,
                gaps.join(", ")
            )
        });
        match (self.not_live_note(), gap_note) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        }
    }

    pub(crate) fn add_gap(&self, start_sample: u64, end_sample: u64) {
        self.gaps.lock().unwrap().push(awic_proto::RecGap {
            start_sample,
            end_sample,
        });
    }

    pub(crate) fn resumes(&self) -> u32 {
        self.resumes.load(Ordering::SeqCst)
    }

    pub(crate) fn count_resume(&self) {
        self.resumes.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn stop_was_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    fn error_message(&self) -> Option<String> {
        match &*self.lifecycle.lock().unwrap() {
            Lifecycle::Error(msg) => Some(msg.clone()),
            _ => None,
        }
    }

    fn mark_stopped(&self) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if matches!(*lifecycle, Lifecycle::Recording) {
            *lifecycle = Lifecycle::Stopped;
        }
    }

    fn mark_error(&self, msg: String) {
        *self.lifecycle.lock().unwrap() = Lifecycle::Error(msg);
    }

    /// Appends one chunk under a short write lock (SPEC §2.8, ingest path).
    ///
    /// A panic inside `awic-record` is caught here, *inside* the guard's
    /// scope, and becomes this recording's error: unwinding through the
    /// guard would poison the lock, and every other recording's ingest
    /// thread reads this one's lock when it enforces the shared memory
    /// budget — one bug in one recording used to wedge the whole recorder
    /// (live test, 2026-09-15).
    fn append(&self, data: &[u8]) -> Result<(), String> {
        let mut rec = self.write_rec();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match self.kind {
            RecordingKindLocal::Digital => rec.append_packed(data),
            RecordingKindLocal::Analog { .. } => rec.append_analog(data),
        }));
        match result {
            Ok(appended) => appended.map_err(|e| e.to_string()),
            Err(panic) => Err(format!(
                "internal recorder error, recording halted: {}",
                panic_message(&panic)
            )),
        }
    }

    /// The recording, for reading. A poisoned lock is recovered rather than
    /// propagated: the panic that poisoned it has already been reported,
    /// and a query panicking in turn would only spread it.
    pub(crate) fn read_rec(&self) -> std::sync::RwLockReadGuard<'_, Recording> {
        self.recording
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The recording, for writing — see [`Self::read_rec`] on poisoning.
    pub(crate) fn write_rec(&self) -> std::sync::RwLockWriteGuard<'_, Recording> {
        self.recording
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Bytes this recording would cost stored packed/raw, for `rec.list`'s
    /// compression ratio (SPEC §2.8.4).
    fn ratio(&self) -> f64 {
        let rec = self.read_rec();
        let used = rec.bytes();
        if used == 0 {
            return 1.0;
        }
        let raw = rec.raw_bytes(rec.first_sample(), rec.head_sample());
        raw as f64 / used as f64
    }

    fn summary(&self) -> RecordingSummary {
        let rec = self.read_rec();
        RecordingSummary {
            rec: self.id,
            device: self.device.clone(),
            kind: self.kind.wire_kind(),
            rate_hz: self.rate_hz,
            channels: self
                .channels
                .iter()
                .map(|c| WireRecChannel {
                    channel: c.channel,
                    name: c.name.clone(),
                    calibration: c.calibration.clone(),
                })
                .collect(),
            first_sample: rec.first_sample(),
            head_sample: rec.head_sample(),
            bytes: rec.bytes() as u64,
            ratio: {
                drop(rec);
                self.ratio()
            },
            state: self.state(),
            msg: self.error_message(),
            gaps: self.gaps.lock().unwrap().clone(),
        }
    }

    /// `awic-pattern`'s view of this recording's channels, for
    /// `awic_pattern::parse` (SPEC §2.9.1).
    fn pattern_channels(&self) -> Vec<awic_pattern::ChannelInfo> {
        self.channels
            .iter()
            .map(|c| awic_pattern::ChannelInfo {
                name: c.name.clone(),
                kind: match self.kind {
                    RecordingKindLocal::Digital => {
                        awic_pattern::ChannelKind::Digital { bit: c.channel }
                    }
                    RecordingKindLocal::Analog { .. } => awic_pattern::ChannelKind::Analog {
                        index: c.channel,
                        scale: c.calibration.scale,
                        offset: c.calibration.offset,
                    },
                },
            })
            .collect()
    }

    fn pattern_recording_info(&self) -> awic_pattern::RecordingInfo {
        let (sample_bits, channel_count) = match self.kind {
            RecordingKindLocal::Digital => (8, 0),
            RecordingKindLocal::Analog {
                channel_count,
                sample_bits,
            } => (sample_bits, channel_count),
        };
        awic_pattern::RecordingInfo {
            rate_hz: self.rate_hz as f64,
            sample_bits,
            channel_count,
        }
    }

    /// Registers a `decode` terminal session's listener on this recording's
    /// append path (`terminal::decode_source`, cross-module — hence `pub`
    /// rather than the field access `wait.rs`'s sibling module uses
    /// directly). Not registered while the recording has already ended: the
    /// caller should treat that as "nothing more will ever arrive" up front.
    pub fn register_decode_listener(&self, listener: Arc<decode_feed::DecodeListener>) {
        self.decode_listeners.lock().unwrap().push(listener);
    }

    /// Parses `text` against this recording's channels (SPEC §2.9.1).
    fn parse_pattern(&self, text: &str) -> Result<awic_pattern::Pattern, RecOpError> {
        awic_pattern::parse(
            text,
            &self.pattern_channels(),
            self.pattern_recording_info(),
        )
        .map_err(|err| RecOpError::new("invalid_pattern", err.to_string()))
    }
}

/// The stable `{code, msg}` shape every `rec.*` failure boils down to
/// (SPECIFICATION.md §3.1), mirroring `crate::error::UsbOpError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecOpError {
    pub code: String,
    pub msg: String,
}

impl RecOpError {
    pub fn new(code: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            msg: msg.into(),
        }
    }

    pub fn not_found(rec: u16) -> Self {
        Self::new("rec_not_found", format!("no recording {rec}"))
    }
}

impl From<CoreError> for RecOpError {
    fn from(err: CoreError) -> Self {
        Self::new(crate::error::core_error_code(&err), err.to_string())
    }
}

impl From<awic_record::RecordError> for RecOpError {
    fn from(err: awic_record::RecordError) -> Self {
        use awic_record::RecordError;
        let code = match &err {
            RecordError::Evicted { .. } => "range_evicted",
            RecordError::EmptyRange { .. } => "empty_range",
            RecordError::ExtractTooLarge { .. } => "extract_too_large",
            RecordError::MisalignedAppend { .. } | RecordError::WrongKind { .. } => {
                "internal_error"
            }
        };
        Self::new(code, err.to_string())
    }
}

/// One connection's live state (`new_connection`/`close_connection`).
struct Registered {
    views: Arc<ConnViews>,
}

struct Inner {
    next_id: u16,
    recordings: HashMap<u16, Arc<RecordingEntry>>,
    /// One recording per device (SPEC §2.8): `device -> rec id`.
    device_to_rec: HashMap<String, u16>,
    /// Devices a `rec.start` has claimed but not yet inserted a recording for.
    ///
    /// `start()` opens the device, configures it and starts it streaming -
    /// three awaits of USB I/O, sometimes a firmware re-upload - before
    /// `insert` makes `is_recording` true. Two `rec.start`s ten milliseconds
    /// apart therefore both passed the "one recording per device" check, the
    /// second reconfigured the instrument under the first, and the first ended
    /// in `error` holding nothing (B-59). The claim closes that window.
    starting: HashSet<String>,
    connections: HashMap<u64, Registered>,
}

/// Owns every recording for the life of the server (SPEC §2.8: "server-owned,
/// not per-connection"). Held by `AppState` behind an `Arc`, exactly like
/// `CaptureStore`.
pub struct RecorderManager {
    inner: StdMutex<Inner>,
    budget_bytes: u64,
    next_conn_id: AtomicU64,
    event_bus: EventBus,
    capture_store: Arc<awic_core::CaptureStore>,
}

impl RecorderManager {
    pub fn new(
        memory_mb: u64,
        event_bus: EventBus,
        capture_store: Arc<awic_core::CaptureStore>,
    ) -> Self {
        Self {
            inner: StdMutex::new(Inner {
                next_id: 1,
                recordings: HashMap::new(),
                device_to_rec: HashMap::new(),
                starting: HashSet::new(),
                connections: HashMap::new(),
            }),
            budget_bytes: memory_mb * 1024 * 1024,
            next_conn_id: AtomicU64::new(1),
            event_bus,
            capture_store,
        }
    }

    pub fn is_recording(&self, device: &str) -> bool {
        self.live_recording_on(device).is_some()
    }

    /// Claims `device` for a `rec.start` that is about to set it up, or `None`
    /// when something already holds it - a live recording, or another
    /// `rec.start` still in its own setup.
    ///
    /// The claim is released when the returned guard drops, so every error path
    /// out of `start()` frees it without having to remember to; `commit()`
    /// hands it over to the recording that got registered.
    pub fn claim_device(self: &Arc<Self>, device: &str) -> Option<DeviceClaim> {
        let mut inner = self.inner.lock().unwrap();
        let held = inner
            .device_to_rec
            .get(device)
            .and_then(|id| inner.recordings.get(id))
            .is_some_and(|entry| entry.is_recording());
        if held || !inner.starting.insert(device.to_string()) {
            return None;
        }
        Some(DeviceClaim {
            manager: Arc::clone(self),
            device: device.to_string(),
            committed: false,
        })
    }

    fn release_claim(&self, device: &str) {
        self.inner.lock().unwrap().starting.remove(device);
    }

    /// Whether a `rec.start` is setting this device up right now, for the
    /// `device_busy` message - "a moment ago; retry" is the wrong thing to say
    /// about a start that is still in progress.
    fn is_starting(&self, device: &str) -> bool {
        self.inner.lock().unwrap().starting.contains(device)
    }

    /// The id of the live recording holding `device`, if there is one.
    pub fn live_recording_on(&self, device: &str) -> Option<u16> {
        let inner = self.inner.lock().unwrap();
        let id = *inner.device_to_rec.get(device)?;
        inner
            .recordings
            .get(&id)
            .filter(|entry| entry.is_recording())
            .map(|_| id)
    }

    /// "device X is recording" with the recording's id and age in it
    /// (live test B-31).
    ///
    /// The bare message named neither, which made it useless in the one
    /// situation it actually comes up in: a browser tab closed mid-recording
    /// leaves the device held, and the person who reopens the page has no way
    /// to reach a recording they cannot see. A caller — person or agent — can
    /// act on this one in a single step.
    pub fn device_busy_message(&self, device: &str) -> String {
        match self.live_recording_on(device) {
            Some(rec) => {
                let age = self
                    .get(rec)
                    .map(|entry| entry.started_at.elapsed())
                    .unwrap_or_default();
                format!(
                    "device {device:?} is held by recording {rec}, live for {:.0} s \
                     (SPECIFICATION.md §2.8: a recording outlives the connection that started \
                     it). Stop it with rec.stop rec={rec} — over MCP that is \
                     stop_recording, over SCPI REC:STOP. rec.list shows every recording.",
                    age.as_secs_f64()
                )
            }
            None if self.is_starting(device) => format!(
                "device {device:?} is being set up by another rec.start that has not finished \
                 yet; retry once it answers, or call rec.list to see what it became"
            ),
            // Raced: it ended between the check and this call.
            None => format!("device {device:?} was recording a moment ago; retry"),
        }
    }

    pub fn get(&self, rec: u16) -> Option<Arc<RecordingEntry>> {
        self.inner.lock().unwrap().recordings.get(&rec).cloned()
    }

    fn insert(
        &self,
        device: String,
        device_id: DeviceId,
        rate_hz: u64,
        kind: RecordingKindLocal,
        channels: Vec<RecChannel>,
        instrument: SharedInstrument,
    ) -> Arc<RecordingEntry> {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1).max(1);
        let spec = RecordingSpec {
            kind: kind.to_record_kind(),
            rate_hz,
        };
        let entry = Arc::new(RecordingEntry {
            id,
            device: device.clone(),
            device_id,
            rate_hz,
            kind,
            channels,
            recording: StdRwLock::new(Recording::new(spec)),
            lifecycle: StdMutex::new(Lifecycle::Recording),
            instrument,
            stop_requested: AtomicBool::new(false),
            waiters: StdMutex::new(Vec::new()),
            decode_listeners: StdMutex::new(Vec::new()),
            started_at: Instant::now(),
            gaps: StdMutex::new(Vec::new()),
            resumes: AtomicU32::new(0),
        });
        inner.recordings.insert(id, entry.clone());
        inner.device_to_rec.insert(device, id);
        entry
    }

    pub fn list(&self) -> awic_proto::RecListResult {
        let inner = self.inner.lock().unwrap();
        let mut recordings: Vec<_> = inner.recordings.values().map(|e| e.summary()).collect();
        recordings.sort_by_key(|r| r.rec);
        let used_bytes: u64 = inner
            .recordings
            .values()
            .map(|e| e.read_rec().bytes() as u64)
            .sum();
        awic_proto::RecListResult {
            recordings,
            budget_mb: self.budget_bytes / (1024 * 1024),
            used_bytes,
        }
    }

    /// Removes a recording from bookkeeping. Called by `rec.delete`; the
    /// caller is responsible for having stopped it first.
    fn remove(&self, rec: u16) -> Option<Arc<RecordingEntry>> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.recordings.remove(&rec)?;
        inner.device_to_rec.retain(|_, id| *id != rec);
        Some(entry)
    }

    /// Evicts the oldest block from whichever recording holds the globally
    /// oldest retained data, repeatedly, until total usage is at or under
    /// budget (SPEC §2.8.4).
    ///
    /// Sample indices aren't comparable across recordings — they run on
    /// different devices with different rates and start times — so "oldest"
    /// here is approximated as the wall-clock time of each recording's
    /// oldest retained sample (`started_at + first_sample / rate_hz`). This
    /// is a documented implementation choice (SPEC §2.8.4 does not define
    /// cross-recording ordering), reasonable because there are normally at
    /// most a handful of concurrent recordings.
    pub fn evict_to_budget(&self) {
        let mut entries: Vec<Arc<RecordingEntry>> = self
            .inner
            .lock()
            .unwrap()
            .recordings
            .values()
            .cloned()
            .collect();
        entries.sort_by_key(|e| (oldest_retained_instant(e), e.id));

        loop {
            let total: usize = entries.iter().map(|e| e.read_rec().bytes()).sum();
            if total as u64 <= self.budget_bytes || entries.is_empty() {
                return;
            }
            // Oldest-first pass: evict one block from the first entry able
            // to give one up. Re-sort only on success (an eviction can
            // change which entry is now oldest); a pass that frees nothing
            // anywhere means every recording is down to its last block
            // (kept even over budget, same rule as `CaptureStore`), so stop.
            let mut freed_any = false;
            for entry in &entries {
                if entry.write_rec().evict_oldest_block() > 0 {
                    freed_any = true;
                    break;
                }
            }
            if !freed_any {
                return;
            }
            entries.sort_by_key(|e| (oldest_retained_instant(e), e.id));
        }
    }

    pub fn new_connection(&self) -> u64 {
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().unwrap().connections.insert(
            id,
            Registered {
                views: Arc::new(ConnViews::default()),
            },
        );
        id
    }

    /// Ends every view this connection opened (SPEC §3.4: "views ... die
    /// with it"). Dropping the last `Arc<ConnViews>` aborts its follow-view
    /// tasks (see `ConnViews`'s `Drop` impl).
    pub fn close_connection(&self, conn_id: u64) {
        self.inner.lock().unwrap().connections.remove(&conn_id);
    }

    fn conn_views(&self, conn_id: u64) -> Option<Arc<ConnViews>> {
        self.inner
            .lock()
            .unwrap()
            .connections
            .get(&conn_id)
            .map(|r| r.views.clone())
    }

    fn publish_state(&self, entry: &RecordingEntry) {
        self.event_bus.publish(Event::RecState {
            v: PROTOCOL_VERSION,
            rec: entry.id,
            state: entry.state(),
            msg: entry.error_message(),
        });
    }

    /// Stops every active recording (server shutdown, mirroring
    /// `AppState::shutdown`'s treatment of finite acquisitions).
    pub async fn shutdown(&self) {
        let entries: Vec<Arc<RecordingEntry>> = self
            .inner
            .lock()
            .unwrap()
            .recordings
            .values()
            .cloned()
            .collect();
        for entry in entries {
            if entry.is_recording() {
                entry.stop_requested.store(true, Ordering::SeqCst);
                let mut guard = entry.instrument.lock().await;
                let _ = guard.stop().await;
            }
        }
    }
}

/// `awic_record::Run` and `awic_pattern::Run` are field-for-field identical
/// by design (`awic-pattern`'s doc comment: "matches `awic-record`'s `Run`
/// verbatim... so the storage glue (T18) can pass its runs straight
/// through") but are distinct types, so a conversion is still needed at the
/// boundary.
fn to_pattern_runs(runs: &[awic_record::Run]) -> Vec<awic_pattern::Run> {
    runs.iter()
        .map(|r| awic_pattern::Run {
            start: r.start,
            len: r.len,
            value: r.value,
        })
        .collect()
}

/// The text of a caught panic's payload (`panic!` with a literal or a
/// formatted message; anything else is opaque).
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn oldest_retained_instant(entry: &RecordingEntry) -> Instant {
    let first = entry.read_rec().first_sample();
    let age = std::time::Duration::from_secs_f64(first as f64 / entry.rate_hz.max(1) as f64);
    entry.started_at + age
}

/// `rec.start` (SPEC §2.8, §3.4): configure with continuous forced on, then
/// start, exactly like `acq.configure` + `acq.start` but ingesting into a
/// [`RecordingEntry`] instead of forwarding sample frames.
pub async fn start(
    state: &crate::state::AppState,
    device: String,
    mut cfg: awic_core::AcqConfig,
) -> Result<(u16, awic_core::EffectiveAcq), RecOpError> {
    // Held across the setup below, not just checked before it (B-59).
    let claim = state.recorder().claim_device(&device).ok_or_else(|| {
        RecOpError::new("device_busy", state.recorder().device_busy_message(&device))
    })?;
    if state.device_has_stream(&device).await {
        return Err(RecOpError::new(
            "device_busy",
            format!("device {device:?} has an active acq.start stream; call acq.stop first"),
        ));
    }
    let device_id: DeviceId = device
        .parse()
        .map_err(|err: awic_core::DeviceIdParseError| {
            RecOpError::new("invalid_device", err.to_string())
        })?;

    cfg.continuous = true;
    let instrument = state.ensure_open(&device).await.map_err(RecOpError::from)?;
    let (effective, caps) = {
        let mut guard = instrument.lock().await;
        let effective = state
            .configure_announced(&device_id, &mut guard, cfg)
            .await
            .map_err(RecOpError::from)?;
        (effective.clone(), guard.capabilities().to_vec())
    };

    let kind = recording_kind_from_caps(&caps, effective.cfg.channels.len() as u8);
    let channels = effective
        .cfg
        .channels
        .iter()
        .zip(effective.calibration.iter())
        .map(|(&channel, calibration)| RecChannel {
            channel,
            name: channel_name(kind, channel),
            calibration: calibration.clone(),
        })
        .collect();

    let (sink, rx) = awic_core::SampleSink::channel(64);
    {
        let mut guard = instrument.lock().await;
        state
            .start_announced(&device_id, &mut guard, sink)
            .await
            .map_err(RecOpError::from)?;
    }

    let entry = state.recorder().insert(
        device,
        device_id,
        effective.cfg.rate_hz,
        kind,
        channels,
        instrument,
    );
    ingest::spawn(entry.clone(), rx, state.clone());
    state.recorder().publish_state(&entry);
    // The recording holds the device now, so the claim has done its job.
    claim.commit();

    Ok((entry.id, effective))
}

/// A device claimed by a `rec.start` that is still setting it up (B-59).
///
/// Dropping it releases the claim, which is what makes every `?` on the way
/// through [`start`] safe; [`DeviceClaim::commit`] keeps it claimed because a
/// registered recording now holds the device instead.
pub struct DeviceClaim {
    manager: Arc<RecorderManager>,
    device: String,
    committed: bool,
}

impl DeviceClaim {
    fn commit(mut self) {
        self.committed = true;
        self.manager.release_claim(&self.device);
    }
}

impl Drop for DeviceClaim {
    fn drop(&mut self) {
        if !self.committed {
            self.manager.release_claim(&self.device);
        }
    }
}

/// A driver reports exactly one of `Capability::Analog`/`Digital` per
/// instrument (fx2lafw is digital-only, fx2scope analog-only — SPEC
/// §2.5), so the first match determines this recording's storage kind.
fn recording_kind_from_caps(
    caps: &[awic_core::Capability],
    channel_count: u8,
) -> RecordingKindLocal {
    for cap in caps {
        if let awic_core::Capability::Analog { bits, .. } = cap {
            return RecordingKindLocal::Analog {
                channel_count,
                sample_bits: if *bits <= 8 { 8 } else { 16 },
            };
        }
    }
    RecordingKindLocal::Digital
}

/// `rec.stop`: stops the instrument; the ingest task (`ingest.rs`) observes
/// the channel close and marks the recording `stopped`.
pub async fn stop(state: &crate::state::AppState, rec: u16) -> Result<(), RecOpError> {
    let entry = state
        .recorder()
        .get(rec)
        .ok_or_else(|| RecOpError::not_found(rec))?;
    if !entry.is_recording() {
        return Ok(()); // Already stopped/errored: idempotent.
    }
    entry.stop_requested.store(true, Ordering::SeqCst);
    let mut guard = entry.instrument.lock().await;
    guard.stop().await.map_err(RecOpError::from)?;
    drop(guard);
    // The device has stopped streaming, so the recording is stopped now —
    // not when the ingest task gets round to noticing. Otherwise a `rec.stop`
    // followed straight away by `rec.delete` was refused as "still active;
    // call rec.stop first" (B-48). The ingest task's own mark is then a no-op.
    entry.mark_stopped();
    Ok(())
}

/// `rec.delete`: refused while recording (SPEC's own note that stopping is a
/// separate, explicit step) — deleting live data out from under an ingest
/// task would race the writer lock against a task this call cannot join.
pub fn delete(state: &crate::state::AppState, rec: u16) -> Result<(), RecOpError> {
    let entry = state
        .recorder()
        .get(rec)
        .ok_or_else(|| RecOpError::not_found(rec))?;
    if entry.is_recording() {
        return Err(RecOpError::new(
            "recording_active",
            format!("recording {rec} is still active; call rec.stop first"),
        ));
    }
    state.recorder().remove(rec);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> Arc<RecorderManager> {
        Arc::new(RecorderManager::new(
            256,
            EventBus::default(),
            Arc::new(awic_core::CaptureStore::new(16, 16 * 1024 * 1024)),
        ))
    }

    /// B-59: the window between the "one recording per device" check and the
    /// recording being registered spans three awaits of USB I/O, and two
    /// `rec.start`s ten milliseconds apart both got through it on the bench.
    /// The claim is what the second one now runs into.
    #[test]
    fn a_second_claim_on_one_device_is_refused_while_the_first_sets_up() {
        let manager = manager();
        let first = manager
            .claim_device("3-3.4.1.1")
            .expect("nothing holds the device");
        assert!(
            manager.claim_device("3-3.4.1.1").is_none(),
            "the device is being set up; a second start must be refused"
        );
        // Another device is unaffected.
        assert!(manager.claim_device("3-3.4.2").is_some());
        // And the refusal says what is happening rather than "a moment ago".
        let msg = manager.device_busy_message("3-3.4.1.1");
        assert!(msg.contains("being set up"), "{msg}");
        assert!(msg.contains("rec.list"), "must say how to look: {msg}");
        drop(first);
    }

    /// Every error path out of `start()` returns through the guard's `Drop`, so
    /// a failed setup must not leave the device claimed for ever.
    #[test]
    fn dropping_a_claim_frees_the_device() {
        let manager = manager();
        {
            let _claim = manager.claim_device("3-3.4.1.1").expect("claimed");
        }
        assert!(
            manager.claim_device("3-3.4.1.1").is_some(),
            "a dropped claim must release the device"
        );
    }

    /// A committed claim is handed over to the registered recording, so the
    /// `starting` set must not keep the device after a successful start either.
    #[test]
    fn committing_a_claim_frees_the_set_for_the_recording_to_hold() {
        let manager = manager();
        let claim = manager.claim_device("3-3.4.1.1").expect("claimed");
        claim.commit();
        assert!(!manager.is_starting("3-3.4.1.1"));
        assert!(manager.claim_device("3-3.4.1.1").is_some());
    }
}
