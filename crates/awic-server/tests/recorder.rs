//! Integration tests for the recorder (`rec.*`: SPECIFICATION.md §2.8, §2.9,
//! §3.4) over a real `/ws` connection, driven by `awic-fake`'s synthetic
//! instruments — no hardware needed (DEVELOPMENT.md §1: "no test may
//! require hardware"). Hardware validation against the real logic analyzer/oscilloscope is
//! a separate, manual step (see T18's task report).
//!
//! Two `awic-fake` facts these tests lean on, so a pattern needs no custom
//! test backend:
//! - The default digital device's channels are a ripple counter (its own
//!   doc comment: "the classic logic analyzer demo pattern") — bit `b`
//!   toggles every `2^b` samples forever, an exact square wave with no
//!   noise, so `D<b>`'s "bit rate" for `awic-pattern`'s `f=` is
//!   `rate_hz / 2^b`.
//! - The default analog device's channels are a 1 kHz, 1 V sine — matching
//!   the real FX2 oscilloscope's CAL-output validation scenario.

use std::sync::Arc;
use std::time::Duration;

use awic_core::Config;
use awic_fake::{AnalogDeviceSpec, DigitalDeviceSpec, FakeBackend};
use awic_proto::view_frame::{decode_view_frame, ViewFrameKind};
use awic_proto::Response;
use awic_server::state::AppState;
use awic_server::usb::UnimplementedUsbController;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as ClientMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn spawn_server(config_toml: &str) -> (String, tokio::task::JoinHandle<()>) {
    let config = Config::from_toml_str(config_toml).expect("valid test config");
    let state = AppState::new(
        config,
        Arc::new(FakeBackend::new()),
        Arc::new(UnimplementedUsbController),
    );
    state.bootstrap().await;
    let router = awic_server::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("server error");
    });
    (format!("ws://{addr}/ws"), handle)
}

fn send(v: Value) -> ClientMessage {
    ClientMessage::Text(v.to_string().into())
}

/// Reads text control frames until a `Response` (has an `id`) arrives,
/// ignoring any binary frame (view/sample frames racing ahead) and any
/// unsolicited `Event` (e.g. `device.state` from the fake device's own
/// `flashing` -> `ready` transition, which carries no `id`).
async fn recv_response(ws: &mut WsStream) -> Response {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a response")
            .expect("connection closed unexpectedly")
            .expect("websocket error");
        if let ClientMessage::Text(text) = msg {
            let value: Value = serde_json::from_str(text.as_str()).expect("valid JSON");
            if value.get("id").is_some() {
                return serde_json::from_value(value).expect("valid Response JSON");
            }
        }
    }
}

/// Sends `body` (which must already carry `"op"`) with `v`/`id` filled in,
/// and returns the raw response.
async fn request(ws: &mut WsStream, id: u32, mut body: Value) -> Response {
    body["v"] = json!(1);
    body["id"] = json!(id);
    ws.send(send(body)).await.expect("send");
    recv_response(ws).await
}

/// Like [`request`], but asserts success and returns the `result` payload.
async fn ok_request(ws: &mut WsStream, id: u32, body: Value) -> Value {
    let resp = request(ws, id, body.clone()).await;
    assert!(
        resp.ok,
        "request {id} ({body:?} was sent) failed: {:?}",
        resp.error
    );
    serde_json::to_value(resp.result.unwrap()).unwrap()
}

fn digital_device() -> String {
    DigitalDeviceSpec::logic_8ch_like().id.to_string()
}

fn analog_device() -> String {
    AnalogDeviceSpec::scope_2ch_like().id.to_string()
}

