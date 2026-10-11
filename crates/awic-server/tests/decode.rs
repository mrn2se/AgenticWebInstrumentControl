//! Integration tests for protocol decoding (SPECIFICATION.md §6, T21):
//! `capture.decode`, `rec.decode`, `decode.list` and the `decode` terminal
//! source, all over a real `/ws` connection driven by `awic-fake` — no
//! hardware needed (DEVELOPMENT.md §1).
//!
//! Leans on one `awic-fake` fact (its own doc comment, `crates/awic-fake/src/digital.rs`):
//! the default digital device's **D5** channel carries a genuinely decodable
//! UART byte stream (`b"AWIC"` at 9600 baud, idle-high 8N1) rather than the
//! plain ripple-counter square wave every other bit shows — added by this
//! task specifically so decoders have something protocol-shaped to decode.

use std::sync::Arc;
use std::time::Duration;

use awic_core::Config;
use awic_fake::{AnalogDeviceSpec, DigitalDeviceSpec, FakeBackend};
use awic_proto::term_frame::{decode_header, HEADER_LEN};
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

async fn request(ws: &mut WsStream, id: u32, mut body: Value) -> Response {
    body["v"] = json!(1);
    body["id"] = json!(id);
    ws.send(send(body)).await.expect("send");
    recv_response(ws).await
}

async fn ok_request(ws: &mut WsStream, id: u32, body: Value) -> Value {
    let resp = request(ws, id, body.clone()).await;
    assert!(
        resp.ok,
        "request {id} ({body:?} was sent) failed: {:?}",
        resp.error
    );
    serde_json::to_value(resp.result.unwrap()).unwrap()
}

async fn err_request(ws: &mut WsStream, id: u32, body: Value) -> awic_proto::ErrorInfo {
    let resp = request(ws, id, body.clone()).await;
    assert!(!resp.ok, "request {id} ({body:?}) unexpectedly succeeded");
    resp.error.unwrap()
}

fn digital_device() -> String {
    DigitalDeviceSpec::logic_8ch_like().id.to_string()
}

fn analog_device() -> String {
    AnalogDeviceSpec::scope_2ch_like().id.to_string()
}

/// Reads text frames until an `event` with the given name and a matching
/// `stream`/`rec` field (checked via `matches`) arrives.
async fn wait_for_event(ws: &mut WsStream, event: &str, matches: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ClientMessage::Text(text) = ws.next().await.unwrap().unwrap() {
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                if v.get("event").and_then(Value::as_str) == Some(event) && matches(&v) {
                    return v;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{event} event never arrived"))
}

/// The UART option settings matching `awic-fake`'s D5 source
/// (`D5_UART_BAUD` = 9600; everything else — 8N1, LSB-first, idle-high — is
/// the decoder's own default, matching `UartSource`'s framing).
fn uart_options() -> Value {
    json!([{ "id": "baud", "value": 9600 }])
}

fn uart_channels(hw_channel: u8) -> Value {
    json!([{ "role": "rx", "channel": hw_channel }])
}

/// Every `Data` annotation's byte value, in order — the shape every
/// decode test below checks against `b"AWIC"`.
fn data_bytes(annotations: &[Value]) -> Vec<u8> {
    annotations
        .iter()
        .filter(|a| a["kind"]["kind"] == "data")
        .map(|a| a["kind"]["value"].as_u64().unwrap() as u8)
        .collect()
}

#[tokio::test]
async fn decode_list_reports_all_four_decoders_with_roles_and_options() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let result = ok_request(&mut ws, 1, json!({ "op": "decode.list" })).await;
    let decoders = result["decoders"].as_array().unwrap();
    let ids: Vec<&str> = decoders.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["uart", "i2c", "spi", "can", "one_wire"]);

    let uart = &decoders[0];
    let roles: Vec<&str> = uart["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["rx"]);
    let option_ids: Vec<&str> = uart["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert!(option_ids.contains(&"baud"));
    assert!(option_ids.contains(&"parity"));
}

/// `capture.decode` over a finished capture of the fake UART-like signal.
#[tokio::test]
async fn capture_decode_recovers_the_fake_uart_message() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let device = digital_device();
    ok_request(
        &mut ws,
        1,
        json!({
            "op": "acq.configure", "device": device,
            "cfg": { "rate_hz": 1_000_000, "channels": [5], "samples": 50_000, "trigger": null }
        }),
    )
    .await;
    let start = ok_request(&mut ws, 2, json!({ "op": "acq.start", "device": device })).await;
    let stream = start["stream"].as_u64().unwrap();
    let capture = start["capture"].as_str().unwrap().to_string();

    wait_for_event(&mut ws, "acq.state", |v| {
        v["stream"].as_u64() == Some(stream) && v["state"] == "stopped"
    })
    .await;

    let result = ok_request(
        &mut ws,
        3,
        json!({
            "op": "capture.decode", "capture": capture, "decoder": "uart",
            "channels": uart_channels(5), "options": uart_options()
        }),
    )
    .await;
    assert_eq!(result["truncated"], false, "{result:?}");
    let annotations = result["annotations"].as_array().unwrap();
    let bytes = data_bytes(annotations);
    assert!(!bytes.is_empty(), "{annotations:?}");
    // The message repeats forever; every 4-byte window must be b"AWIC".
    assert!(
        bytes.chunks(4).all(|w| w == b"AWIC"),
        "decoded bytes were not the repeating AWIC message: {bytes:?}"
    );
}

#[tokio::test]
async fn capture_decode_on_an_analog_capture_thresholds_it_first() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let device = analog_device();
    ok_request(
        &mut ws,
        1,
        json!({
            "op": "acq.configure", "device": device,
            "cfg": { "rate_hz": 1_000_000, "channels": [0], "samples": 1000, "trigger": null }
        }),
    )
    .await;
    let start = ok_request(&mut ws, 2, json!({ "op": "acq.start", "device": device })).await;
    let stream = start["stream"].as_u64().unwrap();
    let capture = start["capture"].as_str().unwrap().to_string();
    wait_for_event(&mut ws, "acq.state", |v| {
        v["stream"].as_u64() == Some(stream) && v["state"] == "stopped"
    })
    .await;

    // Not a UART line, but decoded all the same: the result says how the
    // waveform was thresholded, and the analog options are accepted.
    let mut options = uart_options();
    options
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "threshold_mv", "value": 100 }));
    let result = ok_request(
        &mut ws,
        3,
        json!({
            "op": "capture.decode", "capture": capture, "decoder": "uart",
            "channels": uart_channels(0), "options": options
        }),
    )
    .await;
    let note = result["note"].as_str().expect("a note on an analog decode");
    assert!(
        note.starts_with("analog: CH1 threshold 0.100 V (given)"),
        "{note}"
    );
    // `minus` is understood on the wire (here pointing at the channel
    // itself, which is refused as a nonsense pair, not as an unknown field).
    let err = err_request(
        &mut ws,
        4,
        json!({
            "op": "capture.decode", "capture": capture, "decoder": "uart",
            "channels": [{ "role": "rx", "channel": 0, "minus": 0 }],
            "options": uart_options()
        }),
    )
    .await;
    assert_eq!(err.code, "invalid_decoder_config");
}

