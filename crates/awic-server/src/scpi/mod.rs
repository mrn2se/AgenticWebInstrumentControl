//! SCPI transport (SPECIFICATION.md §9, §11.2): [`ServerHost`] implements
//! `awic_scpi::ScpiHost` by translating each command into the existing
//! wire-protocol [`awic_proto::Request`]s and calling
//! [`dispatch::handle_request_with_origin`] — the same function `ws.rs`
//! calls for every WebSocket request. That is what makes SPEC §9's "this
//! transport is not privileged" true rather than aspirational: port
//! protection, soft limits, and the SPEC §12 mutation log all live in
//! `dispatch.rs`/`awic-core`, so this module cannot bypass them even by
//! accident — it has no other way to reach `AppState`.
//!
//! [`listener`] is the TCP front end (port `server.scpi_port`);
//! [`terminal_source`] is the `scpi` terminal source (SPEC §11.2) — both
//! build one [`ServerHost`] + `awic_scpi::Engine` per connection/session and
//! feed it lines of text.

pub mod listener;
pub mod terminal_source;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex};

use awic_proto::{
    AcqConfig, AcqState, Event, MeasurementResult, RecWaitResult, Request, ResponseResult,
    Slope as WireSlope, Trigger,
};
use awic_scpi::{AcqSettings, HostError, Measurement, PortLock, RecSummary, ScpiHost, Slope};

use crate::dispatch::{self, TermSubscriptions};
use crate::state::AppState;
use crate::ws::WsOut;

/// Bound on the per-connection outbound queue [`dispatch::handle_request_with_origin`]
/// writes into. Nothing on the SCPI side ever reads sample/view frames back
/// out (see [`spawn_drain`]), so this only needs to be large enough that a
/// burst of control responses/events never blocks a dispatch call.
const OUT_QUEUE_CAPACITY: usize = 64;

/// Turns one `awic-core`/`awic-server` error code (SPEC §3.1's `{code, msg}`
/// shape) into a stable positive SCPI error code (SPEC §9: "device-specific
/// positive codes ... carrying the server's message"). Unlisted codes still
/// get a message the user can read — only the *number* is generic.
fn scpi_code_for(code: &str) -> i32 {
    match code {
        "port_protected" => 1,
        "soft_limit_exceeded" => 2,
        "device_not_found" => 3,
        "capture_not_found" => 4,
        "channel_not_found" => 5,
        "device_error" => 6,
        "invalid_config" => 7,
        "not_implemented" => 8,
        "stream_not_found" => 9,
        "device_busy" => 10,
        "device_not_configured" => 11,
        "continuous_needs_recorder" => 12,
        "invalid_capture_id" => 13,
        "rec_not_found" => 14,
        "invalid_pattern" => 15,
        "range_evicted" => 16,
        "empty_range" => 17,
        "extract_too_large" => 18,
        "internal_error" => 19,
        "invalid_device" => 20,
        "measurement_unavailable" => 21,
        "not_digital" => 22,
        "invalid_decoder_config" => 23,
        "capture_timeout" => 24,
        "capture_evicted" => 25,
        _ => 99,
    }
}

fn internal_shape_error() -> HostError {
    HostError::new(
        900,
        "internal error: dispatch returned an unexpected response shape",
    )
}

/// Drains a `ServerHost`'s own `WsOut` queue and discards everything —
/// **this is where "no bulk sample data over SCPI" (SPEC §9) actually
/// happens**: `ACQ:STAR` runs the identical `acq.start` path a WebSocket
/// client would, sample frames and all, but nothing on the SCPI side ever
/// reads them back out of this channel, so they never reach a `nc`/Python
/// socket. Binary frames still release their backpressure/in-flight
/// bookkeeping exactly as a real writer would (`ws.rs`'s own writer loop),
/// so a SCPI-driven acquisition can't stall the producer waiting for acks
/// nobody will ever send.
fn spawn_drain(mut rx: mpsc::Receiver<WsOut>) {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                WsOut::Frame {
                    payload_len,
                    backpressure,
                    ..
                } => backpressure.release(payload_len),
                WsOut::ViewFrame { in_flight, .. } => {
                    in_flight.store(false, std::sync::atomic::Ordering::Release)
                }
                WsOut::Text(_) | WsOut::TermFrame(_) => {}
            }
        }
    });
}

