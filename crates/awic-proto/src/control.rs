//! JSON control-plane wire types (SPECIFICATION.md §3.1).
//!
//! One `/ws` socket carries text frames of these shapes and binary frames of
//! sample data (see [`crate::frame`]). `v` is the protocol version (`1`
//! today). Every request carries `id`; every response echoes it; events never
//! carry one.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::recorder::{
    RecAck, RecListResult, RecSearchResult, RecSnapshotResult, RecStartResult, RecViewResult,
    RecWaitResult, RecordingState,
};
use crate::{
    CaptureDecodeResult, ChannelMapping, DecodeFormat, DecodeListResult, DecodeOpenSpec, DecoderId,
    IntervalKind, McpImageResult, McpLogResult, McpLoggingState, McpStatusResult, OptionSetting,
    RecDecodeResult, RenderKind, RenderResult, SerialSettings, SurveyResult, TerminalKind,
    TerminalSourceInfo, TerminalState,
};

/// Protocol version. Bumped on a breaking wire change.
pub const PROTOCOL_VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// Requests (client -> server)
// ---------------------------------------------------------------------------

/// A client -> server request. Internally tagged on `op` so the wire shape is
/// exactly `{ "v": 1, "id": 42, "op": "device.list", ...op-specific fields }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "op")]
#[ts(export, export_to = "types.ts")]
pub enum Request {
    #[serde(rename = "device.list")]
    DeviceList { v: u8, id: u32 },

    #[serde(rename = "usb.tree")]
    UsbTree { v: u8, id: u32 },

    #[serde(rename = "usb.power")]
    UsbPower {
        v: u8,
        id: u32,
        hub: String,
        port: u8,
        on: bool,
    },

    /// Lock or unlock a port (SPECIFICATION.md §4.4): `protected: true` locks
    /// it, `false` unlocks it even where protection would apply by default,
    /// `null` returns it to the protection derived from what is attached.
    /// Session state — persisted, not configuration.
    #[serde(rename = "usb.protect")]
    UsbProtect {
        v: u8,
        id: u32,
        hub: String,
        port: u8,
        protected: Option<bool>,
    },

    /// Power-cycles a port in one call (live test F-08): off, `off_ms`
    /// (default 1000) later on, then waits up to `wait_s` (default 5, at most
    /// 30) for a device to enumerate on it again. Protection applies exactly
    /// as to `usb.power`.
    #[serde(rename = "usb.cycle")]
    UsbCycle {
        v: u8,
        id: u32,
        hub: String,
        port: u8,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        off_ms: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        wait_s: Option<f64>,
    },

    /// Names what a port feeds (live test F-08), e.g. `"DUT: RP2350 board"`,
    /// and marks it as the device under test — shown in `usb.tree` so a user
    /// or agent knows what switching it does. `label: null` clears the name;
    /// `dut` omitted keeps the current mark. Session state, like locks.
    #[serde(rename = "usb.label")]
    UsbLabel {
        v: u8,
        id: u32,
        hub: String,
        port: u8,
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        dut: Option<bool>,
    },

    #[serde(rename = "acq.configure")]
    AcqConfigure {
        v: u8,
        id: u32,
        device: String,
        cfg: AcqConfig,
    },

    #[serde(rename = "acq.start")]
    AcqStart { v: u8, id: u32, device: String },

    #[serde(rename = "acq.stop")]
    AcqStop { v: u8, id: u32, stream: u16 },

    /// Server-side measurement (SPECIFICATION.md §5.4): computed on the
    /// **full-resolution** samples held in the capture store, never on a
    /// decimated preview. `capture` is the `capture://<uuid>` handle
    /// `acq.start` returned; `channel` addresses one channel *as the store
    /// lays it out* — see [`MeasurementKind`] and `awic-core::measure` for
    /// what that means for analog vs. digital captures.
    #[serde(rename = "capture.measure")]
    CaptureMeasure {
        v: u8,
        id: u32,
        capture: String,
        channel: u8,
        measurements: Vec<MeasurementKind>,
    },

    /// Configures the device for continuous acquisition (forced on
    /// regardless of `cfg.continuous`) and starts it as a recording
    /// (SPECIFICATION.md §2.8, §3.4) — `acq.configure` plus a continuous
    /// start in one step.
    #[serde(rename = "rec.start")]
    RecStart {
        v: u8,
        id: u32,
        device: String,
        cfg: AcqConfig,
    },

    #[serde(rename = "rec.stop")]
    RecStop { v: u8, id: u32, rec: u16 },

    /// Removes a stopped/errored recording's data. Refused while `state ==
    /// recording` — call `rec.stop` first.
    #[serde(rename = "rec.delete")]
    RecDelete { v: u8, id: u32, rec: u16 },

    #[serde(rename = "rec.list")]
    RecList { v: u8, id: u32 },

    /// Opens a view: `follow_s` for a live trailing window, or `t0_s`/`t1_s`
    /// for a fixed range (SPECIFICATION.md §3.4). Views belong to this
    /// connection and die with it.
    #[serde(rename = "rec.view")]
    RecView {
        v: u8,
        id: u32,
        rec: u16,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        follow_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
        columns: u32,
        /// Live-tail rate for a `follow_s` view; default 30, max 60. Ignored
        /// for a fixed view.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        fps: Option<u32>,
    },

    /// Changes an open view's window/columns/fps in place; omitted fields
    /// keep their current value.
    #[serde(rename = "rec.view.update")]
    RecViewUpdate {
        v: u8,
        id: u32,
        view: u16,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        follow_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        columns: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        fps: Option<u32>,
    },

    #[serde(rename = "rec.view.close")]
    RecViewClose { v: u8, id: u32, view: u16 },

    /// History search (SPECIFICATION.md §2.9.3). `t0_s`/`t1_s` default to
    /// the retained span; searching evicted history is an error naming what
    /// is retained, not an empty result.
    #[serde(rename = "rec.search")]
    RecSearch {
        v: u8,
        id: u32,
        rec: u16,
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
        /// Default 16, hard maximum 1024 (SPEC §2.9.3, §8.3's context budget).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_matches: Option<u32>,
    },

