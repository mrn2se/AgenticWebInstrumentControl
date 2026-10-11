//! [`ScpiHost`]: everything the engine needs from whatever actually runs the
//! commands, expressed in the engine's own vocabulary (device selector
//! strings, channel numbers, capture handle strings) rather than any
//! transport's wire types — the boundary that keeps this crate ignorant of
//! `awic-server`/`awic-proto` entirely (task brief: "the crate knows nothing
//! of the server"). `awic-server::scpi` implements it by building
//! `awic_proto::Request`s and calling `dispatch::handle_request_with_origin`.

use async_trait::async_trait;

use crate::error::HostError;

/// `POS`/`NEG` for an edge trigger (SPEC §9's `ACQ:TRIG:EDGE:SLOP` example).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slope {
    Rising,
    Falling,
}

/// `USB:PORT:LOCK "<hub>",<port>,ON|OFF|AUTO` (SPEC §4.4: locked, unlocked,
/// or back to the automatic rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortLock {
    Lock,
    Unlock,
    Auto,
}

/// The edge trigger fields accumulated across `ACQ:TRIG:EDGE:SOUR`/`SLOP`/
/// `LEV`, applied together at `ACQ:STAR` — each field independent because
/// SCPI sets them one command at a time (SPEC §9's example sends all three
/// in one compound message, but nothing requires that).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EdgeTrigger {
    pub source_channel: Option<u32>,
    pub slope: Option<Slope>,
    pub level_v: Option<f64>,
}

/// Everything `ACQ:RATE`/`ACQ:CHAN`/`ACQ:SAMP`/`ACQ:TRIG:...` accumulate,
/// applied in one shot by `ACQ:STAR`/`REC:STAR`. Every field optional: the
/// host (which already has `awic_core::AcqConfig`'s defaults) fills in
/// whatever the session never set, exactly as `acq.configure` does for a
/// WebSocket client that omits fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcqSettings {
    pub rate_hz: Option<f64>,
    pub channels: Option<Vec<u32>>,
    pub samples: Option<u64>,
    pub trigger: Option<EdgeTrigger>,
    /// `ACQ:RANGe`: analog full-scale range in volts, applied to every
    /// selected channel. `None` leaves the device on its own default — which
    /// is all a SCPI client could do before this existed, so the scope always
    /// ran at 500 mV/div however it was driven over VISA (live test B-30).
    /// Ignored by digital instruments, exactly as `ranges_v` is on the wire.
    pub range_v: Option<f64>,
}

/// The measurement kinds exposed over SCPI (SPEC §9's `MEAS:FREQ?` example
/// plus the period/amplitude/duty-cycle siblings `awic_core::measure`
/// already computes) — a subset of `awic_proto::MeasurementKind`, not a
/// dependency on it (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measurement {
    Frequency,
    Period,
    Vpp,
    Vmin,
    Vmax,
    Vavg,
    Vrms,
    Duty,
}

/// One `REC:LIST?` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RecSummary {
    pub rec: u32,
    pub device: String,
    pub state: String,
    pub rate_hz: u64,
}

/// Everything the engine calls out to the host for. Every fallible method
/// returns [`HostError`] rather than panicking or inventing a SCPI code —
/// the engine wraps it into [`crate::error::ScpiError::Device`] and queues it
/// (SPEC §9: "device-specific positive codes ... carrying the server's
/// message").
#[async_trait]
pub trait ScpiHost: Send + Sync {
    /// `*IDN?`'s answer, in full — the host owns the exact identity string
    /// (manufacturer, model, serial, firmware version) since only it knows
    /// its own version.
    async fn idn(&self) -> String;

    /// `INST:CAT?`: every currently known instrument, in whatever form
    /// `INST:SEL` also accepts.
    async fn instrument_catalog(&self) -> Vec<String>;

    /// `INST:SEL "<selector>"`: selects an instrument by label or bus path.
    /// Returns the canonical selector to remember for `INST:SEL?` and as the
    /// implicit device for `ACQ:*`/`REC:STAR`.
    async fn select_instrument(&self, selector: &str) -> Result<String, HostError>;