/// One SCPI connection's link back into the server: its own connection id
/// (recorder views/bookkeeping, SPEC §3.4), origin label (SPEC §12), and
/// discard-everything output queue. Implements [`ScpiHost`] by building a
/// [`Request`] and calling [`dispatch::handle_request_with_origin`] for
/// every operation — see the module doc for why that matters.
pub struct ServerHost {
    state: AppState,
    out_tx: mpsc::Sender<WsOut>,
    conn_id: u64,
    origin: String,
    term_subs: TermSubscriptions,
}

impl ServerHost {
    /// `origin` is logged with every mutation (SPEC §12) — `"scpi <peer>"`
    /// for a TCP client, `"scpi console"` for the terminal source.
    pub fn new(state: AppState, origin: String) -> Self {
        let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE_CAPACITY);
        spawn_drain(out_rx);
        let conn_id = state.recorder().new_connection();
        Self {
            state,
            out_tx,
            conn_id,
            origin,
            term_subs: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    async fn call(&self, request: Request) -> Result<ResponseResult, HostError> {
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
            Err(HostError::new(scpi_code_for(&err.code), err.msg))
        }
    }
}

impl Drop for ServerHost {
    /// Ends whatever `rec.view`s this connection opened (SPEC §3.4: "views
    /// ... die with it") — SCPI never opens one itself, but the recorder
    /// tracks every connection id it hands out and expects this call
    /// eventually, exactly as `ws.rs` makes it on socket close.
    fn drop(&mut self) {
        self.state.recorder().close_connection(self.conn_id);
    }
}

/// Builds the `awic_proto::AcqConfig` the wire protocol expects from the
/// session's accumulated `ACQ:RATE`/`ACQ:CHAN`/`ACQ:SAMP`/`ACQ:TRIG:...`
/// settings, filling in defaults for whatever was never set — the same
/// spirit as `acq.configure` filling in a device's own defaults for an
/// omitted field. `continuous` is forced by the caller (`false` for
/// `ACQ:STAR`, `true` for `REC:STAR`, which ignores `samples` regardless).
fn to_acq_config(settings: &AcqSettings, continuous: bool) -> AcqConfig {
    let channels: Vec<u8> = settings
        .channels
        .clone()
        .unwrap_or_else(|| vec![0])
        .into_iter()
        .map(|c| c as u8)
        .collect();
    // `ACQ:RANGe` is one number for the whole acquisition, because SCPI
    // selects the channel with `ACQ:CHAN` rather than addressing per-channel
    // settings; the wire takes one range per selected channel, so it is
    // repeated. An unset range leaves `ranges_v` empty, which is what the
    // device reads as "your own default".
    let ranges_v = match settings.range_v {
        Some(v) => vec![v as f32; channels.len()],
        None => vec![],
    };
    let trigger = settings.trigger.and_then(|t| {
        t.source_channel.map(|channel| Trigger::Edge {
            channel: channel as u8,
            slope: match t.slope.unwrap_or(Slope::Rising) {
                Slope::Rising => WireSlope::Rising,
                Slope::Falling => WireSlope::Falling,
            },
            level: Some(t.level_v.unwrap_or(0.0) as f32),
        })
    });
    AcqConfig {
        rate_hz: settings.rate_hz.unwrap_or(1_000_000.0) as u64,
        channels,
        // A finite capture needs a sample count; a continuous recording
        // ignores it (`recorder::start` forces `continuous = true`
        // regardless of what's passed here), so 0 is the honest default.
        samples: if continuous {
            0
        } else {
            settings.samples.unwrap_or(1_000)
        },
        trigger,
        ranges_v,
        continuous,
    }
}

fn to_measurement_kind(kind: Measurement) -> awic_proto::MeasurementKind {
    use awic_proto::MeasurementKind as W;
    match kind {
        Measurement::Frequency => W::Frequency,
        Measurement::Period => W::Period,
        Measurement::Vpp => W::Vpp,
        Measurement::Vmin => W::Vmin,
        Measurement::Vmax => W::Vmax,
        Measurement::Vavg => W::Vavg,
        Measurement::Vrms => W::Vrms,
        Measurement::Duty => W::Duty,
    }
}

