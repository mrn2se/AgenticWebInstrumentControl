//! Shared server state: the device registry, event bus, backend, USB
//! controller, and the bookkeeping needed to run acquisitions
//! (SPECIFICATION.md §2.6, §3.1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use awic_proto::{DeviceState, UsbChange};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use awic_core::{
    CaptureStore, Config, CoreError, DeviceId, EffectiveAcq, EventBus, Instrument, Registry,
};

use crate::backend::PlaceholderBackend;
use crate::backpressure::StreamBackpressure;
use crate::recorder::RecorderManager;
use crate::terminal::SessionManager;
use crate::usb::UnimplementedUsbController;

pub use crate::usb::UsbController;
pub use awic_core::Backend;

/// One live instrument, shared between whichever request is currently
/// configuring/starting/stopping it. Locked for the duration of a single
/// `Instrument` call (SPECIFICATION.md §2.6: one owner per device).
pub type SharedInstrument = Arc<AsyncMutex<Box<dyn Instrument>>>;

/// Bookkeeping for one running acquisition stream, keyed by `stream_id` in
/// [`Inner::streams`].
pub struct StreamHandle {
    pub device: String,
    pub instrument: SharedInstrument,
    pub forwarder: JoinHandle<()>,
    pub backpressure: Arc<StreamBackpressure>,
    /// The software trigger this stream is waiting on, if any (B-32).
    pub trigger: Option<Arc<crate::soft_trigger::TriggerWatch>>,
}

struct Inner {
    config: Config,
    registry: Arc<Registry>,
    event_bus: EventBus,
    backend: Arc<dyn Backend>,
    usb: Arc<dyn UsbController>,
    /// Devices that have been opened (and `prepare()`d) at least once,
    /// keyed by their wire-format device id string. Reused across
    /// `acq.configure` calls rather than re-opened every time.
    instruments: AsyncMutex<HashMap<String, SharedInstrument>>,
    /// Serializes opening devices, so two concurrent requests for the same
    /// device cannot both run its firmware upload.
    opening: AsyncMutex<()>,
    /// The most recent `acq.configure` per device — `acq.start` needs
    /// `rate_hz` (and whether a trigger was armed) to file the resulting
    /// capture's metadata, and the wire protocol doesn't repeat the config
    /// on `acq.start` itself (SPEC §3.1's example only carries `device`).
    configs: AsyncMutex<HashMap<String, EffectiveAcq>>,
    capture_store: Arc<CaptureStore>,
    streams: AsyncMutex<HashMap<u16, StreamHandle>>,
    next_stream_id: AtomicU16,
    /// The recorder (SPECIFICATION.md §2.8): server-owned continuous
    /// recordings, independent of the finite `acq.*` streams above.
    recorder: Arc<RecorderManager>,
    /// The terminal module's session manager (SPEC §11.2/§11.2.1). Built
    /// with no source providers by [`AppState::new`]; `main.rs` wires up the
    /// real `log`/`serial` providers via
    /// [`AppState::with_terminal_providers`].
    terminal: SessionManager,
    /// Live MCP sessions (SPEC §8.1), keyed by the `Mcp-Session-Id` minted at
    /// `initialize` — see `crate::mcp::route`'s doc comment for why this
    /// lives on `AppState` rather than as a module-level global.
    mcp_sessions: crate::mcp::route::McpSessions,
    /// What the web UI's MCP tab shows (SPEC §8.5): sessions and the
    /// bounded message log of `/mcp`.
    mcp_monitor: Arc<crate::mcp::monitor::McpMonitor>,
    /// Broadcast so every open `/ws` connection can be told to close its
    /// socket promptly on graceful shutdown, rather than axum waiting
    /// indefinitely for long-lived WebSocket "requests" to finish on their
    /// own (SPECIFICATION.md's graceful-shutdown expectation).
    shutdown_tx: broadcast::Sender<()>,
}

