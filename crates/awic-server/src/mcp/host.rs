//! Translates one MCP tool call into the existing wire-protocol
//! [`awic_proto::Request`] and calls
//! [`dispatch::handle_request_with_origin`] — exactly the pattern
//! `crate::scpi::ServerHost` uses (see that module's doc comment), applied
//! to MCP instead of SCPI. This is what makes SPECIFICATION.md §8.4 true
//! rather than aspirational: port protection, soft limits, and the mutation
//! log all live in `dispatch.rs`/`awic-core`, so a tool call cannot bypass
//! them even by accident — `McpHost` has no other way to reach `AppState`
//! for a mutating operation.
//!
//! One [`McpHost`] backs one MCP session (SPEC §8.1's streamable HTTP: a
//! session begins at `initialize` and ends when the client stops using it or
//! calls `DELETE`). It owns:
//! - a discard-almost-everything drain of its own `WsOut` queue, exactly
//!   like `scpi::spawn_drain` — a `capture`/`start_recording` tool call runs
//!   the same `acq.start`/`rec.start` path a WebSocket client would, sample
//!   frames and all, and nothing here ever forwards those on (SPEC §8.3:
//!   "no tool returns bulk sample data"). The one exception is `0xA7`
//!   terminal-output frames, which are decoded and appended to the relevant
//!   [`SerialBuffer`] instead of being dropped — that's how `serial_read`/
//!   `serial_expect` see UART output at all.
//! - per-session serial buffers, keyed by the `term.open` session id.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex as AsyncMutex};

use awic_core::CaptureId;
use awic_proto::{
    AcqState, ChannelMapping, DecoderId, Event, MeasurementKind, MeasurementResult, OptionSetting,
    Request, ResponseResult, SerialSettings, Trigger,
};

use crate::dispatch::{self, TermSubscriptions};
use crate::mcp::serial_buffer::SerialBuffer;
use crate::state::AppState;
use crate::ws::WsOut;

const OUT_QUEUE_CAPACITY: usize = 64;

/// A tool-call failure, carrying the same `{code, msg}` shape every other
/// transport surfaces (SPEC §3.1) — `mcp::tools` turns this into an MCP
/// `isError: true` result with the server's own actionable message, never a
/// generic "something went wrong".
#[derive(Debug, Clone)]
pub struct McpToolError {
    pub code: String,
    pub msg: String,
}

impl McpToolError {
    pub fn new(code: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            msg: msg.into(),
        }
    }
}

fn internal_shape_error() -> McpToolError {
    McpToolError::new(
        "internal_error",
        "dispatch returned an unexpected response shape",
    )
}

pub struct McpHost {
    state: AppState,
    out_tx: mpsc::Sender<WsOut>,
    conn_id: u64,
    origin: String,
    term_subs: TermSubscriptions,
    serial_buffers: Arc<AsyncMutex<HashMap<u16, Arc<AsyncMutex<SerialBuffer>>>>>,
    /// `http://<Host>` the client reached `/mcp` at, for the `url` of a
    /// rendered view (F-09); unset when the request named no host.
    base_url: std::sync::OnceLock<String>,
}