    /// Resolves on the first match after this request arrives (or after
    /// `from_s`), or times out (SPECIFICATION.md §2.9.3). `timeout_ms` is
    /// capped at 600000; ends with an error if the recording stops first.
    #[serde(rename = "rec.wait")]
    RecWait {
        v: u8,
        id: u32,
        rec: u16,
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        timeout_ms: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        from_s: Option<f64>,
    },

    /// Decompresses `[t0_s, t1_s)` of a recording into an ordinary capture
    /// (SPECIFICATION.md §2.8.5), capped at 64 Mi samples.
    #[serde(rename = "rec.snapshot")]
    RecSnapshot {
        v: u8,
        id: u32,
        rec: u16,
        t0_s: f64,
        t1_s: f64,
    },

    /// Bounded, decimated read of one channel of a capture (SPECIFICATION.md
    /// §8.3, added for M7/T22's `read_capture` MCP tool): at most
    /// `max_points` `(min, max)` buckets, never full-resolution samples.
    /// `channel` is a hardware channel number, exactly as `capture.measure`
    /// takes it. `max_points` defaults to 512, hard-capped at 4096
    /// regardless of what is asked.
    #[serde(rename = "capture.read")]
    CaptureRead {
        v: u8,
        id: u32,
        capture: String,
        channel: u8,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_points: Option<u32>,
        /// Read only `[t0_s, t1_s)` seconds of the capture (default: all of
        /// it) — zooming without a new capture (live test B-13).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
    },

    /// Decodes a protocol over a finished capture's channels (SPECIFICATION.md
    /// §6, §8.2). Bounded: `max_annotations` defaults to
    /// [`crate::DEFAULT_MAX_ANNOTATIONS`], hard-capped at
    /// [`crate::HARD_MAX_ANNOTATIONS`] regardless of what is asked — the
    /// result says how many were omitted rather than silently truncating.
    #[serde(rename = "capture.decode")]
    CaptureDecode {
        v: u8,
        id: u32,
        capture: String,
        decoder: DecoderId,
        channels: Vec<ChannelMapping>,
        #[serde(default)]
        options: Vec<OptionSetting>,
        /// Seconds from the capture's start; default its start and end.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_annotations: Option<u32>,
        /// Drop annotations nested deeper than this (0 frames, 1 fields,
        /// 2 bits) before counting against `max_annotations`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_level: Option<u8>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        format: Option<DecodeFormat>,
        /// Transactions format: the most data bytes one UART burst or
        /// CS-less SPI burst carries before it is continued in the next entry
        /// (B-97). Default [`crate::DEFAULT_MAX_TRANSACTION_BYTES`].
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_bytes: Option<u32>,
        /// Transactions format: fold consecutive identical transactions into
        /// one entry with a `repeat` count (F-14). Default on.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        fold: Option<bool>,
    },

    /// Decodes a protocol over `[t0_s, t1_s)` of a recording (SPECIFICATION.md
    /// §6), streamed through the decoder in bounded windows rather than
    /// materializing the whole range. `t0_s`/`t1_s` default to the retained
    /// span, same convention as `rec.search`.
    #[serde(rename = "rec.decode")]
    RecDecode {
        v: u8,
        id: u32,
        rec: u16,
        decoder: DecoderId,
        channels: Vec<ChannelMapping>,
        #[serde(default)]
        options: Vec<OptionSetting>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_annotations: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_level: Option<u8>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        format: Option<DecodeFormat>,
        /// Transactions format: the most data bytes one UART burst or
        /// CS-less SPI burst carries before it is continued in the next entry
        /// (B-97). Default [`crate::DEFAULT_MAX_TRANSACTION_BYTES`].
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max_bytes: Option<u32>,
        /// Transactions format: fold consecutive identical transactions into
        /// one entry with a `repeat` count (F-14). Default on.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        fold: Option<bool>,
    },

    /// Surveys a digital capture, or `[t0_s, t1_s)` of a digital recording
    /// (live test F-04): per channel activity, idle level, pulse widths and
    /// rhythm, plus protocol guesses (UART and its baud rate, an I²C pair, a
    /// clock). Give exactly one of `capture` and `rec`.
    #[serde(rename = "survey")]
    Survey {
        v: u8,
        id: u32,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        capture: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rec: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
    },

    /// Draws a capture (SPECIFICATION.md §5.6) and returns the picture plus
    /// the numbers it measured. This is the operation behind the web UI's
    /// snapshot button and an agent's "what does this look like?", and it is
    /// the same renderer either way.
    #[serde(rename = "capture.render")]
    CaptureRender {
        v: u8,
        id: u32,
        capture: String,
        kind: RenderKind,
        /// Hardware channel numbers (D4 is 4, CH1 is 0), as every other op
        /// takes them; empty or omitted draws all of them. The eye view takes
        /// exactly one.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        channels: Vec<u8>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        width: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        height: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t0_s: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        t1_s: Option<f64>,
        /// Eye view: the period to fold on. Omitted recovers it from the
        /// signal, which is usually what is wanted — give it when the signal
        /// has a known bit rate and you want to see how far it strays.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        period_s: Option<f64>,
        /// Histogram and anomaly views: which intervals to measure. Defaults
        /// to periods.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        interval_kind: Option<IntervalKind>,
        /// Anomaly view: how far an interval may stray from the dominant one
        /// before it is called out, as a fraction (`0.05` = 5 %).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tolerance: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        title: Option<String>,
    },

    /// The available decoders, their required channel roles and their
    /// options (types, defaults, allowed values) — so a UI or agent can
    /// build a decode form/tool-call with no hardcoded protocol knowledge
    /// (SPECIFICATION.md T21).
    #[serde(rename = "decode.list")]
    DecodeList { v: u8, id: u32 },

    /// The terminal module's source list (SPECIFICATION.md §11.2/§11.2.1):
    /// every `serial` interface currently on the bus plus the fixed `log`
    /// source. Discovered fresh each call, never cached server-side either.
    #[serde(rename = "term.sources")]
    TermSources { v: u8, id: u32 },