    /// `USB:TREE?`: a compact, one-line-per-port text rendering of the
    /// topology (SPEC §9: "no bulk data").
    async fn usb_tree_text(&self) -> Result<String, HostError>;

    async fn usb_port_power_set(&self, hub: &str, port: u32, on: bool) -> Result<(), HostError>;
    async fn usb_port_power_get(&self, hub: &str, port: u32) -> Result<bool, HostError>;
    async fn usb_port_lock(&self, hub: &str, port: u32, mode: PortLock) -> Result<(), HostError>;

    /// `ACQ:STAR`: configures `device` (or the currently selected
    /// instrument) with `settings` and starts a finite acquisition. Returns
    /// the `capture://` handle.
    async fn acquire(&self, device: &str, settings: AcqSettings) -> Result<String, HostError>;
    async fn acquire_stop(&self, device: &str) -> Result<(), HostError>;

    /// `MEAS:<kind>? CHn[,"<capture>"]`. `capture` is the handle to measure
    /// on — the engine resolves "last capture" itself before calling this,
    /// so the host never needs to track that. Returns the value and its
    /// unit (`"Hz"`, `"V"`, `"s"`, `"%"`).
    async fn measure(
        &self,
        capture: &str,
        channel: u32,
        kind: Measurement,
    ) -> Result<(f64, String), HostError>;

    /// `REC:STAR` (on `device`, or the currently selected instrument):
    /// configures `device` with `settings` — the same accumulated
    /// `ACQ:RATE`/`ACQ:CHAN`/... this session built up, exactly as `ACQ:STAR`
    /// uses them for a finite capture — and starts a continuous recording.
    /// Returns its id.
    async fn rec_start(&self, device: &str, settings: AcqSettings) -> Result<u32, HostError>;
    async fn rec_stop(&self, rec: u32) -> Result<(), HostError>;
    async fn rec_list(&self) -> Vec<RecSummary>;
    /// `REC:SEAR?`: match times in seconds, oldest first.
    async fn rec_search(
        &self,
        rec: u32,
        pattern: &str,
        max_matches: Option<u32>,
    ) -> Result<Vec<f64>, HostError>;
    /// `REC:WAIT?`: `Some(t_s)` on a match, `None` on timeout (the engine
    /// renders a timeout as `9.91E+37`, SPEC §9).
    async fn rec_wait(
        &self,
        rec: u32,
        pattern: &str,
        timeout_s: f64,
    ) -> Result<Option<f64>, HostError>;
    /// `REC:SNAP? t0,t1`: returns a `capture://` handle.
    async fn rec_snapshot(&self, rec: u32, t0_s: f64, t1_s: f64) -> Result<String, HostError>;

    /// `DEC:LIST?` (SPEC §6, T21): the available decoders' names, e.g.
    /// `["UART", "I2C", "SPI", "CAN", "1-Wire"]`.
    async fn decoder_list(&self) -> Vec<String>;
    /// `DEC:CAPT? "<decoder>",(@ch,...)[,"<capture>"]`: decodes `decoder`
    /// over `capture` (or the last capture, if omitted), with `channels`
    /// bound positionally in that decoder's own declared role order (e.g.
    /// SPI: CLK, MOSI, MISO, CS; CAN and 1-Wire: their one bus line — a shorter list
    /// binds only the leading, required roles). `channels` are 1-based, exactly like `ACQ:CHAN`'s
    /// `(@n)` and `MEAS:<kind>? CHn` — the host converts to 0-based before
    /// calling `capture.decode`. Returns an already-bounded, compact text
    /// rendering (SPEC §9: "no bulk data over SCPI") — never the JSON
    /// annotation list the other transports get.
    async fn decode_capture_text(
        &self,
        decoder: &str,
        channels: &[u32],
        capture: &str,
    ) -> Result<String, HostError>;
}