/// `rec.decode` over a live recording's range, streamed through the decoder
/// in bounded windows (never materializing the whole range at once).
#[tokio::test]
async fn rec_decode_over_a_range_recovers_the_fake_uart_message() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [5], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    // Let a few loops of the message accumulate.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let result = ok_request(
        &mut ws,
        2,
        json!({
            "op": "rec.decode", "rec": rec, "decoder": "uart",
            "channels": uart_channels(5), "options": uart_options()
        }),
    )
    .await;
    assert_eq!(result["truncated"], false, "{result:?}");
    let annotations = result["annotations"].as_array().unwrap();
    let bytes = data_bytes(annotations);
    assert!(bytes.len() >= 4, "{annotations:?}");
    assert!(
        bytes.chunks(4).all(|w| w == b"AWIC"),
        "decoded bytes were not the repeating AWIC message: {bytes:?}"
    );
    assert!(result["decoded"]["t1_s"].as_f64().unwrap() > 0.0);

    let _ = ok_request(&mut ws, 3, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// `max_annotations` bounds the result and reports how many were omitted
/// (SPEC T21: "say how many were omitted").
#[tokio::test]
async fn rec_decode_truncates_and_reports_how_many_were_omitted() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [5], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let result = ok_request(
        &mut ws,
        2,
        json!({
            "op": "rec.decode", "rec": rec, "decoder": "uart",
            "channels": uart_channels(5), "options": uart_options(),
            "max_annotations": 2
        }),
    )
    .await;
    assert_eq!(result["truncated"], true, "{result:?}");
    assert!(result["omitted"].as_u64().unwrap() > 0, "{result:?}");
    assert_eq!(result["annotations"].as_array().unwrap().len(), 2);

    let _ = ok_request(&mut ws, 3, json!({ "op": "rec.stop", "rec": rec })).await;
}

/// A `decode` terminal session opened on a live recording receives decoded
/// text as new runs arrive (SPEC §11.2): the same fake UART message, this
/// time as terminal bytes rather than a JSON annotation list.
#[tokio::test]
async fn decode_terminal_session_receives_decoded_text_while_a_recording_runs() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let start = ok_request(
        &mut ws,
        1,
        json!({
            "op": "rec.start", "device": digital_device(),
            "cfg": { "rate_hz": 1_000_000, "channels": [5], "samples": 0, "continuous": true }
        }),
    )
    .await;
    let rec = start["rec"].as_u64().unwrap() as u16;

    let opened = ok_request(
        &mut ws,
        2,
        json!({
            "op": "term.open", "source": "decode",
            "decode": {
                "rec": rec, "decoder": "uart",
                "channels": uart_channels(5), "options": uart_options()
            }
        }),
    )
    .await;
    assert_eq!(opened["kind"], "decode");
    let session = opened["session"].as_u64().unwrap() as u16;

    // Read binary term frames until "AWIC" shows up as decoded text.
    let text = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = String::new();
        loop {
            if let ClientMessage::Binary(bytes) = ws.next().await.unwrap().unwrap() {
                let header = decode_header(&bytes).expect("valid term frame");
                if header.session_id != session {
                    continue;
                }
                collected.push_str(&String::from_utf8_lossy(&bytes[HEADER_LEN..]));
                if collected.contains("AWIC") {
                    return collected;
                }
            }
        }
    })
    .await
    .expect("decoded UART text never arrived on the terminal session");
    assert!(text.contains("AWIC"), "{text:?}");

    // The session ends cleanly when the recording stops (SPEC §11.2).
    let _ = ok_request(&mut ws, 3, json!({ "op": "rec.stop", "rec": rec })).await;
    wait_for_event(&mut ws, "term.state", |v| {
        v["session"].as_u64() == Some(session as u64) && v["state"] == "closed"
    })
    .await;
}