/// `bus-port.port...` the way every other part of this server renders a
/// [`awic_proto::UsbTreeNode`]'s identity (matches `DeviceId`'s own
/// `Display`, SPEC §2.3).
fn bus_path(bus: u8, port_path: &[u8]) -> String {
    if port_path.is_empty() {
        return bus.to_string();
    }
    let path = port_path
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(".");
    format!("{bus}-{path}")
}

/// One node's own record, without recursing — shared by [`render_usb_tree`]
/// (which recurses through each port's nested `device`) and nothing else,
/// so the two stay in sync.
fn render_node_line(node: &awic_proto::UsbTreeNode) -> String {
    let path = bus_path(node.bus, &node.port_path);
    let mut line = format!("{path} {:04x}:{:04x}", node.vid, node.pid);
    if let Some(product) = &node.product {
        line.push(' ');
        line.push_str(product);
    }
    if node.is_hub {
        line.push_str(" hub");
    }
    if node.is_instrument {
        line.push_str(" instrument");
    }
    for port in &node.ports {
        let power = match port.power {
            Some(true) => "on",
            Some(false) => "off",
            None => "?",
        };
        line.push_str(&format!(" port{}={power}", port.number));
        if port.protected {
            line.push('*');
        }
    }
    line
}

/// `USB:TREE?`: one record per node (SPEC §9: "compact ... with no bulk
/// data"), `; `-separated on the **one** physical line the TCP transport's
/// line-oriented contract requires (a literal `\n` here would silently
/// split one query's answer across two reads for any client that reads a
/// line at a time — the exact framing bug this joiner exists to avoid).
/// `UsbTreeNode`'s tree shape lives in `UsbPort::device` (each port's
/// attached child, `None` for an empty or switched-off port), so rendering
/// it means recursing through every port, not just walking the top-level
/// list.
fn render_usb_tree(tree: &awic_proto::UsbTreeResult) -> String {
    let mut records = Vec::new();
    for node in &tree.roots {
        render_node(node, &mut records);
    }
    records.join("; ")
}

fn render_node(node: &awic_proto::UsbTreeNode, records: &mut Vec<String>) {
    records.push(render_node_line(node));
    for port in &node.ports {
        if let Some(child) = &port.device {
            render_node(child, records);
        }
    }
}

/// Depth-first search for the node at bus path `hub`, from `tree.roots`
/// down through every port's nested `device` — the hub we want is rarely a
/// root node itself (SPEC §9's own example, `"3-3.4"`, is two hops down
/// from `"3-3"`).
fn find_node<'a>(
    tree: &'a awic_proto::UsbTreeResult,
    hub: &str,
) -> Option<&'a awic_proto::UsbTreeNode> {
    fn search<'a>(
        node: &'a awic_proto::UsbTreeNode,
        hub: &str,
    ) -> Option<&'a awic_proto::UsbTreeNode> {
        if bus_path(node.bus, &node.port_path) == hub {
            return Some(node);
        }
        node.ports
            .iter()
            .find_map(|p| p.device.as_deref().and_then(|child| search(child, hub)))
    }
    tree.roots.iter().find_map(|node| search(node, hub))
}

fn find_port_power(tree: &awic_proto::UsbTreeResult, hub: &str, port: u32) -> Option<bool> {
    find_node(tree, hub)
        .and_then(|node| node.ports.iter().find(|p| p.number as u32 == port))
        .and_then(|p| p.power)
}

fn to_u8_port(port: u32) -> Result<u8, HostError> {
    u8::try_from(port).map_err(|_| HostError::new(22, format!("port {port} is out of range")))
}

#[async_trait::async_trait]
impl ScpiHost for ServerHost {
    async fn idn(&self) -> String {
        format!("AWIC,awic,0,{}", env!("CARGO_PKG_VERSION"))
    }

