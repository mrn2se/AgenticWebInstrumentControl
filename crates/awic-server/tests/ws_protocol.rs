//! Integration tests over a real `/ws` WebSocket connection
//! (SPECIFICATION.md §3):
//! `device.list` -> `acq.configure` -> `acq.start` against a test-only
//! backend (`support::TestBackend` — `awic-fake`/`awic-usb` are separate
//! crates this server never depends on), checking the resulting binary
//! sample frames, error handling for a bad request, `id` correlation across
//! concurrent in-flight requests, and the optional bearer-token gate.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use awic_core::Config;
use awic_proto::frame::{decode_frame, Encoding};
use awic_proto::{Response, ResponseResult};
use awic_server::state::AppState;
use awic_server::usb::UnimplementedUsbController;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

use support::TestBackend;

/// Starts a server on an ephemeral port with `config_toml` applied, returns
/// its `ws://` base URL and the task serving it (kept alive for the
/// duration of the test by holding the `JoinHandle`).
async fn spawn_server(config_toml: &str) -> (String, tokio::task::JoinHandle<()>) {
    let config = Config::from_toml_str(config_toml).expect("valid test config");
    let state = AppState::new(
        config,
        Arc::new(TestBackend::new()),
        Arc::new(UnimplementedUsbController),
    );
    state.bootstrap().await;

    let router = awic_server::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        // `ws_handler` requires `ConnectInfo<SocketAddr>` (SPEC §12's
        // per-mutation origin logging) — wire it up here exactly as
        // `main.rs` does, so every request through this real socket gets a
        // peer address instead of every upgrade being rejected.
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("server error");
    });
    (format!("ws://{addr}/ws"), handle)
}

/// Reads WebSocket messages until a `Text` control response arrives, and
/// decodes it as a [`Response`]. Any binary frames seen along the way (e.g.
/// sample data racing ahead of a later request) are ignored — callers that
/// care about binary frames read those explicitly instead.
async fn recv_response<S>(ws: &mut S) -> Response
where
    S: StreamExt<Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a response")
            .expect("connection closed unexpectedly")
            .expect("websocket error");
        if let ClientMessage::Text(text) = msg {
            return serde_json::from_str(text.as_str()).expect("valid Response JSON");
        }
    }
}

fn send(text: serde_json::Value) -> ClientMessage {
    ClientMessage::Text(text.to_string().into())
}

#[tokio::test]
async fn full_acquisition_flow_over_real_websocket() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _http_response) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    // device.list
    ws.send(send(json!({"v": 1, "id": 1, "op": "device.list"})))
        .await
        .unwrap();
    let resp = recv_response(&mut ws).await;
    assert!(resp.ok, "device.list failed: {:?}", resp.error);
    let devices = match resp.result.unwrap() {
        ResponseResult::DeviceList(d) => d.devices,
        other => panic!("unexpected result: {other:?}"),
    };
    assert_eq!(devices.len(), 1);
    let device_id = devices[0].id.clone();

    // acq.configure
    ws.send(send(json!({
        "v": 1, "id": 2, "op": "acq.configure", "device": device_id,
        "cfg": { "rate_hz": 1_000_000, "channels": [0, 1], "samples": 8192, "trigger": null }
    })))
    .await
    .unwrap();
    let resp = recv_response(&mut ws).await;
    assert!(resp.ok, "acq.configure failed: {:?}", resp.error);

    // acq.start
    ws.send(send(
        json!({"v": 1, "id": 3, "op": "acq.start", "device": device_id}),
    ))
    .await
    .unwrap();
    let resp = recv_response(&mut ws).await;
    assert!(resp.ok, "acq.start failed: {:?}", resp.error);
    let (stream_id, capture) = match resp.result.unwrap() {
        ResponseResult::AcqStart(r) => (r.stream, r.capture),
        other => panic!("unexpected result: {other:?}"),
    };
    assert!(capture.starts_with("capture://"));

    // Collect several binary sample frames and check the header contract.
    let mut last_seq: Option<u32> = None;
    let mut frames_checked = 0;
    while frames_checked < 5 {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a sample frame")
            .expect("connection closed")
            .expect("websocket error");
        match msg {
            ClientMessage::Binary(bytes) => {
                let (header, _payload) = decode_frame(&bytes).expect("valid frame header");
                assert_eq!(header.stream_id, stream_id);
                assert_eq!(header.channel_count, 2);
                assert_eq!(header.encoding, Encoding::U8Interleaved);
                if let Some(prev) = last_seq {
                    assert!(
                        header.sequence > prev,
                        "sequence numbers must be strictly increasing: {prev} then {}",
                        header.sequence
                    );
                }
                last_seq = Some(header.sequence);
                frames_checked += 1;
            }
            // acq.state events etc. — ignore, this test only checks frames.
            ClientMessage::Text(_) => {}
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(last_seq.is_some());
}

#[tokio::test]
async fn unknown_op_returns_a_stable_error_code_without_closing_the_connection() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    ws.send(send(json!({"v": 1, "id": 9, "op": "totally.unknown"})))
        .await
        .unwrap();
    let resp = recv_response(&mut ws).await;
    assert!(!resp.ok);
    assert_eq!(resp.id, 9);
    assert_eq!(resp.error.unwrap().code, "unknown_op");

    // The connection must still be usable afterwards.
    ws.send(send(json!({"v": 1, "id": 10, "op": "device.list"})))
        .await
        .unwrap();
    let resp = recv_response(&mut ws).await;
    assert!(resp.ok, "connection should still work: {:?}", resp.error);
    assert_eq!(resp.id, 10);
}