    /// Opens (or joins, if already open) a terminal session against
    /// `source` — a [`TerminalSourceInfo::source`] from `term.sources`.
    /// `serial` configures the UART frame format and is required for a
    /// `serial` source, ignored for `log` (SPEC §11.2's example).
    #[serde(rename = "term.open")]
    TermOpen {
        v: u8,
        id: u32,
        source: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        serial: Option<SerialSettings>,
        /// Required (and only meaningful) for the `decode` source (SPEC
        /// §11.2): "asks for the recording, decoder, channels and options."
        #[serde(skip_serializing_if = "Option::is_none", default)]
        decode: Option<DecodeOpenSpec>,
    },

    /// Writes to an open serial session. Exactly one of `data` (UTF-8 text
    /// — the common case, typed input) or `data_b64` (arbitrary bytes) must
    /// be set; refused for a `log` session, which is read-only.
    #[serde(rename = "term.write")]
    TermWrite {
        v: u8,
        id: u32,
        session: u16,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        data_b64: Option<String>,
    },

    /// Leaves a terminal session. The underlying serial port closes once
    /// the last connection leaves it (SPECIFICATION.md §11.2.1); a `log`
    /// session has no underlying resource to release.
    #[serde(rename = "term.close")]
    TermClose { v: u8, id: u32, session: u16 },

    /// The MCP endpoint's connection details, sessions and logging state
    /// (SPECIFICATION.md §8.5).
    #[serde(rename = "mcp.status")]
    McpStatus { v: u8, id: u32 },

    /// MCP log entries with `seq > after` (all retained ones when omitted),
    /// oldest first, at most `limit` (default 500).
    #[serde(rename = "mcp.log")]
    McpLog {
        v: u8,
        id: u32,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        after: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        limit: Option<u32>,
    },

    /// The `index`-th picture of log entry `seq`.
    #[serde(rename = "mcp.image")]
    McpImage {
        v: u8,
        id: u32,
        seq: u32,
        index: u32,
    },

    /// Switches the MCP message log (and its pictures) on or off, or clears
    /// it. Omitted fields are left as they are.
    #[serde(rename = "mcp.logging")]
    McpLogging {
        v: u8,
        id: u32,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        enabled: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        images: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        clear: Option<bool>,
    },
}

impl Request {
    /// The `id` every request carries, regardless of op.
    pub fn id(&self) -> u32 {
        match self {
            Request::DeviceList { id, .. }
            | Request::UsbTree { id, .. }
            | Request::UsbPower { id, .. }
            | Request::UsbProtect { id, .. }
            | Request::UsbCycle { id, .. }
            | Request::UsbLabel { id, .. }
            | Request::Survey { id, .. }
            | Request::CaptureRender { id, .. }
            | Request::AcqConfigure { id, .. }
            | Request::AcqStart { id, .. }
            | Request::AcqStop { id, .. }
            | Request::CaptureMeasure { id, .. }
            | Request::CaptureDecode { id, .. }
            | Request::RecDecode { id, .. }
            | Request::DecodeList { id, .. }
            | Request::RecStart { id, .. }
            | Request::RecStop { id, .. }
            | Request::RecDelete { id, .. }
            | Request::RecList { id, .. }
            | Request::RecView { id, .. }
            | Request::RecViewUpdate { id, .. }
            | Request::RecViewClose { id, .. }
            | Request::RecSearch { id, .. }
            | Request::RecWait { id, .. }
            | Request::RecSnapshot { id, .. }
            | Request::CaptureRead { id, .. }
            | Request::TermSources { id, .. }
            | Request::TermOpen { id, .. }
            | Request::TermWrite { id, .. }
            | Request::TermClose { id, .. }
            | Request::McpStatus { id, .. }
            | Request::McpLog { id, .. }
            | Request::McpImage { id, .. }
            | Request::McpLogging { id, .. } => *id,
        }
    }
}

// ---------------------------------------------------------------------------
// Responses (server -> client)
// ---------------------------------------------------------------------------

/// A server -> client response to a [`Request`].
///
/// Modeled as a struct rather than an enum tagged on `ok`: serde's tagged-enum
/// support requires a *string* discriminant, but the wire format uses a JSON
/// boolean (`"ok": true|false`, per SPECIFICATION.md §3.1). `result` is set
/// when `ok` is `true`, `error` when it is `false`; the two constructors
/// ([`Response::ok`], [`Response::err`]) are the only supported way to build
/// one, so the invariant holds by construction, and both fields are omitted
/// (not emitted as `null`) rather than present-and-empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct Response {
    pub v: u8,
    pub id: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<ResponseResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<ErrorInfo>,
}

impl Response {
    pub fn ok(id: u32, result: ResponseResult) -> Self {
        Response {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u32, code: impl Into<String>, msg: impl Into<String>) -> Self {
        Response {
            v: PROTOCOL_VERSION,
            id,
            ok: false,
            result: None,
            error: Some(ErrorInfo {
                code: code.into(),
                msg: msg.into(),
            }),
        }
    }
}

/// `{ "code": "port_protected", "msg": "..." }`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct ErrorInfo {
    pub code: String,
    pub msg: String,
}