/// `rec.start` plus a follow view: frames arrive at roughly the requested
/// fps, tagged with the view's id and a digital-shaped kind.
#[tokio::test]
async fn follow_view_receives_frames_at_roughly_the_requested_fps() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [4], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    // Let some history build up before opening the view.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let view = ok_request(
        &mut ws,
        2,
        json!({ "op": "rec.view", "rec": rec, "follow_s": 0.01, "columns": 200, "fps": 10 }),
    )
    .await;
    let view_id = view["view"].as_u64().unwrap() as u16;

    let mut frames = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(600), ws.next()).await {
            Ok(Some(Ok(ClientMessage::Binary(bytes)))) => {
                let (header, _payload) = decode_view_frame(&bytes).expect("valid view frame");
                assert_eq!(header.view_id, view_id);
                assert!(matches!(
                    header.kind,
                    ViewFrameKind::DigitalRuns | ViewFrameKind::DigitalSummary
                ));
                frames += 1;
            }
            Ok(Some(Ok(ClientMessage::Text(_)))) => {}
            _ => break,
        }
    }
    // 10 fps over ~500ms is ~5 frames; generous slack for scheduling jitter,
    // but still catches "no frames at all" and "one frame per tick with no
    // conflation, hundreds of frames" alike.
    assert!(
        frames >= 2,
        "expected multiple follow-view frames, got {frames}"
    );
    assert!(
        frames <= 15,
        "expected roughly fps*duration frames, got {frames}"
    );

    let _ = ok_request(
        &mut ws,
        3,
        json!({ "op": "rec.view.close", "view": view_id }),
    )
    .await;
    let _ = ok_request(&mut ws, 4, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// A view client that never reads its socket must not stall the
/// connection's other traffic — the deterministic proof that this is
/// conflation (bounded) rather than an unbounded queue lives in
/// `recorder::views`'s own unit tests; this checks the same property at the
/// wire level: the connection keeps answering other requests regardless.
#[tokio::test]
async fn a_view_client_that_never_drains_does_not_stall_the_connection() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [4], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    let _ = ok_request(
        &mut ws,
        2,
        json!({ "op": "rec.view", "rec": rec, "follow_s": 0.01, "columns": 4000, "fps": 60 }),
    )
    .await;

    // Deliberately do not read anything for a while.
    tokio::time::sleep(Duration::from_millis(300)).await;

    ws.send(send(json!({"v": 1, "id": 3, "op": "device.list"})))
        .await
        .expect("send");
    let resp = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ClientMessage::Text(text) = ws.next().await.unwrap().unwrap() {
                let r: Response = serde_json::from_str(text.as_str()).unwrap();
                if r.id == 3 {
                    return r;
                }
            }
        }
    })
    .await
    .expect("connection stalled: device.list never answered after a non-draining view client");
    assert!(resp.ok, "{:?}", resp.error);

    let _ = ok_request(&mut ws, 4, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// `rec.search` finds a bit pattern in a fake digital signal, at the
/// expected, exact spacing given the synthetic source's known period.
#[tokio::test]
async fn rec_search_finds_a_bit_pattern_in_a_fake_digital_signal() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [4], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    // D4 toggles every 2^4 = 16 samples at 1 MHz => bit rate 62500 Hz.
    // Poll rather than sleep a fixed time: how much the fake instrument has
    // streamed depends on machine load, and a flat sleep makes this a race
    // (it lost it under a parallel test run, 2026-09-16).
    let mut result = json!(null);
    let mut matches: Vec<serde_json::Value> = Vec::new();
    for attempt in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        result = ok_request(
            &mut ws,
            2 + attempt,
            json!({ "op": "rec.search", "rec": rec, "pattern": "D4 f=62500 1010", "max_matches": 16 }),
        )
        .await;
        matches = result["matches"].as_array().cloned().unwrap_or_default();
        if matches.len() >= 2 {
            break;
        }
    }
    assert!(
        !matches.is_empty(),
        "expected at least one match, got {result:?}"
    );
    if matches.len() >= 2 {
        let t0 = matches[0]["t_s"].as_f64().unwrap();
        let t1 = matches[1]["t_s"].as_f64().unwrap();
        // 4 cells * 16 samples/cell = 64 samples = 64 microseconds at 1 MHz.
        assert!(
            (t1 - t0 - 0.000_064).abs() < 0.000_005,
            "unexpected match spacing: {t0} then {t1}"
        );
    }

    let _ = ok_request(&mut ws, 99, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// B-44: a window opening inside a pulse must not report the window start as
/// that pulse's onset — the match is marked `truncated`, whole pulses after
/// it are not.
#[tokio::test]
async fn rec_search_marks_a_match_cut_by_the_window_start_as_truncated() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [4], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    // D4 is high for 16 samples (16 us) at a time. Find real onsets first.
    let mut onsets: Vec<u64> = Vec::new();
    for attempt in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let r = ok_request(
            &mut ws,
            2 + attempt,
            json!({ "op": "rec.search", "rec": rec, "pattern": "D4 high >5us", "max_matches": 8 }),
        )
        .await;
        onsets = r["matches"]
            .as_array()
            .map(|m| m.iter().map(|x| x["sample"].as_u64().unwrap()).collect())
            .unwrap_or_default();
        if onsets.len() >= 4 {
            break;
        }
    }
    assert!(onsets.len() >= 4, "{onsets:?}");

    // Open the window 4 samples into the second pulse.
    let cut = onsets[1] + 4;
    let r = ok_request(
        &mut ws,
        200,
        json!({ "op": "rec.search", "rec": rec, "pattern": "D4 high >2us",
                "t0_s": cut as f64 / 1e6, "max_matches": 3 }),
    )
    .await;
    let m = r["matches"].as_array().unwrap();
    assert_eq!(m[0]["sample"].as_u64(), Some(cut), "{r}");
    assert_eq!(m[0]["truncated"], json!(true), "{r}");
    assert_eq!(m[1]["sample"].as_u64(), Some(onsets[2]), "{r}");
    assert!(m[1].get("truncated").is_none(), "{r}");

    let _ = ok_request(&mut ws, 99, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// `rec.wait` resolves with an honest timeout when the pattern never
/// occurs, and — the important part — another request on the same
/// connection is answered well before that timeout elapses (SPEC §2.9.3:
/// a pending wait must not block the connection's other requests).
#[tokio::test]
async fn rec_wait_times_out_cleanly_without_blocking_other_requests() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [2], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    // D2 toggles every 4 samples (4 microseconds): a 5ms low level never
    // happens, so this only ever resolves by timing out.
    ws.send(send(json!({
        "v": 1, "id": 2, "op": "rec.wait", "rec": rec, "pattern": "D2 low >=5ms", "timeout_ms": 2000
    })))
    .await
    .expect("send");
    ws.send(send(json!({"v": 1, "id": 3, "op": "device.list"})))
        .await
        .expect("send");

    let resp3 = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let r = recv_response(&mut ws).await;
            if r.id == 3 {
                return r;
            }
        }
    })
    .await
    .expect("device.list did not answer promptly while rec.wait was pending");
    assert!(resp3.ok, "{:?}", resp3.error);

    let resp2 = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let r = recv_response(&mut ws).await;
            if r.id == 2 {
                return r;
            }
        }
    })
    .await
    .expect("rec.wait never resolved");
    assert!(resp2.ok, "{:?}", resp2.error);
    let result = serde_json::to_value(resp2.result.unwrap()).unwrap();
    assert_eq!(
        result.get("timeout"),
        Some(&json!(true)),
        "expected a timeout, got {result:?}"
    );

    let _ = ok_request(&mut ws, 4, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// `rec.wait` resolving with an actual match (the companion of the timeout
/// test above): D2 rises every 4 samples, so this resolves almost at once.
#[tokio::test]
async fn rec_wait_resolves_with_a_match() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [2], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    let result = ok_request(
        &mut ws,
        2,
        json!({ "op": "rec.wait", "rec": rec, "pattern": "D2 rising", "timeout_ms": 5000 }),
    )
    .await;
    assert!(
        result.get("match").is_some(),
        "expected a match, got {result:?}"
    );

    let _ = ok_request(&mut ws, 3, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// B-78: a level already in progress where the wait begins is not an event
/// after the call. D4 (counter bit 4) is low for samples `[32k, 32k+16)`;
/// resuming 2 samples into such a run (14 samples still low, past the
/// 10 µs bound) used to answer that run, at the resume
/// sample, as if it had begun there. The answer is the next low run's real
/// onset.
#[tokio::test]
async fn rec_wait_does_not_match_a_level_already_in_progress() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [4], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    let resume: u64 = 32 * 1000 + 2;
    for attempt in 0..100 {
        let list = ok_request(&mut ws, 100 + attempt, json!({ "op": "rec.list" })).await;
        let head = list["recordings"]
            .as_array()
            .and_then(|recordings| {
                recordings
                    .iter()
                    .find(|r| r["rec"].as_u64() == Some(u64::from(rec)))
                    .and_then(|r| r["head_sample"].as_u64())
            })
            .unwrap_or(0);
        if head > resume + 64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let result = ok_request(
        &mut ws,
        2,
        json!({
            "op": "rec.wait", "rec": rec, "pattern": "D4 low >=10us",
            "from_s": resume as f64 / 1e6, "timeout_ms": 5000
        }),
    )
    .await;
    let sample = result["match"]["sample"]
        .as_u64()
        .unwrap_or_else(|| panic!("expected a match, got {result:?}"));
    assert_eq!(
        sample,
        32 * 1001,
        "the wait must answer the next low run's onset, got {result:?}"
    );

    let _ = ok_request(&mut ws, 3, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// `rec.snapshot` then `capture.measure`: the snapshot of a recording is an
/// ordinary capture, so measuring it reads back the source signal's known
/// frequency.
#[tokio::test]
async fn rec_snapshot_then_capture_measure_reads_the_known_frequency() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": analog_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    // Wait until the recording actually holds the 10 ms the snapshot asks
    // for. A fixed sleep assumes how fast the fake instrument streams under
    // whatever else the test run is doing, and loses that bet on a loaded
    // machine (2026-09-16).
    for attempt in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let list = ok_request(&mut ws, 100 + attempt, json!({ "op": "rec.list" })).await;
        let head = list["recordings"]
            .as_array()
            .and_then(|recordings| {
                recordings
                    .iter()
                    .find(|r| r["rec"].as_u64() == Some(u64::from(rec)))
                    .and_then(|r| r["head_sample"].as_u64())
            })
            .unwrap_or(0);
        if head >= 10_000 {
            break;
        }
    }

    let snap = ok_request(
        &mut ws,
        2,
        json!({ "op": "rec.snapshot", "rec": rec, "t0_s": 0.0, "t1_s": 0.01 }),
    )
    .await;
    let capture = snap["capture"]
        .as_str()
        .expect("capture handle")
        .to_string();
    assert!(capture.starts_with("capture://"));

    let measured = ok_request(
        &mut ws,
        3,
        json!({
            "op": "capture.measure", "capture": capture, "channel": 0, "measurements": ["frequency"]
        }),
    )
    .await;
    let results = measured["results"].as_array().expect("results array");
    let value = results[0]["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("frequency should be measurable: {results:?}"));
    assert!(
        (value - 1000.0).abs() < 5.0,
        "expected ~1000 Hz, got {value}"
    );

    let _ = ok_request(&mut ws, 4, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// `acq.configure`/`acq.start` are refused with `device_busy` while a
/// device is recording (SPEC §2.8), in both directions: a device already
/// configured for a finite acquisition before the recording started, and a
/// fresh `acq.configure` attempted after.
#[tokio::test]
async fn acq_operations_are_refused_while_the_device_is_recording() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    let device = digital_device();

    let _ = ok_request(
        &mut ws,
        1,
        json!({
            "op": "acq.configure", "device": device,
            "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 100 }
        }),
    )
    .await;

    let start = ok_request(
        &mut ws,
        2,
        json!({
            "op": "rec.start", "device": device,
            "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    let acq_start = request(&mut ws, 3, json!({ "op": "acq.start", "device": device })).await;
    assert!(!acq_start.ok);
    let error = acq_start.error.unwrap();
    assert_eq!(error.code, "device_busy");
    // The message has to be actionable on its own (live test B-31). The old
    // one said only "call rec.stop first", which is useless to someone whose
    // browser tab closed mid-recording: they cannot see the recording, so they
    // have nothing to pass to rec.stop.
    assert!(
        error.msg.contains(&format!("recording {rec}")),
        "the error must name the recording holding the device: {}",
        error.msg
    );
    assert!(
        error.msg.contains(&format!("rec.stop rec={rec}")),
        "and the exact call that frees it: {}",
        error.msg
    );
    assert!(
        error.msg.contains("rec.list"),
        "and how to find one that is not already known: {}",
        error.msg
    );

    let acq_configure = request(
        &mut ws,
        4,
        json!({
            "op": "acq.configure", "device": device,
            "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 100 }
        }),
    )
    .await;
    assert!(!acq_configure.ok);
    assert_eq!(acq_configure.error.unwrap().code, "device_busy");

    let _ = ok_request(&mut ws, 5, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// Eviction under a tiny memory budget: `first_sample` keeps advancing
/// (data falls off the left) while the recording keeps growing on the
/// right, and total usage never runs away.
#[tokio::test]
async fn eviction_under_a_tiny_memory_budget() {
    let (url, _server) = spawn_server("[recorder]\nmemory_mb = 0\n").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": analog_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [0, 1], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    // Eviction needs at least one closed 64 Ki-sample block, and how soon the
    // fake instrument delivers one depends on machine load: poll for the
    // effect rather than sleeping a fixed time and hoping.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut id = 2;
    let r = loop {
        let list = ok_request(&mut ws, id, json!({ "op": "rec.list" })).await;
        id += 1;
        let r = list["recordings"]
            .as_array()
            .expect("recordings array")
            .iter()
            .find(|r| r["rec"].as_u64().unwrap() as u16 == rec)
            .expect("our recording is listed")
            .clone();
        if r["first_sample"].as_u64().unwrap() > 0 || tokio::time::Instant::now() > deadline {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let first_sample = r["first_sample"].as_u64().unwrap();
    let head_sample = r["head_sample"].as_u64().unwrap();
    assert!(
        head_sample > first_sample,
        "recording should hold some data: {r:?}"
    );
    assert!(
        first_sample > 0,
        "a near-zero budget should have evicted from the left within 10 s: {r:?}"
    );

    let _ = ok_request(&mut ws, id, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// `rec.state` is published when a recording ends, and the data stays
/// listed (queryable) afterward until `rec.delete`.
#[tokio::test]
async fn rec_state_event_is_published_when_a_recording_stops() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    let _ = ok_request(&mut ws, 2, json!({ "op": "rec.stop", "rec": rec })).await;

    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ClientMessage::Text(text) = ws.next().await.unwrap().unwrap() {
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                if v.get("event").and_then(Value::as_str) == Some("rec.state")
                    && v.get("rec").and_then(Value::as_u64) == Some(rec as u64)
                {
                    return v;
                }
            }
        }
    })
    .await
    .expect("rec.state event never arrived");
    assert_eq!(event["state"], "stopped");

    // Still listed and queryable after stopping.
    let list = ok_request(&mut ws, 3, json!({ "op": "rec.list" })).await;
    let recordings = list["recordings"].as_array().unwrap();
    assert!(recordings
        .iter()
        .any(|r| r["rec"].as_u64().unwrap() as u16 == rec));

    let _ = ok_request(&mut ws, 4, json!({ "op": "rec.delete", "rec": rec })).await;
    let list = ok_request(&mut ws, 5, json!({ "op": "rec.list" })).await;
    let recordings = list["recordings"].as_array().unwrap();
    assert!(!recordings
        .iter()
        .any(|r| r["rec"].as_u64().unwrap() as u16 == rec));
}

/// `rec.delete` is refused while a recording is still active.
#[tokio::test]
async fn rec_delete_is_refused_while_recording() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    let resp = request(&mut ws, 2, json!({ "op": "rec.delete", "rec": rec })).await;
    assert!(!resp.ok);
    assert_eq!(resp.error.unwrap().code, "recording_active");

    let _ = ok_request(&mut ws, 3, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// B-24 (live test round 3): an agent searched a recording that had died
/// 40 s earlier, got an empty list, and read it as "nothing happened". A
/// result over a recording that is no longer live must say when it ended —
/// and a live one must not carry the note.
#[tokio::test]
async fn a_search_over_a_recording_that_has_ended_says_so() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [4], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let live = ok_request(
        &mut ws,
        2,
        json!({ "op": "rec.search", "rec": rec, "pattern": "D4 rising", "max_matches": 1 }),
    )
    .await;
    assert!(
        live.get("note").is_none(),
        "a live recording carries no note: {live}"
    );

    ok_request(&mut ws, 3, json!({ "op": "rec.stop", "rec": rec })).await;
    let ended = ok_request(
        &mut ws,
        4,
        json!({ "op": "rec.search", "rec": rec, "pattern": "D4 rising", "max_matches": 1 }),
    )
    .await;
    let note = ended["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("not live") && note.contains("stopped at"),
        "{ended}"
    );

    // The same for a decode over it.
    let decoded = ok_request(
        &mut ws,
        5,
        json!({ "op": "rec.decode", "rec": rec, "decoder": "uart",
                "channels": [{ "role": "rx", "channel": 4 }], "max_annotations": 1 }),
    )
    .await;
    assert!(
        decoded["note"]
            .as_str()
            .unwrap_or_default()
            .contains("not live"),
        "{decoded}"
    );
}

/// B-59: `recorder::start` checked "one recording per device" and then spent
/// three awaits opening, configuring and starting the device before the
/// recording was registered. Two `rec.start`s ten milliseconds apart both got
/// through that window on the bench, the second reconfigured the instrument
/// under the first, and the first was left in `error` holding nothing.
///
/// Sent down two connections without waiting for either reply, so both are in
/// flight at once: exactly one may win, and the loser must be told why.
#[tokio::test]
async fn two_concurrent_rec_starts_on_one_device_leave_exactly_one_recording() {
    let (url, _server) = spawn_server("").await;
    let (mut first, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    let (mut second, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    let device = digital_device();
    let body = json!({
        "v": 1, "id": 1, "op": "rec.start", "device": device,
        "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 0, "continuous": true }
    });

    first.send(send(body.clone())).await.expect("send");
    second.send(send(body)).await.expect("send");
    let a = recv_response(&mut first).await;
    let b = recv_response(&mut second).await;

    assert!(
        a.ok ^ b.ok,
        "exactly one start may win: a.ok={} b.ok={} ({:?} / {:?})",
        a.ok,
        b.ok,
        a.error,
        b.error
    );
    let refused = if a.ok { b } else { a };
    let error = refused.error.expect("the loser carries an error");
    assert_eq!(error.code, "device_busy", "{}", error.msg);
    // Either the winner was already registered, or it is still setting up —
    // both messages have to say what to do next, not just "retry".
    assert!(
        error.msg.contains("rec.list")
            || error.msg.contains("rec.stop")
            || error.msg.contains("retry"),
        "unhelpful refusal: {}",
        error.msg
    );

    // And the device really does hold one recording, not two.
    let list = ok_request(&mut first, 2, json!({ "op": "rec.list" })).await;
    let live: Vec<&Value> = list["recordings"]
        .as_array()
        .expect("recordings array")
        .iter()
        .filter(|r| r["device"] == device.as_str() && r["state"] == "recording")
        .collect();
    assert_eq!(
        live.len(),
        1,
        "one recording per device (SPEC §2.8): {live:?}"
    );
}
