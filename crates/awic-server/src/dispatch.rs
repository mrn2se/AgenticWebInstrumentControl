//! Request dispatch (SPECIFICATION.md §3.1): turns one parsed [`Request`]
//! into the [`Response`] to send back, starting whatever background work
//! (opening a device, streaming samples) the operation implies.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex as AsyncMutex};

use awic_core::{measure_capture, read_capture_window, CaptureId, SampleSink};
use awic_proto::{
    AcqConfigureResult, AcqStartResult, AcqState, AcqStopResult, CaptureMeasureResult,
    CaptureReadResult, DeviceListResult, Event, Request, Response, ResponseResult, TermCloseResult,
    TermOpenResult, TermSourcesResult, TermWriteResult, TerminalKind, PROTOCOL_VERSION,
};

use crate::backpressure::{StreamBackpressure, DEFAULT_HIGH_WATER_BYTES};
use crate::error::core_error_response;
use crate::state::{AppState, StreamHandle};
use crate::stream::{forward_stream, CaptureRecorder};
use crate::terminal::TerminalError;
use crate::ws::WsOut;

/// One `/ws` connection's own record of which terminal sessions it has
/// joined, keyed by `session_id`, valued by the subscriber id
/// [`crate::terminal::SessionManager::open`] returned for *this*
/// connection's subscription — needed by `term.close` (which subscription
/// to drop) and by `ws.rs` on socket close, so "per-connection subscriptions
/// cleaned up when the socket closes" (task brief) actually happens: a
/// session is otherwise shared and outlives any one connection.
pub type TermSubscriptions = Arc<AsyncMutex<HashMap<u16, u64>>>;

/// Maps a [`TerminalError`] onto a stable wire error code (SPEC §3.1),
/// mirroring `error::core_error_response`'s role for `awic-core::CoreError`.
fn terminal_error_response(id: u32, err: TerminalError) -> Response {
    let code = match &err {
        TerminalError::UnknownSource(_) => "unknown_terminal_source",
        TerminalError::Unavailable(..) => "terminal_source_unavailable",
        TerminalError::SettingsRequired(_) => "serial_settings_required",
        TerminalError::UnknownSession(_) => "unknown_terminal_session",
        TerminalError::ReadOnly(_) => "terminal_session_read_only",
        TerminalError::BadWritePayload => "invalid_request",
        TerminalError::BadBase64(_) => "invalid_request",
        TerminalError::Busy(_) => "terminal_source_busy",
        TerminalError::SettingsConflict { .. } => "serial_settings_conflict",
        TerminalError::Io(_) => "terminal_io_error",
    };
    Response::err(id, code, err.to_string())
}

/// How many [`awic_core::SampleChunk`]s may be buffered between an
/// `Instrument`'s producer task and this connection's forwarder before the
/// producer itself blocks (SPECIFICATION.md §2.6: the driver-facing channel
/// is bounded and applies backpressure to the *driver*, independent of the
/// server-to-client backpressure policy in `crate::backpressure`).
const SAMPLE_CHANNEL_CAPACITY: usize = 64;

/// `capture.read` / MCP `read_capture` bounds (SPECIFICATION.md §8.3).
const DEFAULT_READ_CAPTURE_POINTS: u32 = 512;
const MAX_READ_CAPTURE_POINTS: u32 = 4096;

/// Port `port` of the hub whose bus path (`DeviceId`'s display form, e.g.
/// `"3-3.4"`) is `hub`, anywhere in the tree.
fn find_port<'a>(
    tree: &'a awic_proto::UsbTreeResult,
    hub: &str,
    port: u8,
) -> Option<&'a awic_proto::UsbPort> {
    fn walk<'a>(
        node: &'a awic_proto::UsbTreeNode,
        hub: &str,
        port: u8,
    ) -> Option<&'a awic_proto::UsbPort> {
        let path: Vec<String> = node.port_path.iter().map(u8::to_string).collect();
        if format!("{}-{}", node.bus, path.join(".")) == hub {
            return node.ports.iter().find(|p| p.number == port);
        }
        node.ports
            .iter()
            .filter_map(|p| p.device.as_deref())
            .find_map(|child| walk(child, hub, port))
    }
    tree.roots.iter().find_map(|root| walk(root, hub, port))
}

/// Total sample count of the capture along the time axis (i.e. per channel,
/// not the interleaved byte count) — used only to report `truncated`
/// honestly: whether decimation actually dropped points, or the whole
/// capture already fit in the requested `max_points`.
fn channel_sample_count(capture: &awic_core::Capture) -> usize {
    match &capture.data {
        awic_core::CaptureData::Analog {
            raw, sample_bits, ..
        } => {
            let bytes_per_sample = if *sample_bits <= 8 { 1 } else { 2 };
            let stride = capture.meta.channel_count.max(1) as usize;
            raw.len() / bytes_per_sample / stride.max(1)
        }
        awic_core::CaptureData::Digital { packed, .. } => packed.len(),
    }
}

/// Builds the [`CaptureRecorder`] `acq.start` hands to the forwarder, from
/// the device's most recently applied `acq.configure`. Returns `None` (and
/// logs) rather than failing the request if either the device id string
/// doesn't parse or no config was on file — recording the capture is best
/// effort, not a reason to refuse a start that the instrument itself
/// accepted.
async fn build_capture_recorder(
    state: &AppState,
    device: &str,
    capture_id: awic_core::CaptureId,
) -> Option<CaptureRecorder> {
    let device_id: awic_core::DeviceId = match device.parse() {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(device, %err, "not recording capture: device id did not parse");
            return None;
        }
    };
    let Some(effective) = state.get_config(device).await else {
        tracing::warn!(device, "not recording capture: no configuration on file");
        return None;
    };
    let cfg = &effective.cfg;
    // An edge trigger lands at the capture's midpoint: awic-fake places it
    // there, and the software trigger keeps `samples / 2` frames of history
    // (there being no pretrigger-position field on `Trigger`).
    let trigger_position = cfg.trigger.as_ref().map(|_| cfg.samples / 2);
    Some(CaptureRecorder {
        id: capture_id,
        device: device_id,
        rate_hz: cfg.rate_hz,
        trigger_position,
        calibration: effective.calibration.clone(),
        channels: cfg.channels.clone(),
        store: state.capture_store().clone(),
    })
}

/// How long `acq.stop` waits for a stopped stream to drain and file its
/// capture. Drivers bound their own `stop()`, so this is a backstop.
const STOP_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Stops one acquisition and waits for its forwarder to file the capture.
/// The handle must already have been removed from [`AppState`] — that is
/// what marks the end as requested rather than as the device giving up (see
/// [`run_acquisition_stream`]).
async fn stop_and_drain(state: &AppState, stream: u16, mut handle: StreamHandle) {
    {
        let mut guard = handle.instrument.lock().await;
        if let Err(err) = guard.stop().await {
            tracing::warn!(stream, %err, "instrument.stop() failed");
        }
    }
    // Never abort the forwarder outright: it files the capture when its
    // channel closes, and aborting it would discard the partial capture the
    // client already holds a `capture://` handle for. Stopping the
    // instrument closes that channel; wait for it.
    if tokio::time::timeout(STOP_DRAIN_TIMEOUT, &mut handle.forwarder)
        .await
        .is_err()
    {
        handle.forwarder.abort();
        tracing::error!(
            stream,
            "stream did not drain after stop; aborted, capture not filed"
        );
        state.event_bus().publish(Event::AcqState {
            v: PROTOCOL_VERSION,
            stream,
            state: AcqState::Error,
            msg: Some("stopped, but the stream did not drain; capture not saved".into()),
        });
    }
}

/// Runs a stream to its end, then reports how the acquisition ended — the
/// single place that happens, so a capture that completes on its own is
/// announced as surely as one the user stops. Before this, only `acq.stop`
/// announced an end: a capture that simply finished left clients showing
/// "running" forever.
async fn run_acquisition_stream(
    state: AppState,
    stream: u16,
    requested: u64,
    forward: impl std::future::Future<Output = u64>,
) {
    let received = forward.await;
    // `acq.stop` removes the handle before stopping; finding it still here
    // means the stream ended on its own.
    let by_request = state.remove_stream(stream).await.is_none();
    let (acq_state, msg) = if by_request {
        (
            AcqState::Stopped,
            Some(format!("stopped by request after {received} samples")),
        )
    } else if received >= requested {
        (AcqState::Stopped, Some("complete".to_string()))
    } else {
        // The device went quiet before delivering what was configured — e.g.
        // a stall the driver detected. Say so; a short capture reported as
        // complete would be a quiet lie.
        (
            AcqState::Error,
            Some(format!(
                "capture ended early: {received} of {requested} samples; the device stopped streaming"
            )),
        )
    };
    state.event_bus().publish(Event::AcqState {
        v: PROTOCOL_VERSION,
        stream,
        state: acq_state,
        msg,
    });
}