impl McpHost {
    /// `origin` is logged with every mutation (SPEC §8.4/§12) —
    /// `"mcp <client>"`, where `<client>` is the MCP `clientInfo.name` given
    /// at `initialize`, or the peer address if the client never named
    /// itself.
    pub fn new(state: AppState, origin: String) -> Arc<Self> {
        let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE_CAPACITY);
        let conn_id = state.recorder().new_connection();
        let serial_buffers = Arc::new(AsyncMutex::new(HashMap::new()));
        spawn_drain(out_rx, serial_buffers.clone());
        Arc::new(Self {
            state,
            out_tx,
            conn_id,
            origin,
            term_subs: Arc::new(AsyncMutex::new(HashMap::new())),
            serial_buffers,
            base_url: std::sync::OnceLock::new(),
        })
    }

    /// Records where the client reached this server, once (F-09).
    pub fn set_base_url(&self, url: String) {
        let _ = self.base_url.set(url);
    }

    /// `http://<host:port>`, or `None` when the client's request named no
    /// `Host`.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.get().map(String::as_str)
    }

    async fn call(&self, request: Request) -> Result<ResponseResult, McpToolError> {
        let response = dispatch::handle_request_with_origin(
            &self.state,
            &self.out_tx,
            self.conn_id,
            request,
            &self.origin,
            &self.term_subs,
        )
        .await;
        if response.ok {
            response.result.ok_or_else(internal_shape_error)
        } else {
            let err = response.error.unwrap_or_else(|| awic_proto::ErrorInfo {
                code: "internal_error".into(),
                msg: "dispatch reported failure with no error detail".into(),
            });
            Err(McpToolError::new(err.code, err.msg))
        }
    }

    // -- discovery ----------------------------------------------------

    pub async fn list_instruments(&self) -> Result<awic_proto::DeviceListResult, McpToolError> {
        match self.call(Request::DeviceList { v: 1, id: 0 }).await? {
            ResponseResult::DeviceList(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn usb_tree(&self) -> Result<awic_proto::UsbTreeResult, McpToolError> {
        match self.call(Request::UsbTree { v: 1, id: 0 }).await? {
            ResponseResult::UsbTree(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    // -- USB ports ------------------------------------------------------

    pub async fn set_usb_power(
        &self,
        hub: String,
        port: u8,
        on: bool,
    ) -> Result<awic_proto::UsbPowerResult, McpToolError> {
        match self
            .call(Request::UsbPower {
                v: 1,
                id: 0,
                hub,
                port,
                on,
            })
            .await?
        {
            ResponseResult::UsbPower(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn set_usb_port_protected(
        &self,
        hub: String,
        port: u8,
        protected: Option<bool>,
    ) -> Result<awic_proto::UsbProtectResult, McpToolError> {
        match self
            .call(Request::UsbProtect {
                v: 1,
                id: 0,
                hub,
                port,
                protected,
            })
            .await?
        {
            ResponseResult::UsbProtect(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    /// A conservative default for `capture`'s `timeout_s` when the caller
    /// doesn't give one: twice the configured acquisition's own expected
    /// duration (`samples / rate_hz`) plus a fixed margin, floored at 5s and
    /// capped at 60s so a misconfigured or never-configured device can't
    /// hang a tool call indefinitely.
    pub async fn estimated_capture_timeout(&self, device: &str) -> f64 {
        let expected = self
            .state
            .get_config(device)
            .await
            .map(|effective| effective.cfg.samples as f64 / effective.cfg.rate_hz.max(1) as f64)
            .unwrap_or(0.0);
        (expected * 2.0 + 2.0).clamp(5.0, 60.0)
    }

    // -- captures ---------------------------------------------------------

    pub async fn configure_acquisition(
        &self,
        device: String,
        rate_hz: u64,
        channels: Vec<u8>,
        samples: u64,
        trigger: Option<Trigger>,
        ranges_v: Vec<f32>,
    ) -> Result<awic_proto::AcqConfigureResult, McpToolError> {
        let cfg = awic_proto::AcqConfig {
            rate_hz,
            channels,
            samples,
            trigger,
            ranges_v,
            continuous: false,
        };
        match self
            .call(Request::AcqConfigure {
                v: 1,
                id: 0,
                device,
                cfg,
            })
            .await?
        {
            ResponseResult::AcqConfigure(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    /// A stored capture's sample rate, for turning sample numbers into the
    /// times the window parameters take (issue #32).
    pub fn capture_rate_hz(&self, capture: &str) -> Result<u64, McpToolError> {
        let capture_id: CaptureId = capture
            .parse()
            .map_err(|err| McpToolError::new("invalid_capture_id", format!("{err}")))?;
        let store = self.state.capture_store();
        let stored = store.get(&capture_id).ok_or_else(|| {
            let err = store.missing_error(&capture_id, capture);
            McpToolError::new(crate::error::core_error_code(&err), err.to_string())
        })?;
        Ok(stored.meta.rate_hz.max(1))
    }

    /// Starts a finite acquisition on an already-configured device and
    /// blocks until it completes (or `timeout_s` elapses), then returns a
    /// **summary only** (SPEC §8.3): the capture handle plus one scalar
    /// measurement batch per acquired channel, never samples.
    pub async fn capture(
        &self,
        device: String,
        timeout_s: f64,
    ) -> Result<CaptureSummary, McpToolError> {
        let mut events = self.state.event_bus().subscribe();

        let (stream, capture) = match self
            .call(Request::AcqStart {
                v: 1,
                id: 0,
                device: device.clone(),
            })
            .await?
        {
            ResponseResult::AcqStart(r) => (r.stream, r.capture),
            _ => return Err(internal_shape_error()),
        };

        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(timeout_s.max(0.1));
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // A software trigger still waiting: stop it, so the device is
                // free, and say what the trigger channel did instead (B-32).
                if let Some(watch) = self.state.trigger_watch(stream).await {
                    if !watch.fired() {
                        let _ = self
                            .call(Request::AcqStop {
                                v: 1,
                                id: 0,
                                stream,
                            })
                            .await;
                        return Err(McpToolError::new(
                            "trigger_not_fired",
                            format!(
                                "{} (waited {timeout_s:.1} s). Nothing was captured and the \
                                 acquisition is stopped. {}",
                                watch.not_fired_reason(),
                                watch.advice()
                            ),
                        ));
                    }
                }
                return Err(McpToolError::new(
                    "capture_timeout",
                    format!(
                        "device {device:?} did not finish acquiring within {timeout_s:.1}s; \
                         capture {capture} may still be running"
                    ),
                ));
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(Event::AcqState {
                    stream: s,
                    state,
                    msg,
                    ..
                })) if s == stream => match state {
                    AcqState::Stopped => break,
                    AcqState::Error => {
                        return Err(McpToolError::new(
                            "device_error",
                            msg.unwrap_or_else(|| "acquisition ended in error".into()),
                        ))
                    }
                    AcqState::Armed | AcqState::Running => continue,
                },
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err(McpToolError::new(
                        "internal_error",
                        "event bus closed while waiting for the capture to finish",
                    ))
                }
                Err(_) => continue, // loop re-checks the deadline
            }
        }

        let capture_id: CaptureId = capture
            .parse()
            .map_err(|err| McpToolError::new("invalid_capture_id", format!("{err}")))?;
        let stored = self.state.capture_store().get(&capture_id).ok_or_else(|| {
            let err = self
                .state
                .capture_store()
                .missing_error(&capture_id, &capture);
            let code = crate::error::core_error_code(&err);
            McpToolError::new(
                code,
                format!("{err} (it vanished before it could be summarized)"),
            )
        })?;

        let kinds: &[MeasurementKind] = match &stored.data {
            awic_core::CaptureData::Analog { .. } => &[
                MeasurementKind::Vpp,
                MeasurementKind::Vmin,
                MeasurementKind::Vmax,
                MeasurementKind::Vavg,
                MeasurementKind::Vrms,
                MeasurementKind::Frequency,
                MeasurementKind::Duty,
            ],
            awic_core::CaptureData::Digital { .. } => &[
                MeasurementKind::Frequency,
                MeasurementKind::Duty,
                MeasurementKind::PulseWidthHigh,
                MeasurementKind::PulseWidthLow,
                MeasurementKind::EdgeCount,
            ],
        };

        let mut channels = Vec::new();
        for &channel in &stored.meta.channels {
            let results = match self
                .call(Request::CaptureMeasure {
                    v: 1,
                    id: 0,
                    capture: capture.clone(),
                    channel,
                    measurements: kinds.to_vec(),
                })
                .await?
            {
                ResponseResult::CaptureMeasure(r) => r.results,
                _ => return Err(internal_shape_error()),
            };
            let constant = match &stored.data {
                awic_core::CaptureData::Digital {
                    packed,
                    transitions,
                } if channel < 8 => {
                    let first = packed.first().map(|b| (b >> channel) & 1 == 1);
                    let toggles = transitions
                        .windows(2)
                        .any(|w| ((w[0].channel_mask ^ w[1].channel_mask) >> channel) & 1 == 1);
                    first.filter(|_| !toggles)
                }
                _ => None,
            };
            channels.push(ChannelSummary {
                channel,
                results,
                constant,
            });
        }

        // Where the edge is, so a caller never has to assume (B-32).
        let triggered_at = stored.meta.trigger_position.map(|sample| TriggeredAt {
            sample,
            t_s: sample as f64 / stored.meta.rate_hz.max(1) as f64,
        });
        Ok(CaptureSummary {
            capture,
            device,
            rate_hz: stored.meta.rate_hz,
            channels,
            triggered_at,
        })
    }

    pub async fn read_capture(
        &self,
        capture: String,
        channel: u8,
        max_points: Option<u32>,
        t0_s: Option<f64>,
        t1_s: Option<f64>,
    ) -> Result<awic_proto::CaptureReadResult, McpToolError> {
        match self
            .call(Request::CaptureRead {
                v: 1,
                id: 0,
                capture,
                channel,
                max_points,
                t0_s,
                t1_s,
            })
            .await?
        {
            ResponseResult::CaptureRead(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn measure(
        &self,
        capture: String,
        channel: u8,
        measurements: Vec<MeasurementKind>,
    ) -> Result<Vec<MeasurementResult>, McpToolError> {
        match self
            .call(Request::CaptureMeasure {
                v: 1,
                id: 0,
                capture,
                channel,
                measurements,
            })
            .await?
        {
            ResponseResult::CaptureMeasure(r) => Ok(r.results),
            _ => Err(internal_shape_error()),
        }
    }

    /// `decode` (SPEC §6, §8.2, T21): decodes a protocol over `[t0_s, t1_s)`
    /// of either a finished `capture` or a live/stopped `rec` —
    /// exactly one of the two must be given. Bounded like every other
    /// bulk-shaped tool result (§8.3): `max_annotations` is clamped
    /// server-side, and the result says how many annotations were omitted.
    #[allow(clippy::too_many_arguments)]
    pub async fn decode(
        &self,
        capture: Option<String>,
        rec: Option<u16>,
        t0_s: Option<f64>,
        t1_s: Option<f64>,
        decoder: DecoderId,
        channels: Vec<ChannelMapping>,
        options: Vec<OptionSetting>,
        max_annotations: Option<u32>,
        max_level: Option<u8>,
        format: Option<awic_proto::DecodeFormat>,
        max_bytes: Option<u32>,
        fold: Option<bool>,
    ) -> Result<serde_json::Value, McpToolError> {
        match (capture, rec) {
            (Some(capture), None) => match self
                .call(Request::CaptureDecode {
                    v: 1,
                    id: 0,
                    capture,
                    decoder,
                    channels,
                    options,
                    t0_s,
                    t1_s,
                    max_annotations,
                    max_level,
                    format,
                    max_bytes,
                    fold,
                })
                .await?
            {
                ResponseResult::CaptureDecode(r) => Ok(serde_json::to_value(r).unwrap_or_default()),
                _ => Err(internal_shape_error()),
            },
            (None, Some(rec)) => match self
                .call(Request::RecDecode {
                    v: 1,
                    id: 0,
                    rec,
                    decoder,
                    channels,
                    options,
                    t0_s,
                    t1_s,
                    max_annotations,
                    max_level,
                    format,
                    max_bytes,
                    fold,
                })
                .await?
            {
                ResponseResult::RecDecode(r) => Ok(serde_json::to_value(r).unwrap_or_default()),
                _ => Err(internal_shape_error()),
            },
            _ => Err(McpToolError::new(
                "bad_arguments",
                "exactly one of `capture` or `rec` must be given",
            )),
        }
    }

    /// `survey` (live test F-04) over a capture or a recording range.
    pub async fn survey(
        &self,
        capture: Option<String>,
        rec: Option<u16>,
        t0_s: Option<f64>,
        t1_s: Option<f64>,
    ) -> Result<awic_proto::SurveyResult, McpToolError> {
        match self
            .call(Request::Survey {
                v: 1,
                id: 0,
                capture,
                rec,
                t0_s,
                t1_s,
            })
            .await?
        {
            ResponseResult::Survey(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    /// `capture.render` (SPEC §5.6). Everything optional is passed through as
    /// the caller gave it; the renderer fills in its own defaults, so there is
    /// no second set of them here to drift out of step.
    #[allow(clippy::too_many_arguments)]
    pub async fn render(
        &self,
        capture: String,
        kind: awic_proto::RenderKind,
        channels: Vec<u8>,
        width: Option<u32>,
        height: Option<u32>,
        t0_s: Option<f64>,
        t1_s: Option<f64>,
        period_s: Option<f64>,
        interval_kind: Option<awic_proto::IntervalKind>,
        tolerance: Option<f64>,
        title: Option<String>,
    ) -> Result<awic_proto::RenderResult, McpToolError> {
        match self
            .call(Request::CaptureRender {
                v: 1,
                id: 0,
                capture,
                kind,
                channels,
                width,
                height,
                t0_s,
                t1_s,
                period_s,
                interval_kind,
                tolerance,
                title,
            })
            .await?
        {
            ResponseResult::CaptureRender(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn usb_cycle(
        &self,
        hub: String,
        port: u8,
        off_ms: Option<u32>,
        wait_s: Option<f64>,
    ) -> Result<awic_proto::UsbCycleResult, McpToolError> {
        match self
            .call(Request::UsbCycle {
                v: 1,
                id: 0,
                hub,
                port,
                off_ms,
                wait_s,
            })
            .await?
        {
            ResponseResult::UsbCycle(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn usb_label(
        &self,
        hub: String,
        port: u8,
        label: Option<String>,
        dut: Option<bool>,
    ) -> Result<awic_proto::UsbLabelResult, McpToolError> {
        match self
            .call(Request::UsbLabel {
                v: 1,
                id: 0,
                hub,
                port,
                label,
                dut,
            })
            .await?
        {
            ResponseResult::UsbLabel(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn decode_list(&self) -> Result<awic_proto::DecodeListResult, McpToolError> {
        match self.call(Request::DecodeList { v: 1, id: 0 }).await? {
            ResponseResult::DecodeList(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    // -- recorder -----------------------------------------------------

    pub async fn start_recording(
        &self,
        device: String,
        rate_hz: u64,
        channels: Vec<u8>,
        ranges_v: Vec<f32>,
    ) -> Result<awic_proto::RecStartResult, McpToolError> {
        let cfg = awic_proto::AcqConfig {
            rate_hz,
            channels,
            samples: 0,
            trigger: None,
            ranges_v,
            continuous: true,
        };
        match self
            .call(Request::RecStart {
                v: 1,
                id: 0,
                device,
                cfg,
            })
            .await?
        {
            ResponseResult::RecStart(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn stop_recording(&self, rec: u16) -> Result<(), McpToolError> {
        self.call(Request::RecStop { v: 1, id: 0, rec }).await?;
        Ok(())
    }

    /// Stops `rec` if it is still live (`rec.stop` is idempotent), then
    /// deletes it: an agent asking to free a recording means both.
    pub async fn delete_recording(&self, rec: u16) -> Result<(), McpToolError> {
        self.call(Request::RecStop { v: 1, id: 0, rec }).await?;
        self.call(Request::RecDelete { v: 1, id: 0, rec }).await?;
        Ok(())
    }

    pub async fn list_recordings(&self) -> Result<awic_proto::RecListResult, McpToolError> {
        match self.call(Request::RecList { v: 1, id: 0 }).await? {
            ResponseResult::RecList(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn search_pattern(
        &self,
        rec: u16,
        pattern: String,
        t0_s: Option<f64>,
        t1_s: Option<f64>,
        max_matches: Option<u32>,
    ) -> Result<awic_proto::RecSearchResult, McpToolError> {
        match self
            .call(Request::RecSearch {
                v: 1,
                id: 0,
                rec,
                pattern,
                t0_s,
                t1_s,
                max_matches,
            })
            .await?
        {
            ResponseResult::RecSearch(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn wait_for_pattern(
        &self,
        rec: u16,
        pattern: String,
        timeout_ms: Option<u32>,
        from_s: Option<f64>,
    ) -> Result<awic_proto::RecWaitResult, McpToolError> {
        match self
            .call(Request::RecWait {
                v: 1,
                id: 0,
                rec,
                pattern,
                timeout_ms,
                from_s,
            })
            .await?
        {
            ResponseResult::RecWait(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    pub async fn snapshot_recording(
        &self,
        rec: u16,
        t0_s: f64,
        t1_s: f64,
    ) -> Result<awic_proto::RecSnapshotResult, McpToolError> {
        match self
            .call(Request::RecSnapshot {
                v: 1,
                id: 0,
                rec,
                t0_s,
                t1_s,
            })
            .await?
        {
            ResponseResult::RecSnapshot(r) => Ok(r),
            _ => Err(internal_shape_error()),
        }
    }

    // -- serial ---------------------------------------------------------

    pub async fn serial_list(&self) -> Result<Vec<awic_proto::TerminalSourceInfo>, McpToolError> {
        match self.call(Request::TermSources { v: 1, id: 0 }).await? {
            ResponseResult::TermSources(r) => Ok(r
                .sources
                .into_iter()
                .filter(|s| s.kind == awic_proto::TerminalKind::Serial)
                .collect()),
            _ => Err(internal_shape_error()),
        }
    }

    /// `serial: None` joins the port at whatever it is open at, or opens it
    /// at 115200 8N1 when it is not open yet.
    pub async fn serial_open(
        &self,
        source: String,
        serial: Option<SerialSettings>,
    ) -> Result<awic_proto::TermOpenResult, McpToolError> {
        let open = |serial: Option<SerialSettings>| Request::TermOpen {
            v: 1,
            id: 0,
            source: source.clone(),
            serial,
            decode: None,
        };
        let response = match self.call(open(serial)).await {
            Err(err) if serial.is_none() && err.code == "serial_settings_required" => {
                self.call(open(Some(SerialSettings {
                    baud: 115_200,
                    data_bits: 8,
                    parity: awic_proto::Parity::None,
                    stop_bits: awic_proto::StopBits::One,
                })))
                .await?
            }
            other => other?,
        };
        let result = match response {
            ResponseResult::TermOpen(r) => r,
            _ => return Err(internal_shape_error()),
        };
        self.serial_buffers
            .lock()
            .await
            .entry(result.session)
            .or_insert_with(|| Arc::new(AsyncMutex::new(SerialBuffer::default())));
        Ok(result)
    }

    pub async fn serial_write(&self, session: u16, data: Vec<u8>) -> Result<(), McpToolError> {
        self.call(Request::TermWrite {
            v: 1,
            id: 0,
            session,
            data: None,
            data_b64: Some(base64_encode(&data)),
        })
        .await?;
        Ok(())
    }

    /// `serial_close` (live test B-21): leaves the session. The port itself
    /// closes once nobody else — a browser tab, another agent — is still
    /// attached to it, exactly as `term.close` does for a WebSocket client.
    /// Closing a session this MCP session never opened, or already closed,
    /// is not an error.
    pub async fn serial_close(&self, session: u16) -> Result<(), McpToolError> {
        self.call(Request::TermClose {
            v: 1,
            id: 0,
            session,
        })
        .await?;
        self.serial_buffers.lock().await.remove(&session);
        Ok(())
    }

    async fn serial_buffer(
        &self,
        session: u16,
    ) -> Result<Arc<AsyncMutex<SerialBuffer>>, McpToolError> {
        self.serial_buffers
            .lock()
            .await
            .get(&session)
            .cloned()
            .ok_or_else(|| {
                McpToolError::new(
                    "unknown_terminal_session",
                    format!("session {session} was not opened by serial_open in this MCP session"),
                )
            })
    }

    /// `serial_read`: drains up to `max_bytes` that arrived since the last
    /// read (SPEC §8.3/§11.2.1), reporting how many bytes were dropped
    /// because the buffer filled up first.
    pub async fn serial_read(
        &self,
        session: u16,
        max_bytes: usize,
    ) -> Result<SerialReadResult, McpToolError> {
        let buffer = self.serial_buffer(session).await?;
        let mut guard = buffer.lock().await;
        let (data, dropped, more_available) = guard.drain(max_bytes);
        Ok(SerialReadResult {
            data,
            dropped,
            more_available,
        })
    }

    /// `serial_expect`: polls the buffer (without draining it, so a later
    /// `serial_read` still sees the same bytes) until `pattern` matches or
    /// `timeout_ms` elapses — the text counterpart of `wait_for_pattern`
    /// (SPEC §11.2.1).
    pub async fn serial_expect(
        &self,
        session: u16,
        pattern: &regex::Regex,
        timeout_ms: u64,
    ) -> Result<Option<String>, McpToolError> {
        let buffer = self.serial_buffer(session).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            {
                let guard = buffer.lock().await;
                let text = String::from_utf8_lossy(&guard.peek_all()).into_owned();
                if let Some(m) = pattern.find(&text) {
                    return Ok(Some(m.as_str().to_string()));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(
                Duration::from_millis(25)
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()))
                    .max(Duration::from_millis(1)),
            )
            .await;
        }
    }
}

impl Drop for McpHost {
    /// Ends whatever `rec.view`s this session opened, exactly as `ws.rs`
    /// does on socket close and `scpi::ServerHost` does on connection drop —
    /// MCP tools never open a view themselves today, but the recorder
    /// tracks every connection id it hands out and expects this call.
    ///
    /// And leaves every terminal session it joined, as `ws.rs` does: before
    /// this, an agent's serial ports stayed open for the life of the server
    /// after its MCP session ended — by `DELETE /mcp` or by eviction — with
    /// the server still holding `ttyACM0`/`ttyACM1` (live test B-21). `Drop`
    /// cannot await, so the release runs as a task on the current runtime.
    fn drop(&mut self) {
        self.state.recorder().close_connection(self.conn_id);
        let (state, term_subs) = (self.state.clone(), self.term_subs.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for (session_id, subscriber_id) in term_subs.lock().await.drain() {
                    state.terminal().close(session_id, subscriber_id).await.ok();
                }
            });
        }
    }
}

pub struct ChannelSummary {
    pub channel: u8,
    pub results: Vec<MeasurementResult>,
    /// Digital channel that never toggled: its level throughout (live test
    /// B-11 — the level is what identifies an idle UART or I²C line).
    pub constant: Option<bool>,
}

pub struct CaptureSummary {
    pub capture: String,
    pub device: String,
    pub rate_hz: u64,
    pub channels: Vec<ChannelSummary>,
    /// The trigger point, when the capture was triggered.
    pub triggered_at: Option<TriggeredAt>,
}

pub struct TriggeredAt {
    /// Sample index of the edge within the capture.
    pub sample: u64,
    /// The same, in seconds from the capture's first sample.
    pub t_s: f64,
}

pub struct SerialReadResult {
    pub data: Vec<u8>,
    pub dropped: u64,
    pub more_available: bool,
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Drains this host's own `WsOut` queue. Mirrors `scpi::spawn_drain`
/// (releasing backpressure/in-flight bookkeeping so a tool-driven
/// acquisition can't stall its producer) with one addition: `0xA7` terminal
/// output frames are decoded and appended to the matching session's
/// [`SerialBuffer`] rather than discarded — that's the only way
/// `serial_read`/`serial_expect` ever see bytes.
fn spawn_drain(
    mut rx: mpsc::Receiver<WsOut>,
    serial_buffers: Arc<AsyncMutex<HashMap<u16, Arc<AsyncMutex<SerialBuffer>>>>>,
) {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                WsOut::Frame {
                    payload_len,
                    backpressure,
                    ..
                } => backpressure.release(payload_len),
                WsOut::ViewFrame { in_flight, .. } => in_flight.store(false, Ordering::Release),
                WsOut::Text(_) => {}
                WsOut::TermFrame(bytes) => {
                    if let Ok((header, payload)) = awic_proto::term_frame::decode_frame(&bytes) {
                        if let Some(buffer) = serial_buffers.lock().await.get(&header.session_id) {
                            buffer.lock().await.push(payload);
                        }
                    }
                }
            }
        }
    });
}