#[tokio::test]
async fn concurrent_in_flight_requests_get_correlated_by_id() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    // Fired back-to-back before either has a chance to respond; dispatch
    // spawns each independently, so nothing guarantees response order.
    ws.send(send(json!({"v": 1, "id": 100, "op": "device.list"})))
        .await
        .unwrap();
    ws.send(send(json!({"v": 1, "id": 200, "op": "usb.tree"})))
        .await
        .unwrap();

    let mut seen_ids = HashSet::new();
    for _ in 0..2 {
        let resp = recv_response(&mut ws).await;
        seen_ids.insert(resp.id);
    }
    assert_eq!(seen_ids, HashSet::from([100, 200]));
}

#[tokio::test]
async fn ws_upgrade_is_rejected_without_the_configured_token() {
    let (url, _server) = spawn_server(
        r#"
        [auth]
        token = "s3cret"
        "#,
    )
    .await;

    tokio_tungstenite::connect_async(&url)
        .await
        .expect_err("connecting without a token must fail");

    // The correct token, passed as a query parameter (browser WebSocket
    // clients cannot set an Authorization header on the upgrade request),
    // is accepted.
    let authed_url = format!("{url}?token=s3cret");
    let (mut ws, _) = tokio_tungstenite::connect_async(&authed_url)
        .await
        .expect("authorized connection should succeed");
    ws.send(send(json!({"v": 1, "id": 1, "op": "device.list"})))
        .await
        .unwrap();
    let resp = recv_response(&mut ws).await;
    assert!(resp.ok);
}

#[tokio::test]
async fn ws_upgrade_succeeds_when_no_token_is_configured() {
    let (url, _server) = spawn_server("").await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect without any credentials");
    ws.send(send(json!({"v": 1, "id": 1, "op": "device.list"})))
        .await
        .unwrap();
    let resp = recv_response(&mut ws).await;
    assert!(resp.ok);
}

/// SPEC §12: browsers apply no same-origin policy to WebSocket upgrades, so
/// `/ws` itself must refuse a page from another site — otherwise any website
/// the bench user visits could switch USB power. Non-browser clients send no
/// `Origin` and pass; the UI served by this server is same-origin and passes.
#[tokio::test]
async fn ws_upgrade_refuses_foreign_browser_origins() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Error as WsError;

    let (url, _server) = spawn_server("").await;
    let host = url
        .trim_start_matches("ws://")
        .trim_end_matches("/ws")
        .to_string();
    let with_origin = |origin: &str| {
        let mut req = url.as_str().into_client_request().expect("request");
        req.headers_mut()
            .insert("Origin", origin.parse().expect("header value"));
        req
    };

    match tokio_tungstenite::connect_async(with_origin("https://evil.example")).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), 403),
        other => panic!("a foreign origin must be refused with 403, got {other:?}"),
    }
    tokio_tungstenite::connect_async(with_origin(&format!("http://{host}")))
        .await
        .expect("the server's own origin is accepted");
    tokio_tungstenite::connect_async(url.as_str())
        .await
        .expect("a client without Origin (script, agent) is accepted");
}