    /// Bus paths, never labels: the id is what every other command takes,
    /// and what `INST:SEL?` echoes back. (A label is *also* accepted by
    /// `INST:SEL` — see there — but listing labels here would leave a script
    /// holding a name it cannot pass to `usb.*`, MCP, or the UI.)
    async fn instrument_catalog(&self) -> Vec<String> {
        match self.call(Request::DeviceList { v: 1, id: 0 }).await {
            Ok(ResponseResult::DeviceList(r)) => r.devices.into_iter().map(|d| d.id).collect(),
            _ => vec![],
        }
    }

    async fn select_instrument(&self, selector: &str) -> Result<String, HostError> {
        let ResponseResult::DeviceList(list) =
            self.call(Request::DeviceList { v: 1, id: 0 }).await?
        else {
            return Err(internal_shape_error());
        };
        // Bus path first (what `INST:CAT?` answers), then the model name a
        // script is more likely to have been written with — exactly, or as
        // an unambiguous part of it, case-insensitively ("scope"). A
        // refusal lists the catalog rather than leaving the caller to guess
        // (live test B-09).
        let needle = selector.trim().to_ascii_lowercase();
        if let Some(exact) = list.devices.iter().find(|d| {
            d.id.eq_ignore_ascii_case(selector)
                || d.label
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(selector))
        }) {
            return Ok(exact.id.clone());
        }
        let partial: Vec<&awic_proto::DeviceInfo> = list
            .devices
            .iter()
            .filter(|d| {
                !needle.is_empty()
                    && d.label
                        .as_deref()
                        .is_some_and(|l| l.to_ascii_lowercase().contains(&needle))
            })
            .collect();
        let catalog = list
            .devices
            .iter()
            .map(|d| match &d.label {
                Some(label) => format!("{} ({label})", d.id),
                None => d.id.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        match partial.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(HostError::new(
                -224,
                format!("no instrument matches {selector:?}; INST:CAT? lists: [{catalog}]"),
            )),
            many => Err(HostError::new(
                -224,
                format!(
                    "{selector:?} matches {} instruments ({}); use a bus path from INST:CAT?",
                    many.len(),
                    many.iter()
                        .map(|d| d.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    async fn usb_tree_text(&self) -> Result<String, HostError> {
        let ResponseResult::UsbTree(tree) = self.call(Request::UsbTree { v: 1, id: 0 }).await?
        else {
            return Err(internal_shape_error());
        };
        Ok(render_usb_tree(&tree))
    }

    async fn usb_port_power_set(&self, hub: &str, port: u32, on: bool) -> Result<(), HostError> {
        self.call(Request::UsbPower {
            v: 1,
            id: 0,
            hub: hub.to_string(),
            port: to_u8_port(port)?,
            on,
        })
        .await?;
        Ok(())
    }

    async fn usb_port_power_get(&self, hub: &str, port: u32) -> Result<bool, HostError> {
        let ResponseResult::UsbTree(tree) = self.call(Request::UsbTree { v: 1, id: 0 }).await?
        else {
            return Err(internal_shape_error());
        };
        find_port_power(&tree, hub, port)
            .ok_or_else(|| HostError::new(3, format!("no such port {hub}:{port}")))
    }

    async fn usb_port_lock(&self, hub: &str, port: u32, mode: PortLock) -> Result<(), HostError> {
        let protected = match mode {
            PortLock::Lock => Some(true),
            PortLock::Unlock => Some(false),
            PortLock::Auto => None,
        };
        self.call(Request::UsbProtect {
            v: 1,
            id: 0,
            hub: hub.to_string(),
            port: to_u8_port(port)?,
            protected,
        })
        .await?;
        Ok(())
    }

    /// Configures, starts, and **waits for the capture to finish** before
    /// answering its handle: SPEC §9's own sequence is `ACQ:STAR` then
    /// `MEAS:FREQ? CH1`, and answering at start made that `MEAS?` fail with
    /// "capture not found" — silently, as SCPI query errors are (live test,
    /// 2026-09-15). Bounded like MCP `capture`: twice the nominal duration
    /// plus 5 s, at most 60 s; a trigger that never fires ends in
    /// `capture_timeout` with the handle in the message.
    async fn acquire(&self, device: &str, settings: AcqSettings) -> Result<String, HostError> {
        let cfg = to_acq_config(&settings, false);
        let nominal_s = cfg.samples as f64 / cfg.rate_hz.max(1) as f64;
        self.call(Request::AcqConfigure {
            v: 1,
            id: 0,
            device: device.to_string(),
            cfg,
        })
        .await?;
        let mut events = self.state.event_bus().subscribe();
        let ResponseResult::AcqStart(result) = self
            .call(Request::AcqStart {
                v: 1,
                id: 0,
                device: device.to_string(),
            })
            .await?
        else {
            return Err(internal_shape_error());
        };

        let timeout = Duration::from_secs_f64((nominal_s * 2.0 + 5.0).min(60.0));
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(HostError::new(
                    scpi_code_for("capture_timeout"),
                    format!(
                        "acquisition did not finish within {:.0} s; {} may still be running",
                        timeout.as_secs_f64(),
                        result.capture
                    ),
                ));
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(Event::AcqState {
                    stream, state, msg, ..
                })) if stream == result.stream => match state {
                    AcqState::Stopped => return Ok(result.capture),
                    AcqState::Error => {
                        return Err(HostError::new(
                            scpi_code_for("device_error"),
                            msg.unwrap_or_else(|| "acquisition ended in error".into()),
                        ))
                    }
                    AcqState::Armed | AcqState::Running => continue,
                },
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(HostError::new(
                        scpi_code_for("internal_error"),
                        "event bus closed while waiting for the capture to finish",
                    ))
                }
            }
        }
    }

    async fn acquire_stop(&self, device: &str) -> Result<(), HostError> {
        let Some(stream) = self.state.stream_id_for_device(device).await else {
            return Err(HostError::new(
                9,
                format!("device {device:?} has no active acq.start stream"),
            ));
        };
        self.call(Request::AcqStop {
            v: 1,
            id: 0,
            stream,
        })
        .await?;
        Ok(())
    }

    async fn measure(
        &self,
        capture: &str,
        channel: u32,
        kind: Measurement,
    ) -> Result<(f64, String), HostError> {
        let ResponseResult::CaptureMeasure(result) = self
            .call(Request::CaptureMeasure {
                v: 1,
                id: 0,
                capture: capture.to_string(),
                channel: to_u8_port(channel)?,
                measurements: vec![to_measurement_kind(kind)],
            })
            .await?
        else {
            return Err(internal_shape_error());
        };
        match result.results.into_iter().next() {
            Some(MeasurementResult::Ok { value, unit, .. }) => Ok((value, unit)),
            Some(MeasurementResult::Unavailable { unavailable, .. }) => Err(HostError::new(
                scpi_code_for("measurement_unavailable"),
                unavailable,
            )),
            None => Err(internal_shape_error()),
        }
    }

    async fn rec_start(&self, device: &str, settings: AcqSettings) -> Result<u32, HostError> {
        let cfg = to_acq_config(&settings, true);
        let ResponseResult::RecStart(result) = self
            .call(Request::RecStart {
                v: 1,
                id: 0,
                device: device.to_string(),
                cfg,
            })
            .await?
        else {
            return Err(internal_shape_error());
        };
        Ok(result.rec as u32)
    }

    async fn rec_stop(&self, rec: u32) -> Result<(), HostError> {
        self.call(Request::RecStop {
            v: 1,
            id: 0,
            rec: rec as u16,
        })
        .await?;
        Ok(())
    }

    async fn rec_list(&self) -> Vec<RecSummary> {
        match self.call(Request::RecList { v: 1, id: 0 }).await {
            Ok(ResponseResult::RecList(r)) => r
                .recordings
                .into_iter()
                .map(|s| RecSummary {
                    rec: s.rec as u32,
                    device: s.device,
                    state: format!("{:?}", s.state).to_lowercase(),
                    rate_hz: s.rate_hz,
                })
                .collect(),
            _ => vec![],
        }
    }

    async fn rec_search(
        &self,
        rec: u32,
        pattern: &str,
        max_matches: Option<u32>,
    ) -> Result<Vec<f64>, HostError> {
        let ResponseResult::RecSearch(result) = self
            .call(Request::RecSearch {
                v: 1,
                id: 0,
                rec: rec as u16,
                pattern: pattern.to_string(),
                t0_s: None,
                t1_s: None,
                max_matches,
            })
            .await?
        else {
            return Err(internal_shape_error());
        };
        Ok(result.matches.into_iter().map(|m| m.t_s).collect())
    }

    async fn rec_wait(
        &self,
        rec: u32,
        pattern: &str,
        timeout_s: f64,
    ) -> Result<Option<f64>, HostError> {
        let timeout_ms = (timeout_s * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32;
        let result = self
            .call(Request::RecWait {
                v: 1,
                id: 0,
                rec: rec as u16,
                pattern: pattern.to_string(),
                timeout_ms: Some(timeout_ms),
                from_s: None,
            })
            .await?;
        match result {
            ResponseResult::RecWait(RecWaitResult::Match { r#match }) => Ok(Some(r#match.t_s)),
            ResponseResult::RecWait(RecWaitResult::Timeout { .. }) => Ok(None),
            _ => Err(internal_shape_error()),
        }
    }

    async fn rec_snapshot(&self, rec: u32, t0_s: f64, t1_s: f64) -> Result<String, HostError> {
        let ResponseResult::RecSnapshot(result) = self
            .call(Request::RecSnapshot {
                v: 1,
                id: 0,
                rec: rec as u16,
                t0_s,
                t1_s,
            })
            .await?
        else {
            return Err(internal_shape_error());
        };
        Ok(result.capture)
    }

    async fn decoder_list(&self) -> Vec<String> {
        crate::decode::decoder_list()
            .decoders
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    async fn decode_capture_text(
        &self,
        decoder: &str,
        channels: &[u32],
        capture: &str,
    ) -> Result<String, HostError> {
        let info = crate::decode::decoder_list()
            .decoders
            .into_iter()
            .find(|d| d.name.eq_ignore_ascii_case(decoder))
            .ok_or_else(|| {
                HostError::new(
                    scpi_code_for("invalid_decoder_config"),
                    format!("unknown decoder {decoder:?}; see DEC:LIST?"),
                )
            })?;
        // Positional: SPI's optional MOSI/MISO/CS mean a shorter channel
        // list is not an error, just fewer roles bound (see
        // `ScpiHost::decode_capture_text`'s doc comment). Channels arrive
        // 1-based (the `(@n)` convention `ACQ:CHAN`/`MEAS` also use).
        let mut mapping = Vec::with_capacity(channels.len());
        for (&role, &ch) in info.channels.iter().zip(channels.iter()) {
            let zero_based = ch.checked_sub(1).ok_or_else(|| {
                HostError::new(
                    scpi_code_for("invalid_decoder_config"),
                    format!("channel numbers are 1-based (@1..); got {ch}"),
                )
            })?;
            mapping.push(awic_proto::ChannelMapping {
                role,
                channel: to_u8_port(zero_based)?,
                minus: None,
            });
        }
        let ResponseResult::CaptureDecode(result) = self
            .call(Request::CaptureDecode {
                v: 1,
                id: 0,
                capture: capture.to_string(),
                decoder: info.id,
                channels: mapping,
                options: vec![],
                t0_s: None,
                t1_s: None,
                max_annotations: None,
                max_level: None,
                format: None,
                max_bytes: None,
                fold: None,
            })
            .await?
        else {
            return Err(internal_shape_error());
        };
        Ok(render_decode_text(&result))
    }
}

/// A compact, bounded one-line text rendering of a decode result (SPEC §9:
/// "no bulk data over SCPI") — each annotation's own already-formatted
/// `text` (`"0x41"`, `"S"`, `"0x50 W"`, ...), space-joined and capped, with
/// a trailing count of anything `capture.decode` itself already truncated.
fn render_decode_text(result: &awic_proto::CaptureDecodeResult) -> String {
    const MAX_CHARS: usize = 400;
    let mut out = String::new();
    let mut shown = 0usize;
    let annotations = result.annotations.as_deref().unwrap_or_default();
    for ann in annotations {
        let piece_len = ann.text.len() + usize::from(!out.is_empty());
        if out.len() + piece_len > MAX_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&ann.text);
        shown += 1;
    }
    let omitted = result.omitted + (annotations.len() - shown) as u64;
    if omitted > 0 {
        out.push_str(&format!(" (+{omitted} more)"));
    }
    out
}