/// The application's shared state. Cheap to clone (an `Arc` underneath),
/// exactly like [`awic_core::EventBus`] and [`awic_core::CaptureStore`].
/// Field access goes through the methods below rather than raw fields, so
/// the instrument/stream maps stay behind the locking rules those methods
/// enforce.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    pub fn new(config: Config, backend: Arc<dyn Backend>, usb: Arc<dyn UsbController>) -> Self {
        Self::with_terminal_providers(config, backend, usb, Vec::new())
    }

    /// Like [`Self::new`], plus the terminal module's source providers
    /// (`main.rs` passes the real `log`/`serial` ones; every other caller —
    /// every test that doesn't exercise `term.*` — keeps using [`Self::new`]
    /// and gets a session manager with nothing to open, which is exactly
    /// what an empty `term.sources` list should look like).
    pub fn with_terminal_providers(
        config: Config,
        backend: Arc<dyn Backend>,
        usb: Arc<dyn UsbController>,
        terminal_providers: Vec<Arc<dyn crate::terminal::TerminalSourceProvider>>,
    ) -> Self {
        let (shutdown_tx, _rx) = broadcast::channel(1);
        let event_bus = EventBus::default();
        let registry = Arc::new(Registry::new(event_bus.clone()));
        let capture_store = Arc::new(CaptureStore::with_defaults());
        let recorder = Arc::new(RecorderManager::new(
            config.recorder.memory_mb,
            event_bus.clone(),
            capture_store.clone(),
        ));
        // The `decode` terminal source (SPEC §11.2) needs only the recorder,
        // never the rest of `AppState` — unlike `scpi`'s `ServerHost`, which
        // dispatches whole requests and so needs the `LateAppState`
        // chicken-and-egg cell (see `scpi::terminal_source`'s doc comment).
        // Registered unconditionally (like `log`/`scpi`, never hardware-gated)
        // so every `AppState`, including `AppState::new`'s bare test path,
        // gets a working `decode` picker entry.
        let mut terminal_providers = terminal_providers;
        terminal_providers.push(Arc::new(
            crate::terminal::decode_source::DecodeSourceProvider::new(recorder.clone()),
        ));
        let terminal = SessionManager::new(terminal_providers, event_bus.clone());
        let mcp_monitor = Arc::new(crate::mcp::monitor::McpMonitor::new(
            &config.mcp,
            event_bus.clone(),
        ));
        Self(Arc::new(Inner {
            config,
            registry,
            event_bus,
            backend,
            usb,
            instruments: AsyncMutex::new(HashMap::new()),
            opening: AsyncMutex::new(()),
            configs: AsyncMutex::new(HashMap::new()),
            capture_store,
            streams: AsyncMutex::new(HashMap::new()),
            next_stream_id: AtomicU16::new(1),
            recorder,
            terminal,
            mcp_sessions: crate::mcp::route::new_sessions(),
            mcp_monitor,
            shutdown_tx,
        }))
    }

    pub fn terminal(&self) -> &SessionManager {
        &self.0.terminal
    }

    pub fn mcp_sessions(&self) -> &crate::mcp::route::McpSessions {
        &self.0.mcp_sessions
    }

    pub fn mcp_monitor(&self) -> &Arc<crate::mcp::monitor::McpMonitor> {
        &self.0.mcp_monitor
    }

    /// Convenience constructor for `main.rs`: no backend, no USB control —
    /// wired in once the real crates land.
    pub fn placeholder(config: Config) -> Self {
        Self::new(
            config,
            Arc::new(PlaceholderBackend),
            Arc::new(UnimplementedUsbController),
        )
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.0.registry
    }

    pub fn event_bus(&self) -> &EventBus {
        &self.0.event_bus
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.0.backend
    }

    pub fn usb(&self) -> &Arc<dyn UsbController> {
        &self.0.usb
    }

    pub fn capture_store(&self) -> &Arc<CaptureStore> {
        &self.0.capture_store
    }

    pub fn recorder(&self) -> &Arc<RecorderManager> {
        &self.0.recorder
    }

    /// The stream id of `device`'s active finite `acq.start` stream, if any
    /// — SCPI's `ACQ:STOP` (`crate::scpi`) has only the device selected by
    /// `INST:SEL`, not the `stream` id `acq.start` handed back over the
    /// WebSocket, so it needs to look the id up rather than carry it.
    pub async fn stream_id_for_device(&self, device: &str) -> Option<u16> {
        self.0
            .streams
            .lock()
            .await
            .iter()
            .find(|(_, handle)| handle.device == device)
            .map(|(&id, _)| id)
    }

    /// Whether `device` has an active finite `acq.start` stream right now —
    /// `rec.start` refuses a device that does, symmetrically with
    /// `acq.configure`/`acq.start` refusing a device that is recording
    /// (SPECIFICATION.md §2.8: "one recording per device").
    pub async fn device_has_stream(&self, device: &str) -> bool {
        self.0
            .streams
            .lock()
            .await
            .values()
            .any(|handle| handle.device == device)
    }

    pub fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.0.shutdown_tx.subscribe()
    }

    /// One-time startup discovery (SPECIFICATION.md §2.2's `Backend::discover`):
    /// opens and prepares every device the backend currently reports, and
    /// records it in the [`Registry`] so an immediate `device.list` sees it.
    /// A device watcher (`awic-usb`, not yet wired) will eventually replace
    /// this with live hot-plug tracking (§4.3); until then this single pass
    /// is what makes the fake/placeholder backend visible at all.
    pub async fn bootstrap(&self) {
        let ids = match self.0.backend.discover().await {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(%err, "backend discovery failed");
                return;
            }
        };
        for id in ids {
            if let Err(err) = self.open(&id).await {
                tracing::warn!(device = %id, %err, "failed to open discovered device");
            }
        }
    }

    /// The instrument for `device`, opening and preparing it if needed.
    pub async fn ensure_open(&self, device: &str) -> awic_core::Result<SharedInstrument> {
        let id: DeviceId = device
            .parse()
            .map_err(|err: awic_core::DeviceIdParseError| CoreError::Config(err.to_string()))?;
        self.open(&id).await
    }

    /// Opens, registers and prepares `id` — or returns it if already open.
    async fn open(&self, id: &DeviceId) -> awic_core::Result<SharedInstrument> {
        let _serial = self.0.opening.lock().await;
        if let Some(existing) = self.0.instruments.lock().await.get(&id.to_string()) {
            return Ok(existing.clone());
        }
        let mut instrument = self.0.backend.open(id).await?;
        // Visible before preparing: a device whose firmware upload fails then
        // shows up in `error` with the reason instead of being silently absent.
        self.0.registry.detected(
            id.clone(),
            instrument.capabilities().to_vec(),
            instrument.label(),
        );
        self.prepare_announced(id, &mut instrument).await?;
        let handle: SharedInstrument = Arc::new(AsyncMutex::new(instrument));
        self.0
            .instruments
            .lock()
            .await
            .insert(id.to_string(), handle.clone());
        Ok(handle)
    }

    /// `prepare()` surfaced as one `flashing` -> `ready` (or `error`)
    /// transition (SPEC §4.5). Every path that prepares a device goes through
    /// here; before, startup prepared devices but left them `detected`.
    async fn prepare_announced(
        &self,
        id: &DeviceId,
        instrument: &mut Box<dyn Instrument>,
    ) -> awic_core::Result<()> {
        self.0.registry.set_state(id, DeviceState::Flashing, None);
        match instrument.prepare().await {
            Ok(()) => {
                self.0.registry.set_state(id, DeviceState::Ready, None);
                Ok(())
            }
            Err(err) => {
                self.0
                    .registry
                    .set_state(id, DeviceState::Error, Some(err.to_string()));
                Err(err)
            }
        }
    }

    /// Runs `configure` on an already-locked instrument, announcing a
    /// device that has to be re-flashed first as `flashing` (SPEC §4.5) for as
    /// long as that takes.
    ///
    /// An FX2 that stalled part-way through its last acquisition needs its
    /// firmware uploaded again, and the driver does that inside `configure`:
    /// a re-upload plus a re-enumeration, two to four seconds during which the
    /// registry said `ready` and `list_instruments` offered a device that was
    /// not there (B-63). `Instrument::pending_reset` is how a driver says so;
    /// one that never re-flashes reports `false` and this is a plain call.
    pub async fn configure_announced(
        &self,
        id: &DeviceId,
        instrument: &mut Box<dyn Instrument>,
        cfg: awic_core::AcqConfig,
    ) -> awic_core::Result<awic_core::EffectiveAcq> {
        if !instrument.pending_reset() {
            return instrument.configure(cfg).await;
        }
        self.0.registry.set_state(id, DeviceState::Flashing, None);
        let result = instrument.configure(cfg).await;
        match &result {
            Ok(_) => self.0.registry.set_state(id, DeviceState::Ready, None),
            Err(err) => self
                .0
                .registry
                .set_state(id, DeviceState::Error, Some(err.to_string())),
        }
        result
    }

    /// [`Self::configure_announced`] for `start`: the fx2lafw driver resets a
    /// device that stalled at the start of the next acquisition, not in
    /// `configure`. Unannounced, the hot-plug handler took that reset's
    /// re-enumeration for a device leaving and arriving: it dropped the
    /// instrument and uploaded firmware a second time, racing the driver's
    /// own upload, so the first start after every stall failed ("usb control
    /// transfer failed", then "device not found") and only a second call
    /// worked (ESP32/ttyUSB bench, 2026-10-08).
    pub async fn start_announced(
        &self,
        id: &DeviceId,
        instrument: &mut Box<dyn Instrument>,
        sink: awic_core::SampleSink,
    ) -> awic_core::Result<awic_core::CaptureId> {
        if !instrument.pending_reset() {
            return instrument.start(sink).await;
        }
        self.0.registry.set_state(id, DeviceState::Flashing, None);
        let result = instrument.start(sink).await;
        match &result {
            Ok(_) => self.0.registry.set_state(id, DeviceState::Ready, None),
            Err(err) => self
                .0
                .registry
                .set_state(id, DeviceState::Error, Some(err.to_string())),
        }
        result
    }

    /// Keeps the instrument registry in step with the bus (SPEC §4.5):
    /// an instrument that appears is opened and prepared, one that vanishes
    /// is dropped. Without this, the tree tracked hot-plug but the instrument
    /// list did not — and after a port power cycle the cached instrument
    /// pointed at a device that had lost its firmware.
    pub async fn handle_usb_change(&self, device: &str, change: UsbChange) {
        let Ok(id) = device.parse::<DeviceId>() else {
            return;
        };
        // A device being prepared re-enumerates by design — that is its own
        // FX2 firmware upload, not a hot-plug. Acting on it would discard the
        // device mid-flash.
        if self.0.registry.state_of(&id) == Some(DeviceState::Flashing) {
            return;
        }
        match change {
            // A lock or unlock changes what may be switched, not what is on
            // the bus: no instrument appears or disappears.
            UsbChange::Protection => {}
            UsbChange::Removed => {
                let was_open = self.0.instruments.lock().await.remove(device).is_some();
                self.0.configs.lock().await.remove(device);
                if self.0.registry.contains(&id) {
                    self.0.registry.remove(&id);
                }
                if was_open {
                    tracing::info!(device, "instrument left the bus");
                }
            }
            UsbChange::Added => {
                let already_ready = self.0.instruments.lock().await.contains_key(device)
                    && self.0.registry.state_of(&id) == Some(DeviceState::Ready);
                if already_ready {
                    return; // A trailing event from a prepare that already finished.
                }
                let claimed = self
                    .0
                    .backend
                    .discover()
                    .await
                    .is_ok_and(|ids| ids.contains(&id));
                if !claimed {
                    return; // Not an instrument: a hub, or some other peripheral.
                }
                // Prepared on its own task so this handler keeps consuming
                // events meanwhile — the device's own re-enumeration events
                // must be seen *while* it is still `flashing`.
                let state = self.clone();
                tokio::spawn(async move {
                    if let Err(err) = state.open(&id).await {
                        tracing::warn!(device = %id, %err, "failed to open hot-plugged instrument");
                    }
                });
            }
        }
    }

    /// The already-open instrument for `device`, without opening it —
    /// `acq.start` requires a prior `acq.configure` rather than opening a
    /// device implicitly.
    pub async fn configured_instrument(&self, device: &str) -> Option<SharedInstrument> {
        self.0.instruments.lock().await.get(device).cloned()
    }

    /// Records what `acq.configure` actually applied — the driver's
    /// [`EffectiveAcq`], never the request — so a later `acq.start` builds
    /// its timebase and calibration from the truth.
    pub async fn set_config(&self, device: &str, effective: EffectiveAcq) {
        self.0
            .configs
            .lock()
            .await
            .insert(device.to_string(), effective);
    }

    pub async fn get_config(&self, device: &str) -> Option<EffectiveAcq> {
        self.0.configs.lock().await.get(device).cloned()
    }

    pub fn next_stream_id(&self) -> u16 {
        self.0.next_stream_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn insert_stream(&self, stream_id: u16, handle: StreamHandle) {
        self.0.streams.lock().await.insert(stream_id, handle);
    }

    /// The software trigger stream `stream_id` is running, if it has one.
    pub async fn trigger_watch(
        &self,
        stream_id: u16,
    ) -> Option<Arc<crate::soft_trigger::TriggerWatch>> {
        self.0
            .streams
            .lock()
            .await
            .get(&stream_id)
            .and_then(|h| h.trigger.clone())
    }

    pub async fn remove_stream(&self, stream_id: u16) -> Option<StreamHandle> {
        self.0.streams.lock().await.remove(&stream_id)
    }

    /// Takes the acquisition still running on `device`, if any — what a new
    /// `acq.start` on the same device has to end first. One device streams
    /// once at a time (SPEC §2.6: one owner per device); two overlapping
    /// acquisitions share one bulk endpoint and neither gets a whole stream.
    pub async fn remove_stream_of_device(&self, device: &str) -> Option<(u16, StreamHandle)> {
        let mut streams = self.0.streams.lock().await;
        let stream_id = streams
            .iter()
            .find(|(_, handle)| handle.device == device)
            .map(|(id, _)| *id)?;
        streams.remove(&stream_id).map(|handle| (stream_id, handle))
    }

    /// Stops every active acquisition and tells every `/ws` connection to
    /// close (SPECIFICATION.md's graceful-shutdown expectation): broadcasts
    /// the shutdown signal first (so connections start closing immediately,
    /// concurrently with the loop below), then aborts each stream's
    /// forwarder task and calls `Instrument::stop()` on its device.
    pub async fn shutdown(&self) {
        let _ = self.0.shutdown_tx.send(());
        let mut streams = self.0.streams.lock().await;
        for (stream_id, handle) in streams.drain() {
            handle.forwarder.abort();
            let mut instrument = handle.instrument.lock().await;
            if let Err(err) = instrument.stop().await {
                tracing::warn!(stream_id, %err, "instrument.stop() failed during shutdown");
            }
        }
        self.0.recorder.shutdown().await;
    }
}