/// The `result` payload of a successful [`Response`]. Untagged: the caller
/// already knows which variant to expect from the `id` of the request they
/// sent, exactly as the wire format has no explicit discriminant here either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(untagged)]
#[ts(export, export_to = "types.ts")]
pub enum ResponseResult {
    DeviceList(DeviceListResult),
    UsbTree(UsbTreeResult),
    UsbPower(UsbPowerResult),
    UsbProtect(UsbProtectResult),
    AcqConfigure(AcqConfigureResult),
    AcqStart(AcqStartResult),
    AcqStop(AcqStopResult),
    CaptureMeasure(CaptureMeasureResult),
    CaptureDecode(CaptureDecodeResult),
    RecDecode(RecDecodeResult),
    DecodeList(DecodeListResult),
    RecStart(RecStartResult),
    RecStop(RecAck),
    RecDelete(RecAck),
    RecList(RecListResult),
    RecView(RecViewResult),
    RecViewUpdate(RecAck),
    RecViewClose(RecAck),
    RecSearch(RecSearchResult),
    RecWait(RecWaitResult),
    RecSnapshot(RecSnapshotResult),
    CaptureRead(CaptureReadResult),
    TermSources(TermSourcesResult),
    TermOpen(TermOpenResult),
    TermWrite(TermWriteResult),
    TermClose(TermCloseResult),
    UsbCycle(UsbCycleResult),
    UsbLabel(UsbLabelResult),
    Survey(SurveyResult),
    CaptureRender(RenderResult),
    McpStatus(McpStatusResult),
    McpLog(McpLogResult),
    McpImage(McpImageResult),
    McpLogging(McpLoggingState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct DeviceListResult {
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct UsbTreeResult {
    pub roots: Vec<UsbTreeNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct UsbPowerResult {
    pub hub: String,
    pub port: u8,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct UsbProtectResult {
    pub hub: String,
    pub port: u8,
    /// Whether the port is protected now.
    pub protected: bool,
    /// Why, when protected.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
    /// The user's choice now on record: `Some` locked/unlocked, `None` automatic.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_choice: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct UsbCycleResult {
    pub hub: String,
    pub port: u8,
    pub off_ms: u32,
    /// What enumerated on the port after power came back, if anything did
    /// within `wait_s`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub device: Option<Box<UsbTreeNode>>,
    /// Seconds from power-on until `device` appeared.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub enumerated_after_s: Option<f64>,
    /// The port's label and DUT mark, so the caller sees what it just reset.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub dut: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct UsbLabelResult {
    pub hub: String,
    pub port: u8,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
    pub dut: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct AcqConfigureResult {
    pub device: String,
    /// What the device will **actually** do — which may differ from what was
    /// asked: a rate rounded to a supported one, a range snapped to the
    /// nearest the hardware offers. Clients must build their timebase and
    /// scales from this, never from their own request.
    pub cfg: AcqConfig,
    /// How to turn each enabled channel's raw codes into physical units.
    pub calibration: Vec<ChannelCalibration>,
}

/// Per-channel conversion from raw device codes to physical units:
/// `value = raw * scale + offset`.
///
/// Supplied by the driver, because only the driver knows it: the ADC
/// resolution, the analog front-end gain for the selected range, and
/// per-range calibration offsets (the FX2 oscilloscope stores these on the
/// device). A client deriving volts from bit depth alone would be off by
/// exactly those offsets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct ChannelCalibration {
    pub channel: u8,
    pub scale: f32,
    pub offset: f32,
    /// `"V"` for analog; empty for digital logic levels (0/1).
    pub unit: String,
    /// The ± full-scale range actually applied, analog only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub range_v: Option<f32>,
}

impl ChannelCalibration {
    /// Digital channels: codes are already logic levels.
    pub fn digital(channel: u8) -> Self {
        Self {
            channel,
            scale: 1.0,
            offset: 0.0,
            unit: String::new(),
            range_v: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct AcqStartResult {
    pub device: String,
    pub stream: u16,
    /// `capture://<uuid>` handle, addressable once the capture completes.
    pub capture: String,
}

/// `deny_unknown_fields`: without it this empty struct deserializes
/// successfully from *any* JSON object (serde ignores unrecognized fields by
/// default), which makes it a black hole in [`ResponseResult`]'s untagged
/// enum — every later variant with the field set it should be rejecting
/// instead matches `AcqStopResult` first and silently loses its data. Caught
/// by `response_round_trips_through_json_for_every_result_variant` once a
/// variant appeared after it in declaration order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export, export_to = "types.ts")]
pub struct AcqStopResult {}

/// A named measurement (SPECIFICATION.md §5.4). Analog and digital kinds
/// share one flat wire vocabulary rather than two separate enums: `frequency`
/// / `period` / `duty` mean the same thing on either capture type, and a
/// kind that does not apply to the capture actually addressed (e.g. `vpp` on
/// a digital capture) comes back as an `unavailable` result rather than a
/// protocol-level error — a client that requests the wrong kind for a
/// channel finds out per-measurement, not by having the whole call fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum MeasurementKind {
    /// Analog: peak-to-peak amplitude, `Vmax - Vmin`.
    Vpp,
    /// Analog: minimum sample voltage.
    Vmin,
    /// Analog: maximum sample voltage.
    Vmax,
    /// Analog: arithmetic mean voltage.
    Vavg,
    /// Analog: true RMS voltage, **including** any DC component.
    Vrms,
    /// Analog or digital: mean of complete like-edge-to-like-edge periods.
    Frequency,
    /// Analog or digital: `1 / frequency`, from the same period measurement.
    Period,
    /// Analog or digital: fraction of each period spent "high".
    Duty,
    /// Analog: 10%-90% rise time between the waveform's settled low and high
    /// levels.
    RiseTime,
    /// Analog: 90%-10% fall time between the waveform's settled high and low
    /// levels.
    FallTime,
    /// Digital: mean duration of a high (logic 1) pulse.
    PulseWidthHigh,
    /// Digital: mean duration of a low (logic 0) pulse.
    PulseWidthLow,
    /// Digital: total number of edges (both directions) seen on the channel.
    EdgeCount,
}

/// One requested measurement's outcome: either a value with its unit, or a
/// typed reason it could not be computed (SPECIFICATION.md §5.4 — "report
/// 'not measurable' honestly"). Untagged so the wire shape is exactly
/// `{ "kind": "frequency", "value": 1000.2, "unit": "Hz" }` or
/// `{ "kind": "duty", "unavailable": "..." }`, never a number standing in for
/// a measurement that could not honestly be made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(untagged)]
#[ts(export, export_to = "types.ts")]
pub enum MeasurementResult {
    Ok {
        kind: MeasurementKind,
        value: f64,
        unit: String,
        /// A qualifier the number must not travel without (B-39/B-40/B-41):
        /// "describes the line inside N bursts", "upper bound: the edge spans
        /// two samples or fewer", "zero offset not calibrated". Absent when
        /// the value needs no caveat, so the common wire shape is unchanged.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        #[ts(optional)]
        note: Option<String>,
    },
    Unavailable {
        kind: MeasurementKind,
        /// Human-readable reason, e.g. "capture is shorter than one period".
        unavailable: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct CaptureMeasureResult {
    pub capture: String,
    pub channel: u8,
    /// One entry per requested [`MeasurementKind`], same order as the request.
    pub results: Vec<MeasurementResult>,
}

/// `capture.read`'s result (SPECIFICATION.md §8.3): one channel, decimated.
/// `min`/`max` share an index with each other — bucket `i` spans
/// `[t0_s + i*bucket_s, t0_s + (i+1)*bucket_s)` — and are always the same
/// length as each other, at most the request's (capped) `max_points`.
/// Digital channels use the same shape: a bucket's AND becomes `min` (0.0 or
/// 1.0), its OR becomes `max`, so a toggled bucket reads as `(0.0, 1.0)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct CaptureReadResult {
    pub capture: String,
    pub channel: u8,
    pub rate_hz: u64,
    /// `"V"` for analog; empty for digital logic levels.
    pub unit: String,
    pub min: Vec<f32>,
    pub max: Vec<f32>,
    /// `true` if the capture holds more points than were returned, i.e. the
    /// (possibly capped) `max_points` bound was reached.
    pub truncated: bool,
    /// The range read, seconds, when a window was asked for.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub t0_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub t1_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct TermSourcesResult {
    pub sources: Vec<TerminalSourceInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
///
/// SPEC §11.2's example shows a bare `{ "session": 3 }` — deliberately kept
/// fuller here: `source`/`kind`/`label` let the client render the new
/// sub-tab immediately with no extra round trip (it already sent `source`
/// in the request, but not necessarily `kind`/`label`, which it read off an
/// earlier `term.sources`), and `state` gives it a correct initial badge
/// before the first `term.state` event — which can otherwise race a slow
/// first render. All additive; a client that only reads `session` behaves
/// exactly as the minimal example describes.
pub struct TermOpenResult {
    /// Identifies this session on the wire from now on: in `term.write`,
    /// `term.close`, `term.state` events, and as the `session` field of
    /// every `0xA7` output frame for it. Stable across every connection that
    /// joins the same underlying source (SPECIFICATION.md §11.2.1) — two
    /// clients opening the same serial port see the same `session`.
    pub session: u16,
    pub source: String,
    pub kind: TerminalKind,
    pub label: String,
    pub state: TerminalState,
    /// `serial` sessions: the frame format the port is open at — on a join,
    /// the one it was first opened with.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub serial: Option<crate::SerialSettings>,
}

/// `deny_unknown_fields`: see [`AcqStopResult`]'s doc comment — the same
/// untagged-enum footgun applies to every empty result struct here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export, export_to = "types.ts")]
pub struct TermWriteResult {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export, export_to = "types.ts")]
pub struct TermCloseResult {}

// ---------------------------------------------------------------------------
// Events (server -> client, unsolicited)
// ---------------------------------------------------------------------------

/// A server -> client event. Internally tagged on `event`; events never
/// carry an `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "event")]
#[ts(export, export_to = "types.ts")]
pub enum Event {
    #[serde(rename = "device.added")]
    DeviceAdded { v: u8, device: DeviceInfo },

    #[serde(rename = "device.removed")]
    DeviceRemoved { v: u8, device: String },

    #[serde(rename = "device.state")]
    DeviceState {
        v: u8,
        device: String,
        state: DeviceState,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        msg: Option<String>,
    },

    #[serde(rename = "acq.state")]
    AcqState {
        v: u8,
        stream: u16,
        state: AcqState,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        msg: Option<String>,
    },

    /// The physical USB topology changed: a device appeared on or vanished
    /// from the bus (SPECIFICATION.md §4.3). Covers *every* USB device, not
    /// just instruments — hubs, unrelated peripherals, and an instrument
    /// mid-re-enumeration all count. Carries no tree; clients re-fetch
    /// `usb.tree`. Distinct from `device.*`, which describe the instrument
    /// registry, not the bus.
    #[serde(rename = "usb.changed")]
    UsbChanged {
        v: u8,
        /// Bus path of the device that changed, e.g. `"3-3.3"`.
        device: String,
        change: UsbChange,
    },

    /// A recording (SPECIFICATION.md §2.8) started, stopped, or ended in
    /// error — device loss or a driver error while streaming. The data
    /// stays queryable in every state until `rec.delete` or eviction.
    #[serde(rename = "rec.state")]
    RecState {
        v: u8,
        rec: u16,
        state: RecordingState,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        msg: Option<String>,
    },
    /// A terminal session's lifecycle changed (SPECIFICATION.md §11.2.1):
    /// its device left the bus (`waiting`, scrollback kept), came back
    /// (`open` again, automatically), or the session ended (`closed`).
    /// Pushed to every connection subscribed to the session, not just the
    /// one that caused the transition.
    #[serde(rename = "term.state")]
    TermState {
        v: u8,
        session: u16,
        state: TerminalState,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        msg: Option<String>,
    },

    /// The terminal source list changed — a serial interface appeared or
    /// disappeared. Carries no list, like `usb.changed`: clients re-fetch
    /// `term.sources`.
    #[serde(rename = "term.sources")]
    TermSourcesChanged { v: u8 },

    /// Something happened on `/mcp` (SPECIFICATION.md §8.5): a request, a
    /// session starting or ending. Carries no entry, like `usb.changed`:
    /// a client showing the MCP tab fetches `mcp.log` after its last `seq`
    /// and, when `sessions_changed`, `mcp.status`.
    #[serde(rename = "mcp.activity")]
    McpActivity {
        v: u8,
        seq: u32,
        sessions_changed: bool,
    },
}

/// What happened to a device on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export, export_to = "types.ts")]
pub enum UsbChange {
    Added,
    Removed,
    /// A port's protection changed (locked/unlocked); `device` is the hub.
    Protection,
}

// ---------------------------------------------------------------------------
// Shared value types
// ---------------------------------------------------------------------------

/// Lifecycle state of a device as tracked by `awic-core`'s `Registry`
/// (SPECIFICATION.md §4.5). Serializes as a bare lower-case string, e.g.
/// `"flashing"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum DeviceState {
    Detected,
    Flashing,
    Ready,
    Error,
}

/// Lifecycle state of an acquisition stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum AcqState {
    Armed,
    Running,
    Stopped,
    Error,
}

/// A device as reported to clients: identity, capabilities, and current
/// state. `id` is the `DeviceId` bus path (e.g. `"3-3.4.2"`) formatted as a
/// string — `awic-proto` does not depend on `awic-core`, so it never sees the
/// structured `DeviceId` type itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct DeviceInfo {
    pub id: String,
    pub vid: u16,
    pub pid: u16,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
    pub state: DeviceState,
    pub capabilities: Vec<CapabilityInfo>,
}

/// Wire form of `awic-core::Capability`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum CapabilityInfo {
    Analog {
        channels: u8,
        bits: u8,
        rates_hz: Vec<u64>,
        ranges_v: Vec<f32>,
    },
    Digital {
        channels: u8,
        max_rate_hz: u64,
        /// The driver's exact rate ladder (like `Analog::rates_hz`), so the
        /// UI can offer e.g. 1 MHz for the logic analyzer instead of only the
        /// ceiling. `#[serde(default)]` keeps this wire-compatible with any
        /// consumer built against the older, ladder-less `Digital` variant:
        /// it decodes fine with an empty ladder, which callers should treat
        /// the same as "only `max_rate_hz` is known".
        #[serde(default)]
        rates_hz: Vec<u64>,
    },
    PowerSupply {
        channels: u8,
        v_max: f32,
        i_max: f32,
    },
}

/// A USB device in the topology tree (SPECIFICATION.md §4.1) — a hub or a
/// leaf. A hub's downstream ports are listed in [`UsbTreeNode::ports`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct UsbTreeNode {
    pub bus: u8,
    pub port_path: Vec<u8>,
    pub vid: u16,
    pub pid: u16,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub product: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub speed: Option<String>,
    pub is_hub: bool,
    /// Per-port power switching supported by this hub's descriptor. `None`
    /// for leaf devices, or if the descriptor could not be read.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ppps: Option<bool>,
    pub is_instrument: bool,
    /// Hubs only: **every** downstream port, `1..=bNbrPorts`, whether or not
    /// anything is attached. Empty for leaf devices.
    pub ports: Vec<UsbPort>,
}

/// One downstream port of a hub — the thing that is actually switched.
///
/// Ports are first-class, and are listed whether or not a device is attached,
/// for a reason that is easy to get wrong: power belongs to the *port*, not
/// to the device. If power state lived on device nodes, switching a port off
/// would unpower its device, the device would drop off the bus, its node
/// would vanish — and with it the only control able to switch the port back
/// on. The user could turn a device off from the UI but never on again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct UsbPort {
    /// 1-based port number on the parent hub.
    pub number: u8,
    /// Current port power state, where known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub power: Option<bool>,
    /// Hardware capability: the hub switches this port's power (`ppps`).
    pub power_switchable: bool,
    /// Whether switching is refused until the port is unlocked (§4.4).
    pub protected: bool,
    /// The protection in force and why: derived from what is attached, or
    /// locked by the user.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub protection: Option<String>,
    /// What the automatic rule says about this port — the reason it would be
    /// protected by default — *regardless of the user's choice*. Present even
    /// when the user has locked or unlocked it, so a UI can warn before
    /// unlocking a port that feeds a hub or a keyboard however it got to its
    /// current state.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub derived_protection: Option<String>,
    /// The user's choice on record for this port: `true` locked, `false`
    /// unlocked, absent = automatic (derived from what is attached).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_choice: Option<bool>,
    /// What the port feeds, as named with `usb.label` (live test F-08).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
    /// Marked as the device under test: switching it resets what is being
    /// tested.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub dut: bool,
    /// Whether the UI may offer a power toggle right now: power-switchable
    /// and not protected.
    pub switchable: bool,
    /// Why `switchable` is false — a hardware limit or the protection —
    /// shown in place of a dead control (§4.4, §11).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub non_switchable_reason: Option<String>,
    /// The device attached to this port, if any. `None` for an empty port —
    /// and for a port that is switched off, since its device is unpowered.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub device: Option<Box<UsbTreeNode>>,
}

/// Acquisition configuration, shared verbatim between the wire protocol and
/// `awic-core::Instrument::configure` (SPECIFICATION.md §3.1, §2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct AcqConfig {
    pub rate_hz: u64,
    pub channels: Vec<u8>,
    pub samples: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trigger: Option<Trigger>,
    /// Analog only: the requested ± full-scale range, in volts, for each
    /// entry of `channels` in order — values from the device's
    /// `Capability::Analog.ranges_v`. Empty means the device default. Ignored
    /// by digital devices. Without this there is no way to set a scope's gain.
    /// Omitted from the wire when empty, so a digital request is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges_v: Vec<f32>,
    /// Stream until `Instrument::stop()` instead of capturing exactly `samples`
    /// (SPEC §2.3.2). `samples` is then ignored — 0 is a valid value. Omitted
    /// from the wire when false, so every existing finite-capture request is
    /// unchanged.
    #[serde(default, skip_serializing_if = "is_false")]
    pub continuous: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum Slope {
    Rising,
    Falling,
}

/// A trigger condition. Only the `edge` kind exists today; the `kind` tag
/// leaves room to add e.g. pulse-width triggers without a wire break.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum Trigger {
    Edge {
        channel: u8,
        slope: Slope,
        /// Volts, analog channels only. A digital edge is a change of bit, so
        /// a digital trigger leaves it out (B-96); an analog trigger without
        /// one is refused at `acq.configure`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        level: Option<f32>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn device_list_request_round_trips_and_matches_spec_shape() {
        let json = json!({ "v": 1, "id": 42, "op": "device.list" });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(req, Request::DeviceList { v: 1, id: 42 });
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn usb_tree_request_round_trips() {
        let json = json!({ "v": 1, "id": 43, "op": "usb.tree" });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(req, Request::UsbTree { v: 1, id: 43 });
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn usb_power_request_matches_spec_example() {
        let json =
            json!({ "v": 1, "id": 44, "op": "usb.power", "hub": "3-3", "port": 3, "on": false });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::UsbPower {
                v: 1,
                id: 44,
                hub: "3-3".into(),
                port: 3,
                on: false,
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
        assert_eq!(req.id(), 44);
    }

    #[test]
    fn acq_configure_request_matches_spec_example() {
        let json = json!({
            "v": 1, "id": 45, "op": "acq.configure", "device": "3-3.4.2",
            "cfg": {
                "rate_hz": 24000000, "channels": [0, 1], "samples": 1048576,
                "trigger": { "kind": "edge", "channel": 0, "slope": "rising", "level": 1.5 }
            }
        });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::AcqConfigure {
                v: 1,
                id: 45,
                device: "3-3.4.2".into(),
                cfg: AcqConfig {
                    rate_hz: 24_000_000,
                    channels: vec![0, 1],
                    samples: 1_048_576,
                    trigger: Some(Trigger::Edge {
                        channel: 0,
                        slope: Slope::Rising,
                        level: Some(1.5),
                    }),
                    ranges_v: vec![],
                    continuous: false,
                },
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn acq_start_and_stop_requests_round_trip() {
        let start = Request::AcqStart {
            v: 1,
            id: 46,
            device: "3-3.4.2".into(),
        };
        let round_tripped: Request =
            serde_json::from_value(serde_json::to_value(&start).unwrap()).unwrap();
        assert_eq!(start, round_tripped);

        let stop = Request::AcqStop {
            v: 1,
            id: 47,
            stream: 7,
        };
        let round_tripped: Request =
            serde_json::from_value(serde_json::to_value(&stop).unwrap()).unwrap();
        assert_eq!(stop, round_tripped);
    }

    #[test]
    fn capture_measure_request_round_trips() {
        let json = json!({
            "v": 1, "id": 48, "op": "capture.measure",
            "capture": "capture://00000000-0000-0000-0000-000000000000",
            "channel": 0,
            "measurements": ["frequency", "vpp"]
        });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::CaptureMeasure {
                v: 1,
                id: 48,
                capture: "capture://00000000-0000-0000-0000-000000000000".into(),
                channel: 0,
                measurements: vec![MeasurementKind::Frequency, MeasurementKind::Vpp],
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
        assert_eq!(req.id(), 48);
    }

    #[test]
    fn measurement_result_serializes_untagged_ok_or_unavailable() {
        let ok = MeasurementResult::Ok {
            kind: MeasurementKind::Frequency,
            value: 1000.2,
            unit: "Hz".into(),
            note: None,
        };
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({ "kind": "frequency", "value": 1000.2, "unit": "Hz" })
        );

        let unavailable = MeasurementResult::Unavailable {
            kind: MeasurementKind::Duty,
            unavailable: "capture is shorter than one period".into(),
        };
        assert_eq!(
            serde_json::to_value(&unavailable).unwrap(),
            json!({ "kind": "duty", "unavailable": "capture is shorter than one period" })
        );

        // A note rides along only when present (B-39/B-40/B-41).
        let noted = MeasurementResult::Ok {
            kind: MeasurementKind::Vmax,
            value: 1.0,
            unit: "V".into(),
            note: Some("uncalibrated".into()),
        };
        assert_eq!(
            serde_json::to_value(&noted).unwrap(),
            json!({ "kind": "vmax", "value": 1.0, "unit": "V", "note": "uncalibrated" })
        );

        // Both round-trip back through the untagged enum unambiguously.
        for result in [ok, unavailable, noted] {
            let value = serde_json::to_value(&result).unwrap();
            let back: MeasurementResult = serde_json::from_value(value).unwrap();
            assert_eq!(back, result);
        }
    }

    #[test]
    fn ok_response_matches_spec_shape_no_error_field() {
        let resp = Response::ok(
            42,
            ResponseResult::DeviceList(DeviceListResult { devices: vec![] }),
        );
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            value,
            json!({ "v": 1, "id": 42, "ok": true, "result": { "devices": [] } })
        );
        assert!(value.get("error").is_none());
    }

    #[test]
    fn err_response_matches_spec_example_no_result_field() {
        let resp = Response::err(
            44,
            "port_protected",
            "hub 3-3 port 4 is protected: feeds hub 3-3.4",
        );
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            value,
            json!({
                "v": 1, "id": 44, "ok": false,
                "error": { "code": "port_protected", "msg": "hub 3-3 port 4 is protected: feeds hub 3-3.4" }
            })
        );
        assert!(value.get("result").is_none());
    }

    #[test]
    fn response_round_trips_through_json_for_every_result_variant() {
        let results = vec![
            ResponseResult::DeviceList(DeviceListResult { devices: vec![] }),
            ResponseResult::UsbTree(UsbTreeResult { roots: vec![] }),
            ResponseResult::UsbPower(UsbPowerResult {
                hub: "3-3".into(),
                port: 1,
                on: true,
            }),
            ResponseResult::AcqConfigure(AcqConfigureResult {
                device: "3-3.4.2".into(),
                cfg: AcqConfig {
                    rate_hz: 1_000,
                    channels: vec![0],
                    samples: 100,
                    trigger: None,
                    ranges_v: vec![],
                    continuous: false,
                },
                calibration: vec![],
            }),
            ResponseResult::AcqStart(AcqStartResult {
                device: "3-3.4.2".into(),
                stream: 7,
                capture: "capture://00000000-0000-0000-0000-000000000000".into(),
            }),
            ResponseResult::AcqStop(AcqStopResult {}),
            ResponseResult::TermSources(TermSourcesResult {
                sources: vec![TerminalSourceInfo {
                    source: "log".into(),
                    kind: TerminalKind::Log,
                    label: "Server log".into(),
                    device: None,
                    available: true,
                    unavailable_reason: None,
                }],
            }),
            ResponseResult::TermOpen(TermOpenResult {
                session: 1,
                source: "log".into(),
                kind: TerminalKind::Log,
                label: "Server log".into(),
                state: TerminalState::Open,
                serial: None,
            }),
            // TermWriteResult and TermCloseResult are deliberately excluded
            // here: see `empty_result_variants_are_wire_indistinguishable`
            // below for why an exact round trip isn't a meaningful
            // assertion for them.
            ResponseResult::CaptureMeasure(CaptureMeasureResult {
                capture: "capture://00000000-0000-0000-0000-000000000000".into(),
                channel: 0,
                results: vec![
                    MeasurementResult::Ok {
                        kind: MeasurementKind::Frequency,
                        value: 1_000.2,
                        unit: "Hz".into(),
                        note: None,
                    },
                    MeasurementResult::Unavailable {
                        kind: MeasurementKind::Duty,
                        unavailable: "capture is shorter than one period".into(),
                    },
                ],
            }),
        ];

        for result in results {
            let resp = Response::ok(1, result.clone());
            let value = serde_json::to_value(&resp).unwrap();
            let back: Response = serde_json::from_value(value).unwrap();
            assert_eq!(back, resp, "round trip failed for {result:?}");
        }
    }

    /// `AcqStopResult`, `TermWriteResult` and `TermCloseResult` all serialize
    /// to `{}`: an untagged enum has no discriminant, so once more than one
    /// variant is content-less, which *variant* a generic deserialize
    /// reconstructs is decided by declaration order, not by which one was
    /// originally sent. That's harmless in practice — SPEC §3.1: the caller
    /// already knows which result to expect from the request's own `op`,
    /// tracked via `id`, and never deserializes a `ResponseResult` blind —
    /// but it does mean an *exact* round trip isn't a meaningful assertion
    /// for these three, unlike every other variant, which carries enough of
    /// its own shape (or at least a field) to be recovered unambiguously.
    #[test]
    fn empty_result_variants_are_wire_indistinguishable_but_still_valid_json() {
        for result in [
            ResponseResult::AcqStop(AcqStopResult {}),
            ResponseResult::TermWrite(TermWriteResult {}),
            ResponseResult::TermClose(TermCloseResult {}),
        ] {
            let resp = Response::ok(1, result);
            let value = serde_json::to_value(&resp).unwrap();
            assert_eq!(value["result"], json!({}));
            let back: Response = serde_json::from_value(value).unwrap();
            assert!(back.ok);
            assert_eq!(back.id, 1);
        }
    }

    #[test]
    fn device_added_event_has_no_id_field() {
        let event = Event::DeviceAdded {
            v: 1,
            device: DeviceInfo {
                id: "3-3.4.2".into(),
                vid: 0x1d50,
                pid: 0x608e,
                label: None,
                state: DeviceState::Ready,
                capabilities: vec![],
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["event"], "device.added");
        assert!(value.get("id").is_none());
        assert!(value.get("label").is_none());
    }

    #[test]
    fn device_state_event_matches_spec_example() {
        let json =
            json!({ "v": 1, "event": "device.state", "device": "3-3.4.2", "state": "flashing" });
        let event: Event = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            event,
            Event::DeviceState {
                v: 1,
                device: "3-3.4.2".into(),
                state: DeviceState::Flashing,
                msg: None,
            }
        );
        assert_eq!(serde_json::to_value(&event).unwrap(), json);
    }

    #[test]
    fn acq_state_event_matches_spec_example() {
        let json = json!({ "v": 1, "event": "acq.state", "stream": 7, "state": "armed" });
        let event: Event = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            event,
            Event::AcqState {
                v: 1,
                stream: 7,
                state: AcqState::Armed,
                msg: None,
            }
        );
        assert_eq!(serde_json::to_value(&event).unwrap(), json);
    }

    #[test]
    fn term_sources_request_round_trips() {
        let json = json!({ "v": 1, "id": 50, "op": "term.sources" });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(req, Request::TermSources { v: 1, id: 50 });
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn term_open_request_with_serial_settings_round_trips() {
        let json = json!({
            "v": 1, "id": 51, "op": "term.open", "source": "serial:3-3.4.4:1.1",
            "serial": { "baud": 115200, "data_bits": 8, "parity": "none", "stop_bits": "one" }
        });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::TermOpen {
                v: 1,
                id: 51,
                source: "serial:3-3.4.4:1.1".into(),
                serial: Some(SerialSettings {
                    baud: 115_200,
                    data_bits: 8,
                    parity: crate::Parity::None,
                    stop_bits: crate::StopBits::One,
                }),
                decode: None,
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn term_open_request_without_settings_omits_the_field() {
        let json = json!({ "v": 1, "id": 52, "op": "term.open", "source": "log" });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::TermOpen {
                v: 1,
                id: 52,
                source: "log".into(),
                serial: None,
                decode: None,
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn term_write_request_round_trips_with_plain_text() {
        let json = json!({ "v": 1, "id": 53, "op": "term.write", "session": 1, "data": "hi\n" });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::TermWrite {
                v: 1,
                id: 53,
                session: 1,
                data: Some("hi\n".into()),
                data_b64: None,
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
        assert_eq!(req.id(), 53);
    }

    #[test]
    fn term_close_request_round_trips() {
        let json = json!({ "v": 1, "id": 54, "op": "term.close", "session": 1 });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::TermClose {
                v: 1,
                id: 54,
                session: 1
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn term_state_event_matches_spec_shape() {
        let json = json!({ "v": 1, "event": "term.state", "session": 1, "state": "waiting" });
        let event: Event = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            event,
            Event::TermState {
                v: 1,
                session: 1,
                state: TerminalState::Waiting,
                msg: None,
            }
        );
        assert_eq!(serde_json::to_value(&event).unwrap(), json);
    }

    #[test]
    fn term_sources_changed_event_carries_no_list() {
        let json = json!({ "v": 1, "event": "term.sources" });
        let event: Event = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(event, Event::TermSourcesChanged { v: 1 });
        assert_eq!(serde_json::to_value(&event).unwrap(), json);
    }

    #[test]
    fn device_removed_event_round_trips() {
        let event = Event::DeviceRemoved {
            v: 1,
            device: "3-3.4.2".into(),
        };
        let back: Event = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        assert_eq!(event, back);
    }
}