/// Dispatches one request without an origin to log against — used by every
/// existing test in this module, none of which exercise SPEC §12's
/// mutation-logging. Real traffic goes through
/// [`handle_request_with_origin`] via `ws.rs`, which has the connection's
/// peer address to pass.
pub async fn handle_request(
    state: &AppState,
    out_tx: &mpsc::Sender<WsOut>,
    conn_id: u64,
    request: Request,
) -> Response {
    let term_subs: TermSubscriptions = Arc::new(AsyncMutex::new(HashMap::new()));
    handle_request_with_origin(state, out_tx, conn_id, request, "unknown", &term_subs).await
}

/// Turns one parsed [`Request`] into the [`Response`] to send back, starting
/// whatever background work (opening a device, streaming samples, opening a
/// terminal session) the operation implies. `origin` identifies who asked —
/// `"ws <peer addr>"` today, SCPI/MCP transports' own label later — and is
/// logged at `info` for every mutating op (SPEC §12: "every mutation ... is
/// logged with origin"), which is exactly what makes those mutations show up
/// in the `log` terminal source. `term_subs` is this connection's own
/// terminal-session bookkeeping (see [`TermSubscriptions`]); `ws.rs` owns it
/// for the connection's lifetime and drains it when the socket closes.
pub async fn handle_request_with_origin(
    state: &AppState,
    out_tx: &mpsc::Sender<WsOut>,
    conn_id: u64,
    request: Request,
    origin: &str,
    term_subs: &TermSubscriptions,
) -> Response {
    match request {
        Request::DeviceList { id, .. } => Response::ok(
            id,
            ResponseResult::DeviceList(DeviceListResult {
                devices: state.registry().list(),
            }),
        ),

        Request::UsbTree { id, .. } => match state.usb().tree().await {
            Ok(result) => Response::ok(id, ResponseResult::UsbTree(result)),
            Err(err) => Response::err(id, err.code, err.msg),
        },

        Request::UsbPower {
            id, hub, port, on, ..
        } => {
            // Protection is checked inside `power`, against what is attached
            // right now — not against a snapshot taken here.
            match state.usb().power(&hub, port, on).await {
                Ok(result) => {
                    tracing::info!(origin, "usb.power hub={hub} port={port} on={on}");
                    Response::ok(id, ResponseResult::UsbPower(result))
                }
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::UsbProtect {
            id,
            hub,
            port,
            protected,
            ..
        } => match state.usb().protect(&hub, port, protected).await {
            Ok(result) => {
                tracing::info!(
                    origin,
                    "usb.protect hub={hub} port={port} protected={protected:?}"
                );
                // Every open UI re-reads the tree on usb.changed.
                state.event_bus().publish(Event::UsbChanged {
                    v: PROTOCOL_VERSION,
                    device: hub,
                    change: awic_proto::UsbChange::Protection,
                });
                Response::ok(id, ResponseResult::UsbProtect(result))
            }
            Err(err) => Response::err(id, err.code, err.msg),
        },

        Request::UsbCycle {
            id,
            hub,
            port,
            off_ms,
            wait_s,
            ..
        } => {
            let off_ms = off_ms.unwrap_or(1000).clamp(50, 30_000);
            let wait = std::time::Duration::from_secs_f64(wait_s.unwrap_or(5.0).clamp(0.0, 30.0));
            if let Err(err) = state.usb().power(&hub, port, false).await {
                return Response::err(id, err.code, err.msg);
            }
            tracing::info!(
                origin,
                "usb.cycle hub={hub} port={port} off for {off_ms} ms"
            );
            tokio::time::sleep(std::time::Duration::from_millis(u64::from(off_ms))).await;
            // The clock starts when power is *asked for*, not when the call
            // returns: switching on can itself block until the device has
            // re-enumerated, and timing from its return then reports a board
            // "back after 0.02 s" that in fact took 0.7 s (live test B-22).
            let on_at = std::time::Instant::now();
            if let Err(err) = state.usb().power(&hub, port, true).await {
                return Response::err(
                    id,
                    err.code,
                    format!("{} — the port was switched off and is still OFF", err.msg),
                );
            }
            // Wait for something to enumerate there again; the hot-plug
            // watcher keeps the tree fresh, so polling it is enough.
            let (mut device, mut enumerated_after_s, mut label, mut dut) =
                (None, None, None, false);
            loop {
                if let Ok(tree) = state.usb().tree().await {
                    if let Some(p) = find_port(&tree, &hub, port) {
                        label = p.label.clone();
                        dut = p.dut;
                        if let Some(d) = &p.device {
                            device = Some(d.clone());
                            enumerated_after_s = Some(on_at.elapsed().as_secs_f64());
                            break;
                        }
                    }
                }
                if on_at.elapsed() >= wait {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Response::ok(
                id,
                ResponseResult::UsbCycle(awic_proto::UsbCycleResult {
                    hub,
                    port,
                    off_ms,
                    device,
                    enumerated_after_s,
                    label,
                    dut,
                }),
            )
        }

        Request::UsbLabel {
            id,
            hub,
            port,
            label,
            dut,
            ..
        } => match state.usb().label(&hub, port, label, dut).await {
            Ok(result) => {
                tracing::info!(
                    origin,
                    "usb.label hub={hub} port={port} label={:?} dut={}",
                    result.label,
                    result.dut
                );
                state.event_bus().publish(Event::UsbChanged {
                    v: PROTOCOL_VERSION,
                    device: hub,
                    change: awic_proto::UsbChange::Protection,
                });
                Response::ok(id, ResponseResult::UsbLabel(result))
            }
            Err(err) => Response::err(id, err.code, err.msg),
        },

        Request::AcqConfigure {
            id, device, cfg, ..
        } => {
            if state.recorder().is_recording(&device) {
                return Response::err(
                    id,
                    "device_busy",
                    state.recorder().device_busy_message(&device),
                );
            }
            let instrument = match state.ensure_open(&device).await {
                Ok(instrument) => instrument,
                Err(err) => return core_error_response(id, err),
            };
            let mut guard = instrument.lock().await;
            // A device that cannot trigger itself streams continuously and
            // the server finds the edge (B-32); the driver never sees the
            // trigger, and the effective config still reports it.
            let soft = cfg.trigger.clone().filter(|_| !guard.honours_trigger());
            if soft.is_some() && (cfg.continuous || cfg.samples == 0) {
                return Response::err(
                    id,
                    "invalid_params",
                    "a trigger needs a finite capture: set samples > 0 and leave continuous off",
                );
            }
            let driver_cfg = match &soft {
                Some(_) => awic_core::AcqConfig {
                    trigger: None,
                    continuous: true,
                    ..cfg.clone()
                },
                None => cfg.clone(),
            };
            let Ok(device_id) = device.parse::<awic_core::DeviceId>() else {
                return Response::err(
                    id,
                    "invalid_device",
                    format!("{device:?} is not a device id"),
                );
            };
            match state
                .configure_announced(&device_id, &mut guard, driver_cfg)
                .await
            {
                Ok(mut effective) => {
                    drop(guard);
                    // A device that triggers itself is held to the same
                    // rules: the channel must be acquired, and an analog
                    // trigger needs its level (B-96).
                    if let Some(trigger) = cfg.trigger.as_ref().filter(|_| soft.is_none()) {
                        if let Err(msg) = crate::soft_trigger::EdgeSpec::resolve(
                            trigger,
                            &effective.cfg.channels,
                            &effective.calibration,
                        ) {
                            return Response::err(id, "invalid_params", msg);
                        }
                    }
                    if let Some(trigger) = soft {
                        effective.cfg.continuous = false;
                        effective.cfg.samples = cfg.samples;
                        if let Err(msg) = crate::soft_trigger::EdgeSpec::resolve(
                            &trigger,
                            &effective.cfg.channels,
                            &effective.calibration,
                        ) {
                            return Response::err(id, "invalid_params", msg);
                        }
                        effective.cfg.trigger = Some(trigger);
                    }
                    state.set_config(&device, effective.clone()).await;
                    tracing::info!(
                        origin,
                        "acq.configure device={device} rate_hz={}",
                        effective.cfg.rate_hz
                    );
                    // Report what the device will do, not what was asked —
                    // they differ whenever the hardware rounds a rate or
                    // snaps a range.
                    Response::ok(
                        id,
                        ResponseResult::AcqConfigure(AcqConfigureResult {
                            device,
                            cfg: effective.cfg,
                            calibration: effective.calibration,
                        }),
                    )
                }
                Err(err) => core_error_response(id, err),
            }
        }

        Request::AcqStart { id, device, .. } => {
            if state.recorder().is_recording(&device) {
                return Response::err(
                    id,
                    "device_busy",
                    state.recorder().device_busy_message(&device),
                );
            }
            let Some(instrument) = state.configured_instrument(&device).await else {
                return Response::err(
                    id,
                    "device_not_configured",
                    format!("device {device:?} has not been configured; call acq.configure first"),
                );
            };

            // A continuous acquisition (SPEC §2.3.2) streams until `stop()`
            // with no sample count to bound it: forwarding it through this
            // path would accumulate the entire stream into the finite
            // `capture.*` machinery's memory. Continuous acquisition is
            // driven by the recorder (§2.8), which calls `Instrument::start`
            // directly rather than going through `acq.start`.
            if state
                .get_config(&device)
                .await
                .is_some_and(|effective| effective.cfg.continuous)
            {
                return Response::err(
                    id,
                    "continuous_needs_recorder",
                    format!(
                        "device {device:?} is configured for continuous acquisition; \
                         use rec.start instead of acq.start (SPECIFICATION.md §2.8)"
                    ),
                );
            }

            // Pressing "Single" again while the last one is still running
            // is an ordinary thing to do. End that one properly first —
            // otherwise two acquisitions share the device's one bulk
            // endpoint, neither receives a whole stream, and the superseded
            // one is reported as "the device stopped streaming" when in fact
            // the user asked for a new capture (live test, 2026-09-16).
            if let Some((old_stream, handle)) = state.remove_stream_of_device(&device).await {
                tracing::info!(
                    origin,
                    "acq.start device={device} supersedes stream={old_stream}"
                );
                stop_and_drain(state, old_stream, handle).await;
            }

            let effective = state.get_config(&device).await;
            let (sink, rx) = SampleSink::channel(SAMPLE_CHANNEL_CAPACITY);
            let (capture_id, soft) = {
                let mut guard = instrument.lock().await;
                let soft = effective
                    .as_ref()
                    .and_then(|e| e.cfg.trigger.as_ref().map(|t| (t, e)))
                    .filter(|_| !guard.honours_trigger())
                    .and_then(|(t, e)| {
                        crate::soft_trigger::EdgeSpec::resolve(t, &e.cfg.channels, &e.calibration)
                            .ok()
                            .map(|spec| {
                                (crate::soft_trigger::TriggerWatch::new(spec), e.cfg.clone())
                            })
                    });
                let device_id = guard.id().clone();
                match state.start_announced(&device_id, &mut guard, sink).await {
                    Ok(capture_id) => (capture_id, soft),
                    Err(err) => return core_error_response(id, err),
                }
            };

            let stream_id = state.next_stream_id();
            // With a software trigger the driver feeds the trigger, and the
            // trigger feeds the forwarder only the window around the edge.
            let rx = match &soft {
                Some((watch, cfg)) => {
                    let (tx, trigger_rx) = mpsc::channel(SAMPLE_CHANNEL_CAPACITY);
                    tokio::spawn(crate::soft_trigger::run(
                        state.clone(),
                        stream_id,
                        instrument.clone(),
                        rx,
                        tx,
                        watch.clone(),
                        cfg.samples,
                        cfg.rate_hz,
                    ));
                    trigger_rx
                }
                None => rx,
            };
            let backpressure = Arc::new(StreamBackpressure::new(DEFAULT_HIGH_WATER_BYTES));
            let recorder = build_capture_recorder(state, &device, capture_id).await;
            let requested = state.get_config(&device).await.map_or(0, |e| e.cfg.samples);
            let forwarder = tokio::spawn(run_acquisition_stream(
                state.clone(),
                stream_id,
                requested,
                forward_stream(
                    rx,
                    stream_id,
                    out_tx.clone(),
                    backpressure.clone(),
                    recorder,
                ),
            ));
            state
                .insert_stream(
                    stream_id,
                    StreamHandle {
                        device: device.clone(),
                        instrument,
                        forwarder,
                        backpressure,
                        trigger: soft.as_ref().map(|(watch, _)| watch.clone()),
                    },
                )
                .await;

            // Armed until the software trigger fires; it then says Running.
            state.event_bus().publish(Event::AcqState {
                v: PROTOCOL_VERSION,
                stream: stream_id,
                state: if soft.is_some() {
                    AcqState::Armed
                } else {
                    AcqState::Running
                },
                msg: soft.as_ref().map(|_| "waiting for the trigger".to_string()),
            });
            tracing::info!(origin, "acq.start device={device} stream={stream_id}");

            Response::ok(
                id,
                ResponseResult::AcqStart(AcqStartResult {
                    device,
                    stream: stream_id,
                    capture: capture_id.to_string(),
                }),
            )
        }

        Request::AcqStop { id, stream, .. } => {
            // Removing the handle first is what marks this end as requested
            // (see `run_acquisition_stream`).
            let Some(handle) = state.remove_stream(stream).await else {
                return Response::err(id, "stream_not_found", format!("no active stream {stream}"));
            };
            tracing::info!(origin, "acq.stop stream={stream}");
            stop_and_drain(state, stream, handle).await;
            Response::ok(id, ResponseResult::AcqStop(AcqStopResult {}))
        }

        Request::CaptureMeasure {
            id,
            capture,
            channel,
            measurements,
            ..
        } => {
            let capture_id: CaptureId = match capture.parse() {
                Ok(id) => id,
                Err(err) => {
                    return Response::err(
                        id,
                        "invalid_capture_id",
                        format!("{capture:?} is not a valid capture handle: {err}"),
                    )
                }
            };
            let Some(stored) = state.capture_store().get(&capture_id) else {
                return core_error_response(
                    id,
                    state.capture_store().missing_error(&capture_id, &capture),
                );
            };
            match measure_capture(&stored, channel, &measurements) {
                Ok(results) => Response::ok(
                    id,
                    ResponseResult::CaptureMeasure(CaptureMeasureResult {
                        capture,
                        channel,
                        results,
                    }),
                ),
                Err(err) => core_error_response(id, err),
            }
        }

        Request::CaptureRead {
            id,
            capture,
            channel,
            max_points,
            t0_s,
            t1_s,
            ..
        } => {
            let capture_id: CaptureId = match capture.parse() {
                Ok(id) => id,
                Err(err) => {
                    return Response::err(
                        id,
                        "invalid_capture_id",
                        format!("{capture:?} is not a valid capture handle: {err}"),
                    )
                }
            };
            let Some(stored) = state.capture_store().get(&capture_id) else {
                return core_error_response(
                    id,
                    state.capture_store().missing_error(&capture_id, &capture),
                );
            };
            // §8.3's context-budget rule: 512 default, 4096 hard cap,
            // regardless of what was asked.
            let requested = max_points.unwrap_or(DEFAULT_READ_CAPTURE_POINTS);
            let points = requested.clamp(1, MAX_READ_CAPTURE_POINTS) as usize;
            let total_samples = channel_sample_count(&stored);
            let rate = stored.meta.rate_hz.max(1) as f64;
            let to_sample = |s: f64| ((s * rate).round().max(0.0) as usize).min(total_samples);
            let windowed = t0_s.is_some() || t1_s.is_some();
            let start = t0_s.map_or(0, to_sample);
            let end = t1_s.map_or(total_samples, to_sample).max(start);
            match read_capture_window(&stored, channel, points, start, end) {
                Ok(result) => Response::ok(
                    id,
                    ResponseResult::CaptureRead(CaptureReadResult {
                        capture,
                        channel,
                        rate_hz: stored.meta.rate_hz,
                        unit: result.unit,
                        truncated: result.min.len() < end - start,
                        min: result.min,
                        max: result.max,
                        t0_s: windowed.then(|| start as f64 / rate),
                        t1_s: windowed.then(|| end as f64 / rate),
                    }),
                ),
                Err(err) => core_error_response(id, err),
            }
        }

        Request::CaptureDecode {
            id,
            capture,
            decoder,
            channels,
            options,
            max_annotations,
            max_level,
            format,
            t0_s,
            t1_s,
            max_bytes,
            fold,
            ..
        } => {
            let capture_id: CaptureId = match capture.parse() {
                Ok(id) => id,
                Err(err) => {
                    return Response::err(
                        id,
                        "invalid_capture_id",
                        format!("{capture:?} is not a valid capture handle: {err}"),
                    )
                }
            };
            let Some(stored) = state.capture_store().get(&capture_id) else {
                return core_error_response(
                    id,
                    state.capture_store().missing_error(&capture_id, &capture),
                );
            };
            let spec = crate::decode::DecodeSpec {
                decoder,
                channels: &channels,
                options: &options,
                max_annotations,
                max_level,
                format,
                max_bytes,
                fold,
            };
            match crate::decode::decode_capture_range(&stored, &spec, t0_s, t1_s) {
                Ok(mut out) => {
                    let (annotations, transactions) = out.take_lists();
                    Response::ok(
                        id,
                        ResponseResult::CaptureDecode(awic_proto::CaptureDecodeResult {
                            capture,
                            decoder,
                            annotations,
                            transactions,
                            summary: out.summary,
                            truncated: out.truncated,
                            omitted: out.omitted,
                            detected_baud: out.detected_baud,
                            note: out.note,
                        }),
                    )
                }
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::RecDecode {
            id,
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
            ..
        } => {
            let Some(entry) = state.recorder().get(rec) else {
                let err = crate::recorder::RecOpError::not_found(rec);
                return Response::err(id, err.code, err.msg);
            };
            let spec = crate::decode::DecodeSpec {
                decoder,
                channels: &channels,
                options: &options,
                max_annotations,
                max_level,
                format,
                max_bytes,
                fold,
            };
            match crate::recorder::decode::decode(&entry, &spec, t0_s, t1_s) {
                Ok(result) => Response::ok(id, ResponseResult::RecDecode(result)),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::Survey {
            id,
            capture,
            rec,
            t0_s,
            t1_s,
            ..
        } => {
            let result = match (capture, rec) {
                (Some(capture), None) => {
                    let stored = match capture.parse::<CaptureId>() {
                        Ok(cid) => state.capture_store().get(&cid),
                        Err(err) => {
                            return Response::err(
                                id,
                                "invalid_capture_id",
                                format!("{capture:?} is not a valid capture handle: {err}"),
                            )
                        }
                    };
                    let Some(stored) = stored else {
                        let cid: CaptureId = capture.parse().expect("parsed above");
                        return core_error_response(
                            id,
                            state.capture_store().missing_error(&cid, &capture),
                        );
                    };
                    crate::survey::survey_capture(&stored, capture, t0_s, t1_s)
                }
                (None, Some(rec)) => match state.recorder().get(rec) {
                    Some(entry) => crate::recorder::survey::survey(&entry, t0_s, t1_s)
                        .map_err(|e| crate::decode::DecodeOpError::new(e.code, e.msg)),
                    None => {
                        let err = crate::recorder::RecOpError::not_found(rec);
                        return Response::err(id, err.code, err.msg);
                    }
                },
                _ => {
                    return Response::err(
                        id,
                        "bad_arguments",
                        "survey takes exactly one of `capture` or `rec`",
                    )
                }
            };
            match result {
                Ok(result) => Response::ok(id, ResponseResult::Survey(result)),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::CaptureRender {
            id,
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
            ..
        } => {
            let stored = match capture.parse::<CaptureId>() {
                Ok(cid) => state.capture_store().get(&cid),
                Err(err) => {
                    return Response::err(
                        id,
                        "invalid_capture_id",
                        format!("{capture:?} is not a valid capture handle: {err}"),
                    )
                }
            };
            let Some(stored) = stored else {
                let cid: CaptureId = capture.parse().expect("parsed above");
                return core_error_response(
                    id,
                    state.capture_store().missing_error(&cid, &capture),
                );
            };
            match crate::render::render(
                &stored,
                &capture,
                kind,
                crate::render::Options {
                    channels,
                    width,
                    height,
                    t0_s,
                    t1_s,
                    period_s,
                    interval_kind,
                    tolerance,
                    title,
                },
            ) {
                Ok(result) => Response::ok(id, ResponseResult::CaptureRender(result)),
                // The renderer refuses for reasons the caller can act on — a
                // constant channel, a period too short to fold — so the
                // message is the whole answer.
                Err(msg) => Response::err(id, "cannot_render", msg),
            }
        }

        Request::DecodeList { id, .. } => Response::ok(
            id,
            ResponseResult::DecodeList(crate::decode::decoder_list()),
        ),

        Request::RecStart {
            id, device, cfg, ..
        } => {
            let device_for_log = device.clone();
            match crate::recorder::start(state, device, cfg).await {
                Ok((rec, effective)) => {
                    tracing::info!(
                        origin,
                        "rec.start device={device_for_log} rec={rec} rate_hz={}",
                        effective.cfg.rate_hz
                    );
                    Response::ok(
                        id,
                        ResponseResult::RecStart(awic_proto::RecStartResult {
                            rec,
                            cfg: effective.cfg,
                            calibration: effective.calibration,
                        }),
                    )
                }
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::RecStop { id, rec, .. } => match crate::recorder::stop(state, rec).await {
            Ok(()) => {
                tracing::info!(origin, "rec.stop rec={rec}");
                Response::ok(id, ResponseResult::RecStop(awic_proto::RecAck {}))
            }
            Err(err) => Response::err(id, err.code, err.msg),
        },

        Request::RecDelete { id, rec, .. } => match crate::recorder::delete(state, rec) {
            Ok(()) => {
                tracing::info!(origin, "rec.delete rec={rec}");
                Response::ok(id, ResponseResult::RecDelete(awic_proto::RecAck {}))
            }
            Err(err) => Response::err(id, err.code, err.msg),
        },

        Request::RecList { id, .. } => {
            Response::ok(id, ResponseResult::RecList(state.recorder().list()))
        }

        Request::RecView {
            id,
            rec,
            follow_s,
            t0_s,
            t1_s,
            columns,
            fps,
            ..
        } => {
            let params = crate::recorder::views::ViewParams {
                follow_s,
                t0_s,
                t1_s,
                columns,
                fps: fps.unwrap_or(30).clamp(1, 60),
            };
            match crate::recorder::views::open(
                state.recorder(),
                conn_id,
                out_tx.clone(),
                rec,
                params,
            )
            .await
            {
                Ok(view) => Response::ok(
                    id,
                    ResponseResult::RecView(awic_proto::RecViewResult { view }),
                ),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::RecViewUpdate {
            id,
            view,
            follow_s,
            t0_s,
            t1_s,
            columns,
            fps,
            ..
        } => {
            let patch = crate::recorder::views::ViewUpdate {
                follow_s,
                t0_s,
                t1_s,
                columns,
                fps: fps.map(|f| f.clamp(1, 60)),
            };
            match crate::recorder::views::update(state.recorder(), conn_id, view, patch).await {
                Ok(()) => Response::ok(id, ResponseResult::RecViewUpdate(awic_proto::RecAck {})),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::RecViewClose { id, view, .. } => {
            match crate::recorder::views::close(state.recorder(), conn_id, view) {
                Ok(()) => Response::ok(id, ResponseResult::RecViewClose(awic_proto::RecAck {})),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::RecSearch {
            id,
            rec,
            pattern,
            t0_s,
            t1_s,
            max_matches,
            ..
        } => {
            let Some(entry) = state.recorder().get(rec) else {
                let err = crate::recorder::RecOpError::not_found(rec);
                return Response::err(id, err.code, err.msg);
            };
            match crate::recorder::search::search(&entry, &pattern, t0_s, t1_s, max_matches) {
                Ok(result) => Response::ok(id, ResponseResult::RecSearch(result)),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::RecWait {
            id,
            rec,
            pattern,
            timeout_ms,
            from_s,
            ..
        } => {
            match crate::recorder::wait::wait_for_pattern(state, rec, &pattern, timeout_ms, from_s)
                .await
            {
                Ok(result) => Response::ok(id, ResponseResult::RecWait(result)),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::RecSnapshot {
            id,
            rec,
            t0_s,
            t1_s,
            ..
        } => {
            let Some(entry) = state.recorder().get(rec) else {
                let err = crate::recorder::RecOpError::not_found(rec);
                return Response::err(id, err.code, err.msg);
            };
            match crate::recorder::snapshot::snapshot(state.recorder(), &entry, t0_s, t1_s) {
                Ok(result) => Response::ok(id, ResponseResult::RecSnapshot(result)),
                Err(err) => Response::err(id, err.code, err.msg),
            }
        }

        Request::McpStatus { id, .. } => {
            let config = state.config();
            Response::ok(
                id,
                ResponseResult::McpStatus(awic_proto::McpStatusResult {
                    endpoint_path: "/mcp".to_string(),
                    bind: config.server.bind.clone(),
                    auth_required: config.auth.token.is_some(),
                    server_name: "awic".to_string(),
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: crate::mcp::protocol::LATEST_PROTOCOL_VERSION.to_string(),
                    tool_count: crate::mcp::tools::tool_defs().len() as u32,
                    max_sessions: crate::mcp::route::MAX_SESSIONS as u32,
                    sessions: state.mcp_monitor().sessions(),
                    logging: state.mcp_monitor().logging(),
                }),
            )
        }

        Request::McpLog {
            id, after, limit, ..
        } => Response::ok(
            id,
            ResponseResult::McpLog(state.mcp_monitor().log(after, limit)),
        ),

        Request::McpImage { id, seq, index, .. } => match state.mcp_monitor().image(seq, index) {
            Ok(image) => Response::ok(id, ResponseResult::McpImage(image)),
            Err(msg) => Response::err(id, "not_found", msg),
        },

        Request::McpLogging {
            id,
            enabled,
            images,
            clear,
            ..
        } => {
            tracing::info!(
                origin,
                "mcp.logging enabled={enabled:?} images={images:?} clear={clear:?}"
            );
            Response::ok(
                id,
                ResponseResult::McpLogging(state.mcp_monitor().set_logging(
                    enabled,
                    images,
                    clear.unwrap_or(false),
                )),
            )
        }

        Request::TermSources { id, .. } => Response::ok(
            id,
            ResponseResult::TermSources(TermSourcesResult {
                sources: state.terminal().sources(),
            }),
        ),

        Request::TermOpen {
            id,
            source,
            serial,
            decode,
            ..
        } => {
            // `decode`'s fixed picker id ("decode") always resolves to a
            // fresh, never-reused session id (SessionManager's
            // `mint_fresh_id`) — every other source opens exactly the id the
            // client asked for, unchanged.
            let resolved_source = state.terminal().resolve_open_id(&source);
            let settings = crate::terminal::OpenSettings { serial, decode };
            match state.terminal().open(&resolved_source, settings).await {
                Ok((session, subscriber_id, out_rx)) => {
                    if session.kind == TerminalKind::Serial {
                        tracing::info!(
                            origin,
                            "term.open source={resolved_source} session={}",
                            session.id
                        );
                    }
                    term_subs.lock().await.insert(session.id, subscriber_id);
                    spawn_terminal_forwarder(session.id, out_rx, out_tx.clone());
                    Response::ok(
                        id,
                        ResponseResult::TermOpen(TermOpenResult {
                            session: session.id,
                            source: resolved_source,
                            kind: session.kind,
                            label: session.label.clone(),
                            state: session.current_state().await,
                            serial: session.serial,
                        }),
                    )
                }
                Err(err) => terminal_error_response(id, err),
            }
        }

        Request::TermWrite {
            id,
            session,
            data,
            data_b64,
            ..
        } => match decode_term_write_payload(data, data_b64) {
            Ok(bytes) => match state.terminal().write(session, bytes).await {
                Ok(()) => {
                    tracing::info!(origin, "term.write session={session}");
                    Response::ok(id, ResponseResult::TermWrite(TermWriteResult {}))
                }
                Err(err) => terminal_error_response(id, err),
            },
            Err(err) => terminal_error_response(id, err),
        },

        Request::TermClose { id, session, .. } => {
            let subscriber_id = term_subs.lock().await.remove(&session);
            match subscriber_id {
                Some(subscriber_id) => match state.terminal().close(session, subscriber_id).await {
                    Ok(()) => {
                        tracing::info!(origin, "term.close session={session}");
                        Response::ok(id, ResponseResult::TermClose(TermCloseResult {}))
                    }
                    Err(err) => terminal_error_response(id, err),
                },
                // Not open on this connection (already closed, or never
                // opened here) — closing twice is not an error.
                None => Response::ok(id, ResponseResult::TermClose(TermCloseResult {})),
            }
        }
    }
}

/// Decodes `term.write`'s payload: exactly one of `data` (UTF-8 text) or
/// `data_b64` (arbitrary bytes) must be set (SPEC §3.1).
fn decode_term_write_payload(
    data: Option<String>,
    data_b64: Option<String>,
) -> Result<Vec<u8>, TerminalError> {
    use base64::Engine;
    match (data, data_b64) {
        (Some(text), None) => Ok(text.into_bytes()),
        (None, Some(b64)) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|err| TerminalError::BadBase64(err.to_string())),
        _ => Err(TerminalError::BadWritePayload),
    }
}

/// Forwards one connection's terminal output receiver into its `WsOut`
/// queue as `0xA7` frames, with its own per-connection sequence number
/// (SPEC §3.5). Spawned once per `term.open`, for the lifetime of that
/// connection's subscription — `ws.rs` aborts it (via dropping the
/// receiver, which ends this loop) when the socket closes.
fn spawn_terminal_forwarder(
    session_id: u16,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    out_tx: mpsc::Sender<WsOut>,
) {
    tokio::spawn(async move {
        let mut sequence: u32 = 0;
        while let Some(payload) = out_rx.recv().await {
            let bytes = awic_proto::term_frame::encode_frame(session_id, sequence, &payload);
            sequence = sequence.wrapping_add(1);
            // The connection's own outbound queue is bounded (`ws.rs`); a
            // full queue here means the client isn't draining at all. The
            // session layer (`terminal::session`) already applies
            // drop-and-count per subscriber before data reaches this
            // forwarder, so this is a last-resort backstop, not the primary
            // backpressure mechanism.
            if out_tx.try_send(WsOut::TermFrame(bytes)).is_err() {
                tracing::warn!(session_id, "dropping terminal frame: client not draining");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::PlaceholderBackend;
    use crate::usb::UnimplementedUsbController;
    use awic_core::Config;

    fn state() -> AppState {
        AppState::new(
            Config::default(),
            Arc::new(PlaceholderBackend),
            Arc::new(UnimplementedUsbController),
        )
    }

    fn out_channel() -> mpsc::Sender<WsOut> {
        mpsc::channel(8).0
    }

    fn recording_state() -> AppState {
        AppState::new(
            Config::default(),
            Arc::new(recording::RecordingBackend {
                captured_id: Arc::new(std::sync::Mutex::new(None)),
            }),
            Arc::new(UnimplementedUsbController),
        )
    }

    /// Runs `run_acquisition_stream` for a stream that delivered `received`
    /// of `requested` samples, and returns the end event it published.
    /// `still_registered: false` models an `acq.stop` having removed it first.
    async fn end_event(
        still_registered: bool,
        requested: u64,
        received: u64,
    ) -> (AcqState, String) {
        use awic_core::Backend;
        let state = recording_state();
        let mut events = state.event_bus().subscribe();
        if still_registered {
            let backend = recording::RecordingBackend {
                captured_id: Arc::new(std::sync::Mutex::new(None)),
            };
            let instrument = backend.open(&"1-1".parse().unwrap()).await.unwrap();
            state
                .insert_stream(
                    5,
                    crate::state::StreamHandle {
                        device: "1-1".into(),
                        instrument: Arc::new(tokio::sync::Mutex::new(instrument)),
                        forwarder: tokio::spawn(async {}),
                        backpressure: Arc::new(crate::backpressure::StreamBackpressure::new(
                            usize::MAX,
                        )),
                        trigger: None,
                    },
                )
                .await;
        }
        run_acquisition_stream(state.clone(), 5, requested, async move { received }).await;
        match events.recv().await.unwrap() {
            Event::AcqState {
                stream, state, msg, ..
            } => {
                assert_eq!(stream, 5);
                (state, msg.unwrap_or_default())
            }
            other => panic!("expected acq.state, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_capture_that_finishes_on_its_own_is_announced() {
        // Previously only acq.stop announced an end: clients showed
        // "running" forever after a capture simply completed.
        assert_eq!(
            end_event(true, 100, 100).await,
            (AcqState::Stopped, "complete".into())
        );
    }

    #[tokio::test]
    async fn a_capture_that_ends_short_is_reported_as_an_error() {
        let (state, msg) = end_event(true, 100, 40).await;
        assert_eq!(
            state,
            AcqState::Error,
            "a short capture must not read as complete"
        );
        assert!(msg.contains("40 of 100"), "{msg}");
    }

    #[tokio::test]
    async fn a_stopped_capture_is_announced_as_stopped_by_request() {
        let (state, msg) = end_event(false, 100, 40).await;
        assert_eq!(state, AcqState::Stopped);
        assert!(msg.contains("stopped by request"), "{msg}");
    }

    #[tokio::test]
    async fn opening_a_device_announces_flashing_then_ready() {
        let state = recording_state();
        state
            .registry()
            .detected("1-1".parse().unwrap(), vec![], None);
        let mut events = state.event_bus().subscribe();
        state.ensure_open("1-1").await.unwrap();
        let mut seen = Vec::new();
        while let Ok(Event::DeviceState { state, .. }) = events.try_recv() {
            seen.push(state);
        }
        assert_eq!(
            seen,
            vec![
                awic_proto::DeviceState::Flashing,
                awic_proto::DeviceState::Ready
            ],
            "real devices previously stayed `detected` forever"
        );
    }

    #[tokio::test]
    async fn device_list_returns_empty_registry() {
        let state = state();
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::DeviceList { v: 1, id: 1 },
        )
        .await;
        assert!(response.ok);
        match response.result.unwrap() {
            ResponseResult::DeviceList(r) => assert!(r.devices.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn usb_tree_surfaces_not_implemented_stub() {
        let state = state();
        let response =
            handle_request(&state, &out_channel(), 0, Request::UsbTree { v: 1, id: 3 }).await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "not_implemented");
    }

    #[tokio::test]
    async fn acq_start_without_prior_configure_is_a_clean_error() {
        let state = state();
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::AcqStart {
                v: 1,
                id: 4,
                device: "1-1".into(),
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "device_not_configured");
    }

    // --- a minimal recording Backend/Instrument, just for the capture-id
    // round-trip test below: it streams one chunk then finishes, and stashes
    // the `CaptureId` `start()` returned so the test can compare it against
    // both the wire response and what actually landed in the store.
    mod recording {
        use super::*;
        use async_trait::async_trait;
        use awic_core::{
            Backend, Capability, CaptureId, DeviceId, Instrument, Result as CoreResult, SampleChunk,
        };
        use awic_proto::frame::Encoding;
        use bytes::Bytes;
        use std::sync::Mutex as StdMutex;

        pub struct RecordingInstrument {
            pub id: DeviceId,
            pub cfg: Option<awic_core::AcqConfig>,
            pub captured_id: Arc<StdMutex<Option<CaptureId>>>,
        }

        #[async_trait]
        impl Instrument for RecordingInstrument {
            fn id(&self) -> &DeviceId {
                &self.id
            }
            /// Stands in for a device that places its trigger itself, as
            /// awic-fake does; the software trigger has its own tests.
            fn honours_trigger(&self) -> bool {
                true
            }
            fn capabilities(&self) -> &[Capability] {
                &[]
            }
            async fn prepare(&mut self) -> CoreResult<()> {
                Ok(())
            }
            /// Behaves like a real scope: snaps the rate down to its
            /// ladder (as the FX2 oscilloscope must) and supplies analog
            /// calibration only the driver could know.
            async fn configure(
                &mut self,
                mut cfg: awic_core::AcqConfig,
            ) -> CoreResult<awic_core::EffectiveAcq> {
                const LADDER: [u64; 3] = [1_000, 1_000_000, 8_000_000];
                cfg.rate_hz = LADDER
                    .iter()
                    .copied()
                    .filter(|&r| r <= cfg.rate_hz)
                    .max()
                    .unwrap_or(LADDER[0]);
                let calibration = cfg
                    .channels
                    .iter()
                    .map(|&channel| awic_core::ChannelCalibration {
                        channel,
                        scale: 0.04,
                        offset: -5.0,
                        unit: "V".into(),
                        range_v: Some(5.0),
                    })
                    .collect();
                self.cfg = Some(cfg.clone());
                Ok(awic_core::EffectiveAcq { cfg, calibration })
            }
            async fn start(&mut self, sink: SampleSink) -> CoreResult<CaptureId> {
                let cfg = self.cfg.clone().expect("configure before start");
                let channel_count = cfg.channels.len().max(1) as u8;
                let capture_id = CaptureId::new();
                *self.captured_id.lock().unwrap() = Some(capture_id);
                tokio::spawn(async move {
                    let _ = sink
                        .send(SampleChunk {
                            encoding: Encoding::U8Interleaved,
                            channel_count,
                            sample_count: 8,
                            t0_ns: 0,
                            data: Bytes::from(vec![9u8; 8 * channel_count as usize]),
                        })
                        .await;
                    // `sink` drops here, closing the channel so the
                    // forwarder's `rx.recv()` returns `None` and it files
                    // the capture.
                });
                Ok(capture_id)
            }
            async fn stop(&mut self) -> CoreResult<()> {
                Ok(())
            }
        }

        pub struct RecordingBackend {
            pub captured_id: Arc<StdMutex<Option<CaptureId>>>,
        }

        #[async_trait]
        impl Backend for RecordingBackend {
            fn name(&self) -> &str {
                "recording"
            }
            async fn discover(&self) -> CoreResult<Vec<DeviceId>> {
                Ok(vec![])
            }
            async fn open(&self, id: &DeviceId) -> CoreResult<Box<dyn Instrument>> {
                Ok(Box::new(RecordingInstrument {
                    id: id.clone(),
                    cfg: None,
                    captured_id: self.captured_id.clone(),
                }))
            }
        }
    }

    // --- B-32: a device that cannot trigger. It streams a square wave on
    // CH2 (and a frame counter on CH1) until stopped, as the native drivers
    // do in continuous mode, and records what it was configured with.
    mod streaming {
        use super::*;
        use async_trait::async_trait;
        use awic_core::{
            AcqConfig, Backend, Capability, CaptureId, DeviceId, Instrument, Result as CoreResult,
            SampleChunk,
        };
        use awic_proto::frame::Encoding;
        use bytes::Bytes;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        pub struct Seen {
            pub cfg: StdMutex<Option<AcqConfig>>,
            pub stopped: AtomicBool,
        }

        pub struct SquareInstrument {
            id: DeviceId,
            seen: Arc<Seen>,
        }

        #[async_trait]
        impl Instrument for SquareInstrument {
            fn id(&self) -> &DeviceId {
                &self.id
            }
            fn capabilities(&self) -> &[Capability] {
                &[]
            }
            async fn prepare(&mut self) -> CoreResult<()> {
                Ok(())
            }
            async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<awic_core::EffectiveAcq> {
                *self.seen.cfg.lock().unwrap() = Some(cfg.clone());
                let calibration = cfg
                    .channels
                    .iter()
                    .map(|&channel| awic_core::ChannelCalibration {
                        channel,
                        scale: 0.04,
                        offset: -5.0,
                        unit: "V".into(),
                        range_v: Some(5.0),
                    })
                    .collect();
                Ok(awic_core::EffectiveAcq { cfg, calibration })
            }
            async fn start(&mut self, sink: SampleSink) -> CoreResult<CaptureId> {
                let seen = self.seen.clone();
                seen.stopped.store(false, Ordering::SeqCst);
                tokio::spawn(async move {
                    // CH2: codes 10 (-4.6 V) for 50 frames, 200 (+3.0 V) for 50.
                    let mut frame = 0usize;
                    while !seen.stopped.load(Ordering::SeqCst) {
                        let data: Vec<u8> = (frame..frame + 30)
                            .flat_map(|i| [(i % 256) as u8, if i % 100 < 50 { 10 } else { 200 }])
                            .collect();
                        let chunk = SampleChunk {
                            encoding: Encoding::U8Interleaved,
                            channel_count: 2,
                            sample_count: 30,
                            t0_ns: frame as u64 * 1_000,
                            data: Bytes::from(data),
                        };
                        if sink.send(chunk).await.is_err() {
                            break;
                        }
                        frame += 30;
                        tokio::task::yield_now().await;
                    }
                });
                Ok(CaptureId::new())
            }
            async fn stop(&mut self) -> CoreResult<()> {
                self.seen.stopped.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        pub struct SquareBackend(pub Arc<Seen>);

        #[async_trait]
        impl Backend for SquareBackend {
            fn name(&self) -> &str {
                "square"
            }
            async fn discover(&self) -> CoreResult<Vec<DeviceId>> {
                Ok(vec![])
            }
            async fn open(&self, id: &DeviceId) -> CoreResult<Box<dyn Instrument>> {
                Ok(Box::new(SquareInstrument {
                    id: id.clone(),
                    seen: self.0.clone(),
                }))
            }
        }
    }

    /// Configures a CH2 rising trigger at `level` volts on the square-wave
    /// device and starts it; returns the state, what the device saw, the
    /// configure result and the stream.
    async fn soft_triggered(
        level: f32,
    ) -> (
        AppState,
        Arc<streaming::Seen>,
        AcqConfigureResult,
        u16,
        String,
    ) {
        use awic_core::{AcqConfig, Slope, Trigger};
        let seen = Arc::new(streaming::Seen::default());
        let state = AppState::new(
            Config::default(),
            Arc::new(streaming::SquareBackend(seen.clone())),
            Arc::new(UnimplementedUsbController),
        );
        let out_tx = out_channel();
        let configure = handle_request(
            &state,
            &out_tx,
            0,
            Request::AcqConfigure {
                v: 1,
                id: 1,
                device: "1-1".into(),
                cfg: AcqConfig {
                    rate_hz: 1_000_000,
                    channels: vec![0, 1],
                    samples: 64,
                    trigger: Some(Trigger::Edge {
                        channel: 1,
                        slope: Slope::Rising,
                        level: Some(level),
                    }),
                    ranges_v: vec![],
                    continuous: false,
                },
            },
        )
        .await;
        assert!(configure.ok, "{:?}", configure.error);
        let Some(ResponseResult::AcqConfigure(configured)) = configure.result else {
            panic!("expected an acq.configure result");
        };
        let start = handle_request(
            &state,
            &out_tx,
            0,
            Request::AcqStart {
                v: 1,
                id: 2,
                device: "1-1".into(),
            },
        )
        .await;
        assert!(start.ok, "{:?}", start.error);
        let Some(ResponseResult::AcqStart(started)) = start.result else {
            panic!("expected an acq.start result");
        };
        (state, seen, configured, started.stream, started.capture)
    }

    #[tokio::test]
    async fn a_trigger_the_device_cannot_honour_is_found_in_software() {
        let (state, seen, configured, _stream, capture) = soft_triggered(0.0).await;

        // The client is told the trigger stands; the driver was asked to
        // stream, with no trigger it would ignore.
        assert!(configured.cfg.trigger.is_some());
        assert!(!configured.cfg.continuous);
        assert_eq!(configured.cfg.samples, 64);
        let driver = seen.cfg.lock().unwrap().clone().unwrap();
        assert!(driver.continuous && driver.trigger.is_none());

        let id: CaptureId = capture.parse().unwrap();
        let stored = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(c) = state.capture_store().get(&id) {
                    return c;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the triggered capture is filed");
        assert_eq!(stored.meta.trigger_position, Some(32));
        let awic_core::CaptureData::Analog { raw, .. } = &stored.data else {
            panic!("analog capture expected");
        };
        assert_eq!(raw.len(), 64 * 2, "exactly the samples asked for");
        // The edge is where the capture says: CH2 low at 31, high at 32 —
        // round 8's captures were a flat level all the way through.
        assert_eq!(raw[31 * 2 + 1], 10);
        assert_eq!(raw[32 * 2 + 1], 200);
        // First rising edge after 32 frames of history is stream frame 50.
        assert_eq!(raw[32 * 2], 50);
        assert!(
            seen.stopped.load(std::sync::atomic::Ordering::SeqCst),
            "the continuous stream is stopped once the window is full"
        );
    }

    #[tokio::test]
    async fn a_trigger_that_never_fires_files_nothing_and_says_why() {
        // Above the signal's +3.0 V maximum: it can never be crossed.
        let (state, _seen, _configured, stream, capture) = soft_triggered(4.0).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(state
            .capture_store()
            .get(&capture.parse::<CaptureId>().unwrap())
            .is_none());
        let watch = state.trigger_watch(stream).await.expect("still armed");
        assert!(!watch.fired());
        let why = watch.not_fired_reason();
        assert!(why.contains("rising edge on CH2 through 4.000 V"), "{why}");
        assert!(why.contains("outside"), "{why}");
    }

    #[tokio::test]
    async fn a_trigger_on_a_channel_not_acquired_is_refused_at_configure() {
        use awic_core::{AcqConfig, Slope, Trigger};
        let seen = Arc::new(streaming::Seen::default());
        let state = AppState::new(
            Config::default(),
            Arc::new(streaming::SquareBackend(seen)),
            Arc::new(UnimplementedUsbController),
        );
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::AcqConfigure {
                v: 1,
                id: 1,
                device: "1-1".into(),
                cfg: AcqConfig {
                    rate_hz: 1_000_000,
                    channels: vec![0],
                    samples: 64,
                    trigger: Some(Trigger::Edge {
                        channel: 1,
                        slope: Slope::Rising,
                        level: Some(1.0),
                    }),
                    ranges_v: vec![],
                    continuous: false,
                },
            },
        )
        .await;
        assert!(!response.ok);
        let err = response.error.unwrap();
        assert_eq!(err.code, "invalid_params");
        assert!(err.msg.contains("not acquired"), "{}", err.msg);
    }

    #[tokio::test]
    async fn acq_configure_reports_the_drivers_effective_config_not_the_request() {
        use awic_core::AcqConfig;
        use recording::RecordingBackend;

        let state = AppState::new(
            Config::default(),
            Arc::new(RecordingBackend {
                captured_id: Arc::new(std::sync::Mutex::new(None)),
            }),
            Arc::new(UnimplementedUsbController),
        );
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::AcqConfigure {
                v: 1,
                id: 7,
                device: "1-1".into(),
                cfg: AcqConfig {
                    // Not on the device's ladder: it will run at 8 MHz.
                    rate_hz: 10_000_000,
                    channels: vec![0, 1],
                    samples: 64,
                    trigger: None,
                    ranges_v: vec![],
                    continuous: false,
                },
            },
        )
        .await;
        assert!(response.ok, "{:?}", response.error);
        let Some(ResponseResult::AcqConfigure(result)) = response.result else {
            panic!("expected an acq.configure result");
        };
        assert_eq!(
            result.cfg.rate_hz, 8_000_000,
            "the client must be told the rate the hardware runs, or its timebase is 25% off"
        );
        assert_eq!(result.calibration.len(), 2);
        assert_eq!(result.calibration[0].unit, "V");
        assert_eq!(result.calibration[0].offset, -5.0);
        // And the server keeps the same truth for filing the capture.
        let stored = state.get_config("1-1").await.expect("config on file");
        assert_eq!(stored.cfg.rate_hz, 8_000_000);
    }

    #[tokio::test]
    async fn acq_start_id_reads_back_from_the_capture_store() {
        use awic_core::{AcqConfig, Slope, Trigger};
        use recording::RecordingBackend;

        let captured_id = Arc::new(std::sync::Mutex::new(None));
        let state = AppState::new(
            Config::default(),
            Arc::new(RecordingBackend {
                captured_id: captured_id.clone(),
            }),
            Arc::new(UnimplementedUsbController),
        );
        let out_tx = out_channel();

        let configure = handle_request(
            &state,
            &out_tx,
            0,
            Request::AcqConfigure {
                v: 1,
                id: 1,
                device: "1-1".into(),
                cfg: AcqConfig {
                    rate_hz: 1_000,
                    channels: vec![0, 1],
                    samples: 64,
                    trigger: Some(Trigger::Edge {
                        channel: 0,
                        slope: Slope::Rising,
                        level: Some(1.0),
                    }),
                    ranges_v: vec![],
                    continuous: false,
                },
            },
        )
        .await;
        assert!(configure.ok, "{:?}", configure.error);

        let start = handle_request(
            &state,
            &out_tx,
            0,
            Request::AcqStart {
                v: 1,
                id: 2,
                device: "1-1".into(),
            },
        )
        .await;
        assert!(start.ok, "{:?}", start.error);
        let capture_uri = match start.result.unwrap() {
            ResponseResult::AcqStart(r) => r.capture,
            other => panic!("unexpected {other:?}"),
        };

        let expected_id = captured_id
            .lock()
            .unwrap()
            .expect("instrument.start() must have run");
        assert_eq!(
            capture_uri,
            expected_id.to_string(),
            "acq.start must report the id Instrument::start returned"
        );

        // Let the forwarder drain the one chunk and file the capture.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let capture = state
            .capture_store()
            .get(&expected_id)
            .expect("the capture must read back under the id acq.start reported");
        assert_eq!(capture.meta.rate_hz, 1_000);
        // awic-fake's convention: an edge trigger lands at the midpoint.
        assert_eq!(capture.meta.trigger_position, Some(32));
    }

    /// SPECIFICATION.md §2.3.2/§2.8: a device configured for continuous
    /// acquisition must be driven by the recorder, not the finite-capture
    /// forwarder — which would otherwise accumulate an unbounded stream into
    /// memory. `acq.start` must refuse it with a clear pointer to `rec.start`
    /// rather than silently running forever.
    #[tokio::test]
    async fn acq_start_on_a_continuous_device_is_refused() {
        use awic_core::AcqConfig;

        let state = recording_state();
        let out_tx = out_channel();

        let configure = handle_request(
            &state,
            &out_tx,
            0,
            Request::AcqConfigure {
                v: 1,
                id: 1,
                device: "1-1".into(),
                cfg: AcqConfig {
                    rate_hz: 1_000_000,
                    channels: vec![0],
                    samples: 0,
                    trigger: None,
                    ranges_v: vec![],
                    continuous: true,
                },
            },
        )
        .await;
        assert!(configure.ok, "{:?}", configure.error);

        let start = handle_request(
            &state,
            &out_tx,
            0,
            Request::AcqStart {
                v: 1,
                id: 2,
                device: "1-1".into(),
            },
        )
        .await;
        assert!(
            !start.ok,
            "acq.start on a continuous device must be refused"
        );
        let error = start.error.unwrap();
        assert_eq!(error.code, "continuous_needs_recorder");
        assert!(
            error.msg.contains("rec.start"),
            "the refusal must point at the way to actually run this: {}",
            error.msg
        );
    }

    #[tokio::test]
    async fn acq_stop_on_unknown_stream_is_a_clean_error() {
        let state = state();
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::AcqStop {
                v: 1,
                id: 5,
                stream: 999,
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "stream_not_found");
    }

    /// B-37: a capture the store dropped answers `capture_evicted` (naming
    /// the bounds), a never-seen id still answers `capture_not_found`.
    #[tokio::test]
    async fn an_evicted_capture_answers_capture_evicted() {
        use awic_core::NewAnalogCapture;
        use awic_proto::MeasurementKind;

        let state = state();
        let mut first = None;
        for _ in 0..=awic_core::capture_store::DEFAULT_MAX_CAPTURES {
            let id = state.capture_store().insert_analog(NewAnalogCapture {
                channels: (0..1).collect(),
                id: awic_core::CaptureId::new(),
                device: "1-1".parse().unwrap(),
                rate_hz: 1_000,
                channel_count: 1,
                sample_bits: 8,
                channel_scale: vec![0.04],
                channel_offset: vec![0.0],
                raw: vec![0, 1],
                t0_ns: 0,
                trigger_position: None,
            });
            first.get_or_insert(id);
        }
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::CaptureMeasure {
                v: 1,
                id: 12,
                capture: first.unwrap().to_string(),
                channel: 0,
                measurements: vec![MeasurementKind::Vpp],
            },
        )
        .await;
        let error = response.error.expect("an error");
        assert_eq!(error.code, "capture_evicted");
        assert!(error.msg.contains("32 captures / 512 MB"), "{}", error.msg);
    }

    /// End-to-end `capture.measure` against a capture actually sitting in the
    /// store — the wire op, dispatched, reading the same full-resolution data
    /// `awic-core::measure` is unit-tested against directly.
    #[tokio::test]
    async fn capture_measure_reads_a_stored_capture_end_to_end() {
        use awic_core::NewAnalogCapture;
        use awic_proto::MeasurementKind;

        let state = state();
        let capture_id = state.capture_store().insert_analog(NewAnalogCapture {
            channels: (0..1).collect(),
            id: awic_core::CaptureId::new(),
            device: "1-1".parse().unwrap(),
            rate_hz: 1_000,
            channel_count: 1,
            sample_bits: 8,
            channel_scale: vec![0.04],
            channel_offset: vec![-5.0],
            raw: vec![0, 128, 255],
            t0_ns: 0,
            trigger_position: None,
        });

        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::CaptureMeasure {
                v: 1,
                id: 10,
                capture: capture_id.to_string(),
                channel: 0,
                measurements: vec![MeasurementKind::Vmin, MeasurementKind::Vmax],
            },
        )
        .await;
        assert!(response.ok, "{:?}", response.error);
        let Some(ResponseResult::CaptureMeasure(result)) = response.result else {
            panic!("expected a capture.measure result");
        };
        assert_eq!(result.capture, capture_id.to_string());
        assert_eq!(result.channel, 0);
        assert_eq!(result.results.len(), 2);
        assert!(matches!(
            result.results[0],
            awic_proto::MeasurementResult::Ok { .. }
        ));
    }

    #[tokio::test]
    async fn capture_measure_on_an_unknown_capture_is_a_named_error() {
        use awic_proto::MeasurementKind;

        let state = state();
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::CaptureMeasure {
                v: 1,
                id: 11,
                capture: "capture://00000000-0000-0000-0000-000000000000".into(),
                channel: 0,
                measurements: vec![MeasurementKind::Vpp],
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "capture_not_found");
    }

    #[tokio::test]
    async fn capture_measure_on_an_unknown_channel_is_a_named_error() {
        use awic_core::NewAnalogCapture;
        use awic_proto::MeasurementKind;

        let state = state();
        let capture_id = state.capture_store().insert_analog(NewAnalogCapture {
            channels: (0..1).collect(),
            id: awic_core::CaptureId::new(),
            device: "1-1".parse().unwrap(),
            rate_hz: 1_000,
            channel_count: 1,
            sample_bits: 8,
            channel_scale: vec![1.0],
            channel_offset: vec![0.0],
            raw: vec![0, 1, 2],
            t0_ns: 0,
            trigger_position: None,
        });

        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::CaptureMeasure {
                v: 1,
                id: 12,
                capture: capture_id.to_string(),
                channel: 5,
                measurements: vec![MeasurementKind::Vpp],
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "channel_not_found");
    }

    #[tokio::test]
    async fn capture_measure_with_a_malformed_capture_handle_is_a_named_error() {
        use awic_proto::MeasurementKind;

        let state = state();
        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::CaptureMeasure {
                v: 1,
                id: 13,
                capture: "not-a-capture-uri".into(),
                channel: 0,
                measurements: vec![MeasurementKind::Vpp],
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_capture_id");
    }

    #[tokio::test]
    async fn capture_read_decimates_one_channel_and_reports_truncation() {
        use awic_core::NewAnalogCapture;

        let state = state();
        let raw: Vec<u8> = (0..200u16).map(|i| (i % 256) as u8).collect();
        let capture_id = state.capture_store().insert_analog(NewAnalogCapture {
            channels: vec![0],
            id: awic_core::CaptureId::new(),
            device: "1-1".parse().unwrap(),
            rate_hz: 1_000,
            channel_count: 1,
            sample_bits: 8,
            channel_scale: vec![1.0],
            channel_offset: vec![0.0],
            raw,
            t0_ns: 0,
            trigger_position: None,
        });

        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::CaptureRead {
                v: 1,
                id: 20,
                capture: capture_id.to_string(),
                channel: 0,
                max_points: Some(10),
                t0_s: None,
                t1_s: None,
            },
        )
        .await;
        assert!(response.ok, "{:?}", response.error);
        let Some(ResponseResult::CaptureRead(result)) = response.result else {
            panic!("expected a capture.read result");
        };
        assert_eq!(result.min.len(), 10);
        assert_eq!(result.max.len(), 10);
        assert!(
            result.truncated,
            "200 samples into 10 buckets must be reported as truncated"
        );
        assert_eq!(result.unit, "V");
    }

    #[tokio::test]
    async fn capture_read_caps_max_points_at_the_hard_limit() {
        use awic_core::NewDigitalCapture;

        let state = state();
        let capture_id = state.capture_store().insert_digital(NewDigitalCapture {
            channels: (0..1).collect(),
            id: awic_core::CaptureId::new(),
            device: "1-1".parse().unwrap(),
            rate_hz: 1_000_000,
            channel_count: 1,
            packed: vec![0u8; 10_000],
            t0_ns: 0,
            trigger_position: None,
        });

        let response = handle_request(
            &state,
            &out_channel(),
            0,
            Request::CaptureRead {
                v: 1,
                id: 21,
                capture: capture_id.to_string(),
                channel: 0,
                max_points: Some(1_000_000),
                t0_s: None,
                t1_s: None,
            },
        )
        .await;
        assert!(response.ok, "{:?}", response.error);
        let Some(ResponseResult::CaptureRead(result)) = response.result else {
            panic!("expected a capture.read result");
        };
        assert!(
            result.min.len() <= 4096,
            "hard cap must apply: {}",
            result.min.len()
        );
    }
}