/// Drives the instrument registry from bus changes for the life of the
/// server. Subscribes to the event bus rather than to the watcher directly,
/// so it reacts to exactly the `usb.changed` events clients see.
pub async fn run_instrument_hotplug(state: AppState) {
    use awic_proto::Event;
    use tokio::sync::broadcast::error::RecvError;
    let mut events = state.event_bus().subscribe();
    loop {
        match events.recv().await {
            Ok(Event::UsbChanged { device, change, .. }) => {
                state.handle_usb_change(&device, change).await
            }
            Ok(_) => {}
            // Missed events: reconcile from scratch rather than guess.
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "instrument hot-plug lagged; rediscovering");
                state.bootstrap().await;
            }
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awic_core::{
        AcqConfig, Capability, CaptureId, Instrument, Result as CoreResult, SampleSink,
    };
    use std::time::Duration;

    struct DummyInstrument {
        id: DeviceId,
        stopped: bool,
    }

    #[async_trait]
    impl Instrument for DummyInstrument {
        fn id(&self) -> &DeviceId {
            &self.id
        }
        fn capabilities(&self) -> &[Capability] {
            &[]
        }
        async fn prepare(&mut self) -> CoreResult<()> {
            Ok(())
        }
        async fn configure(&mut self, _cfg: AcqConfig) -> CoreResult<awic_core::EffectiveAcq> {
            Ok(awic_core::EffectiveAcq::digital(_cfg))
        }
        async fn start(&mut self, _sink: SampleSink) -> CoreResult<CaptureId> {
            Ok(CaptureId::new())
        }
        async fn stop(&mut self) -> CoreResult<()> {
            self.stopped = true;
            Ok(())
        }
    }

    /// An instrument whose next `configure` or `start` re-flashes the device,
    /// which is what an FX2 that stalled part-way through its last
    /// acquisition does (B-63). Both block on `gate` so the `flashing` window
    /// can be observed while it is open.
    struct ReflashingInstrument {
        id: DeviceId,
        gate: Arc<tokio::sync::Mutex<()>>,
    }

    #[async_trait]
    impl Instrument for ReflashingInstrument {
        fn id(&self) -> &DeviceId {
            &self.id
        }
        fn capabilities(&self) -> &[Capability] {
            &[]
        }
        fn pending_reset(&self) -> bool {
            true
        }
        async fn prepare(&mut self) -> CoreResult<()> {
            Ok(())
        }
        async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<awic_core::EffectiveAcq> {
            let _held = self.gate.lock().await;
            Ok(awic_core::EffectiveAcq::digital(cfg))
        }
        async fn start(&mut self, _sink: SampleSink) -> CoreResult<CaptureId> {
            let _held = self.gate.lock().await;
            Ok(CaptureId::new())
        }
        async fn stop(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }

    /// B-63: the driver re-uploads firmware and waits for a re-enumeration
    /// inside `configure` — two to four seconds in which the registry said
    /// `ready` and `list_instruments` offered a device that was not there.
    #[tokio::test]
    async fn a_device_being_re_flashed_is_announced_as_flashing_not_ready() {
        let state = AppState::new(
            Config::default(),
            Arc::new(DummyBackend),
            Arc::new(crate::usb::UnimplementedUsbController),
        );
        let id: DeviceId = "3-3.4.1.1".parse().unwrap();
        state.registry().detected(id.clone(), vec![], None);
        state.registry().set_state(&id, DeviceState::Ready, None);

        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let held = gate.clone().lock_owned().await;
        let mut instrument: Box<dyn Instrument> = Box::new(ReflashingInstrument {
            id: id.clone(),
            gate: gate.clone(),
        });

        let cfg = AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0],
            samples: 1024,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        };
        let announced = {
            let state = state.clone();
            let id = id.clone();
            tokio::spawn(async move {
                state
                    .configure_announced(&id, &mut instrument, cfg)
                    .await
                    .map(|_| ())
            })
        };

        // While `configure` is inside the re-flash, the device must not read
        // `ready`.
        wait_for_state(&state, "3-3.4.1.1", DeviceState::Flashing).await;
        drop(held);
        announced.await.unwrap().expect("configure must succeed");
        wait_for_state(&state, "3-3.4.1.1", DeviceState::Ready).await;
    }

    /// The same for `start`, where the fx2lafw driver actually resets a
    /// stalled device: unannounced, the hot-plug handler raced the reset with
    /// a second firmware upload.
    #[tokio::test]
    async fn a_start_that_re_flashes_is_announced_as_flashing() {
        let state = AppState::new(
            Config::default(),
            Arc::new(DummyBackend),
            Arc::new(crate::usb::UnimplementedUsbController),
        );
        let id: DeviceId = "3-3.4.1.1".parse().unwrap();
        state.registry().detected(id.clone(), vec![], None);
        state.registry().set_state(&id, DeviceState::Ready, None);

        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let held = gate.clone().lock_owned().await;
        let mut instrument: Box<dyn Instrument> = Box::new(ReflashingInstrument {
            id: id.clone(),
            gate: gate.clone(),
        });
        let announced = {
            let state = state.clone();
            let id = id.clone();
            tokio::spawn(async move {
                let (sink, _rx) = SampleSink::channel(1);
                state
                    .start_announced(&id, &mut instrument, sink)
                    .await
                    .map(|_| ())
            })
        };

        wait_for_state(&state, "3-3.4.1.1", DeviceState::Flashing).await;
        drop(held);
        announced.await.unwrap().expect("start must succeed");
        wait_for_state(&state, "3-3.4.1.1", DeviceState::Ready).await;
    }

    /// A driver that never re-flashes reports `pending_reset() == false`, and
    /// then nothing about its state may change: an ordinary configure must not
    /// announce a flash that is not happening.
    #[tokio::test]
    async fn an_ordinary_configure_does_not_announce_a_flash() {
        let state = AppState::new(
            Config::default(),
            Arc::new(DummyBackend),
            Arc::new(crate::usb::UnimplementedUsbController),
        );
        let id: DeviceId = "3-3.4.2".parse().unwrap();
        state.registry().detected(id.clone(), vec![], None);
        state.registry().set_state(&id, DeviceState::Ready, None);
        let mut instrument: Box<dyn Instrument> = Box::new(DummyInstrument {
            id: id.clone(),
            stopped: false,
        });
        state
            .configure_announced(
                &id,
                &mut instrument,
                AcqConfig {
                    rate_hz: 1_000_000,
                    channels: vec![0],
                    samples: 1024,
                    trigger: None,
                    ranges_v: vec![],
                    continuous: false,
                },
            )
            .await
            .expect("configure must succeed");
        assert_eq!(state.registry().state_of(&id), Some(DeviceState::Ready));
    }

    struct DummyBackend;

    #[async_trait]
    impl Backend for DummyBackend {
        fn name(&self) -> &str {
            "dummy"
        }
        async fn discover(&self) -> CoreResult<Vec<DeviceId>> {
            Ok(vec![])
        }
        async fn open(&self, id: &DeviceId) -> CoreResult<Box<dyn Instrument>> {
            Ok(Box::new(DummyInstrument {
                id: id.clone(),
                stopped: false,
            }))
        }
    }

    /// A backend that claims a fixed set of devices, whose instruments'
    /// `prepare()` can be held open to observe the `flashing` window.
    struct ClaimingBackend {
        claims: Vec<DeviceId>,
        gate: Option<Arc<tokio::sync::Notify>>,
    }

    struct GatedInstrument {
        id: DeviceId,
        gate: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait]
    impl Instrument for GatedInstrument {
        fn id(&self) -> &DeviceId {
            &self.id
        }
        fn capabilities(&self) -> &[Capability] {
            &[]
        }
        async fn prepare(&mut self) -> CoreResult<()> {
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            Ok(())
        }
        async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<awic_core::EffectiveAcq> {
            Ok(awic_core::EffectiveAcq::digital(cfg))
        }
        async fn start(&mut self, _sink: SampleSink) -> CoreResult<CaptureId> {
            Ok(CaptureId::new())
        }
        async fn stop(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Backend for ClaimingBackend {
        fn name(&self) -> &str {
            "claiming"
        }
        async fn discover(&self) -> CoreResult<Vec<DeviceId>> {
            Ok(self.claims.clone())
        }
        async fn open(&self, id: &DeviceId) -> CoreResult<Box<dyn Instrument>> {
            Ok(Box::new(GatedInstrument {
                id: id.clone(),
                gate: self.gate.clone(),
            }))
        }
    }

    fn claiming(claims: &[&str], gate: Option<Arc<tokio::sync::Notify>>) -> AppState {
        AppState::new(
            Config::default(),
            Arc::new(ClaimingBackend {
                claims: claims.iter().map(|c| c.parse().unwrap()).collect(),
                gate,
            }),
            Arc::new(UnimplementedUsbController),
        )
    }

    async fn wait_for_state(state: &AppState, device: &str, want: DeviceState) {
        let id: DeviceId = device.parse().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.registry().state_of(&id) != Some(want) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{device} never reached {want:?}"));
    }

    #[tokio::test]
    async fn an_instrument_that_leaves_the_bus_is_dropped() {
        let state = claiming(&["3-3.3"], None);
        state.ensure_open("3-3.3").await.unwrap();
        state.handle_usb_change("3-3.3", UsbChange::Removed).await;
        assert!(!state.registry().contains(&"3-3.3".parse().unwrap()));
        assert!(
            state.configured_instrument("3-3.3").await.is_none(),
            "a stale handle to a vanished device must not be reused"
        );
    }

    #[tokio::test]
    async fn an_instrument_that_appears_is_opened_and_prepared() {
        // The power-cycle case: the port comes back on and the device returns.
        let state = claiming(&["3-3.3"], None);
        state.handle_usb_change("3-3.3", UsbChange::Added).await;
        wait_for_state(&state, "3-3.3", DeviceState::Ready).await;
        assert!(state.configured_instrument("3-3.3").await.is_some());
    }

    #[tokio::test]
    async fn a_device_no_backend_claims_is_ignored() {
        let state = claiming(&["3-3.3"], None);
        state.handle_usb_change("3-3.4", UsbChange::Added).await; // a hub
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!state.registry().contains(&"3-3.4".parse().unwrap()));
    }

    #[tokio::test]
    async fn events_while_flashing_are_the_devices_own_reenumeration() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let state = claiming(&["3-3.4.2"], Some(gate.clone()));
        let opening = {
            let state = state.clone();
            tokio::spawn(async move { state.ensure_open("3-3.4.2").await })
        };
        wait_for_state(&state, "3-3.4.2", DeviceState::Flashing).await;
        // The FX2 upload disconnects and reconnects the device mid-prepare.
        state.handle_usb_change("3-3.4.2", UsbChange::Removed).await;
        state.handle_usb_change("3-3.4.2", UsbChange::Added).await;
        assert!(
            state.registry().contains(&"3-3.4.2".parse().unwrap()),
            "the device must not be discarded mid-flash"
        );
        gate.notify_one();
        opening.await.unwrap().unwrap();
        wait_for_state(&state, "3-3.4.2", DeviceState::Ready).await;
    }

    #[tokio::test]
    async fn ensure_open_caches_the_instrument() {
        let state = AppState::new(
            Config::default(),
            Arc::new(DummyBackend),
            Arc::new(UnimplementedUsbController),
        );
        let a = state.ensure_open("1-1").await.unwrap();
        let b = state.ensure_open("1-1").await.unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "second call must reuse the same instrument"
        );
    }

    #[tokio::test]
    async fn shutdown_stops_every_active_stream() {
        let state = AppState::new(
            Config::default(),
            Arc::new(DummyBackend),
            Arc::new(UnimplementedUsbController),
        );
        let instrument = state.ensure_open("1-1").await.unwrap();
        let forwarder = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        state
            .insert_stream(
                1,
                StreamHandle {
                    device: "1-1".to_string(),
                    instrument: instrument.clone(),
                    forwarder,
                    backpressure: Arc::new(StreamBackpressure::with_default_high_water_mark()),
                    trigger: None,
                },
            )
            .await;

        state.shutdown().await;

        assert!(
            state.remove_stream(1).await.is_none(),
            "stream must be gone"
        );
    }
}
