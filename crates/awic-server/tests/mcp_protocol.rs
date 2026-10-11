//! Integration tests for the MCP transport (SPECIFICATION.md §8,
//! M7): `POST /mcp` driven through the real
//! `axum::Router` `build_router` returns (no separate implementation to
//! drift from `app.rs`), backed by `awic-fake`'s synthetic instruments —
//! the same "no test may require hardware" rule every other transport's
//! tests follow (`tests/scpi_tcp.rs`, `tests/recorder.rs`).
//!
//! Requests go through the router via `tower::ServiceExt::oneshot` rather
//! than a real `TcpListener`: `/mcp` is plain request/response HTTP (no
//! upgrade, unlike `/ws`), so there is nothing a real socket would exercise
//! that `oneshot` doesn't. The stdio shim's own HTTP client *is* tested
//! against a real listening server, in `tests/mcp_stdio.rs`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awic_core::Config;
use awic_fake::FakeBackend;
use awic_proto::{UsbPort, UsbPowerResult, UsbProtectResult, UsbTreeNode, UsbTreeResult};
use awic_server::app::build_router;
use awic_server::error::UsbOpError;
use awic_server::state::AppState;
use awic_server::usb::UsbController;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

/// A hub with one port already protected (feeds a fake "hub" downstream, so
/// `set_usb_power` on it must be refused) and one ordinary switchable port —
/// enough to exercise both the refusal and the success path without needing
/// `awic-usb`.
struct FixedUsb;

#[async_trait]
impl UsbController for FixedUsb {
    async fn tree(&self) -> Result<UsbTreeResult, UsbOpError> {
        Ok(UsbTreeResult {
            roots: vec![UsbTreeNode {
                bus: 3,
                port_path: vec![3],
                vid: 0x1d6b,
                pid: 0x0002,
                product: Some("hub".to_string()),
                speed: Some("480".to_string()),
                is_hub: true,
                ppps: Some(true),
                is_instrument: false,
                ports: vec![
                    UsbPort {
                        number: 4,
                        power: Some(true),
                        power_switchable: true,
                        protected: true,
                        protection: Some("a hub is attached".into()),
                        derived_protection: Some("a hub is attached".into()),
                        user_choice: None,
                        label: None,
                        dut: false,
                        switchable: false,
                        non_switchable_reason: Some("a hub is attached".into()),
                        device: None,
                    },
                    UsbPort {
                        number: 1,
                        power: Some(true),
                        power_switchable: true,
                        protected: false,
                        protection: None,
                        derived_protection: None,
                        user_choice: None,
                        label: None,
                        dut: false,
                        switchable: true,
                        non_switchable_reason: None,
                        device: None,
                    },
                ],
            }],
        })
    }

    async fn power(&self, hub: &str, port: u8, on: bool) -> Result<UsbPowerResult, UsbOpError> {
        if port == 4 {
            return Err(UsbOpError {
                code: "port_protected".into(),
                msg: format!("hub {hub:?} port {port} is protected: a hub is attached"),
            });
        }
        Ok(UsbPowerResult {
            hub: hub.to_string(),
            port,
            on,
        })
    }

    async fn protect(
        &self,
        _hub: &str,
        _port: u8,
        _protected: Option<bool>,
    ) -> Result<UsbProtectResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.protect"))
    }
}

async fn router_with_config(config_toml: &str) -> Router {
    let config = Config::from_toml_str(config_toml).expect("valid test config");
    let state = AppState::new(config, Arc::new(FakeBackend::new()), Arc::new(FixedUsb));
    state.bootstrap().await; // discovers the fake analog + digital devices
    build_router(state)
}

async fn router() -> Router {
    router_with_config("").await
}

/// Sends one HTTP request to the router and returns (status, JSON body,
/// `Mcp-Session-Id` response header, if any).
async fn call(router: &Router, req: Request<Body>) -> (StatusCode, Value, Option<String>) {
    let response = router.clone().oneshot(req).await.expect("router call");
    let status = response.status();
    let session = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        // The auth middleware's own rejection is plain text, not JSON-RPC
        // (SPEC §12's optional token is enforced ahead of this router ever
        // seeing the request) — fall back to a string so tests can still
        // assert on its content.
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, body, session)
}

fn post(body: Value, session: Option<&str>, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        builder = builder.header("Mcp-Session-Id", session);
    }
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn rpc(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Runs `initialize`, returning the minted session id — every other test
/// needs one before it can call anything else, exactly as a real client
/// would.
async fn initialize(router: &Router) -> String {
    let req = post(
        rpc(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "clientInfo": { "name": "test-client" } }),
        ),
        None,
        None,
    );
    let (status, body, session) = call(router, req).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(body["result"]["serverInfo"]["name"], "awic");
    session.expect("initialize must return a Mcp-Session-Id header")
}

async fn call_tool(router: &Router, session: &str, name: &str, arguments: Value) -> Value {
    let req = post(
        rpc(
            2,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        ),
        Some(session),
        None,
    );
    let (status, body, _) = call(router, req).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    body["result"].clone()
}

fn find_analog_device(devices: &[Value]) -> String {
    devices
        .iter()
        .find(|d| {
            d["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["kind"] == "analog")
        })
        .expect("FakeBackend always reports an analog device")["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn find_digital_device(devices: &[Value]) -> String {
    devices
        .iter()
        .find(|d| {
            d["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["kind"] == "digital")
        })
        .expect("FakeBackend always reports a digital device")["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn initialize_negotiates_protocol_version_and_mints_a_session() {
    let router = router().await;
    let session = initialize(&router).await;
    assert!(!session.is_empty());
}

#[tokio::test]
async fn a_request_before_initialize_is_refused() {
    let router = router().await;
    let req = post(rpc(1, "tools/list", Value::Null), None, None);
    let (status, body, _) = call(&router, req).await;
    // Still a well-formed JSON-RPC error, not a raw HTTP failure with no body.
    // 400, not 404: the header is missing rather than naming a session this
    // server does not have (B-69).
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("initialize"));
}

/// B-69: an unknown session was answered 400, the same as a missing header.
/// MCP 2025-06-18 (Streamable HTTP, session management) requires 404 for a
/// session the server no longer knows, and a client that gets one re-
/// initializes. On a 400 it does not: after awic restarted, every tool call
/// from an already-connected Claude Code failed until it was reconnected by
/// hand.
#[tokio::test]
async fn an_unknown_session_is_404_so_a_client_knows_to_re_initialize() {
    let router = router().await;
    // A plausible id this server has never minted — what a client holds after
    // the server restarts.
    let req = post(
        rpc(1, "tools/list", Value::Null),
        Some("2f8d1c40-0000-4000-8000-000000000000"),
        None,
    );
    let (status, body, _) = call(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let message = body["error"]["message"].as_str().expect("a JSON-RPC error");
    assert!(message.contains("unknown Mcp-Session-Id"), "{message}");
    assert!(
        message.contains("initialize"),
        "must say how to recover: {message}"
    );

    // A session this server *did* mint still works, so the 404 is about the
    // session and not about the route.
    let session = initialize(&router).await;
    let req = post(rpc(2, "tools/list", Value::Null), Some(&session), None);
    let (status, _, _) = call(&router, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn ping_and_unknown_notification_are_handled() {
    let router = router().await;
    let session = initialize(&router).await;

    let req = post(rpc(9, "ping", Value::Null), Some(&session), None);
    let (status, body, _) = call(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], json!({}));

    // A notification (no `id`) gets 202 and no JSON-RPC body at all.
    let notify = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let req = post(notify, Some(&session), None);
    let (status, body, _) = call(&router, req).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn tools_list_returns_every_expected_tool_with_a_valid_json_schema() {
    let router = router().await;
    let session = initialize(&router).await;

    let req = post(rpc(2, "tools/list", Value::Null), Some(&session), None);
    let (status, body, _) = call(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let tools = body["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    let expected = [
        "list_instruments",
        "usb_tree",
        "set_usb_power",
        "set_usb_port_protected",
        "configure_acquisition",
        "capture",
        "read_capture",
        "measure",
        "start_recording",
        "stop_recording",
        "delete_recording",
        "list_recordings",
        "search_pattern",
        "wait_for_pattern",
        "snapshot_recording",
        "serial_list",
        "serial_open",
        "serial_write",
        "serial_read",
        "serial_expect",
    ];
    for name in expected {
        assert!(
            names.contains(&name),
            "missing tool {name:?}: have {names:?}"
        );
    }

    for tool in tools {
        assert!(tool["description"].as_str().unwrap().len() > 20, "{tool:?}");
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{tool:?}");
        assert!(schema["properties"].is_object(), "{tool:?}");
    }
}

#[tokio::test]
async fn tools_call_round_trips_list_instruments() {
    let router = router().await;
    let session = initialize(&router).await;
    let result = call_tool(&router, &session, "list_instruments", json!({})).await;
    assert_eq!(result["isError"], false);
    let devices = result["structuredContent"]["devices"].as_array().unwrap();
    assert!(
        !devices.is_empty(),
        "FakeBackend must report at least one device"
    );
}

#[tokio::test]
async fn capture_then_measure_round_trips_against_the_fake_backend() {
    let router = router().await;
    let session = initialize(&router).await;

    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_analog_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );

    let configure = call_tool(
        &router,
        &session,
        "configure_acquisition",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0], "samples": 10_000 }),
    )
    .await;
    assert_eq!(configure["isError"], false, "{configure:?}");

    let captured = call_tool(
        &router,
        &session,
        "capture",
        json!({ "device": device, "timeout_s": 5.0 }),
    )
    .await;
    assert_eq!(captured["isError"], false, "{captured:?}");
    let capture_id = captured["structuredContent"]["capture"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(capture_id.starts_with("capture://"));
    // §8.3: a summary only, never bulk samples.
    assert!(captured["structuredContent"]["channels"][0]["results"].is_array());

    let measured = call_tool(
        &router,
        &session,
        "measure",
        json!({ "capture": capture_id, "channel": 0, "measurements": ["vpp", "frequency"] }),
    )
    .await;
    assert_eq!(measured["isError"], false, "{measured:?}");
    let results = measured["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);

    let preview = call_tool(
        &router,
        &session,
        "read_capture",
        json!({ "capture": capture_id, "channel": 0, "max_points": 32 }),
    )
    .await;
    assert_eq!(preview["isError"], false, "{preview:?}");
    let min = preview["structuredContent"]["min"].as_array().unwrap();
    assert!(min.len() <= 32 && !min.is_empty());
}

#[tokio::test]
async fn start_recording_then_search_pattern_finds_the_digital_devices_ripple_counter() {
    let router = router().await;
    let session = initialize(&router).await;

    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_digital_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );

    let started = call_tool(
        &router,
        &session,
        "start_recording",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0, 1, 2, 3] }),
    )
    .await;
    assert_eq!(started["isError"], false, "{started:?}");
    let rec = started["structuredContent"]["rec"].as_u64().unwrap();

    // Let the recorder ingest a bit of the ripple counter before searching.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let searched = call_tool(
        &router,
        &session,
        "search_pattern",
        json!({ "rec": rec, "pattern": "D0 rising" }),
    )
    .await;
    assert_eq!(searched["isError"], false, "{searched:?}");
    let matches = searched["structuredContent"]["matches"].as_array().unwrap();
    assert!(
        !matches.is_empty(),
        "D0 toggles constantly on the fake ripple counter: {searched:?}"
    );

    let stopped = call_tool(&router, &session, "stop_recording", json!({ "rec": rec })).await;
    assert_eq!(stopped["isError"], false, "{stopped:?}");

    // B-48: a stopped recording keeps its memory until deleted.
    let deleted = call_tool(&router, &session, "delete_recording", json!({ "rec": rec })).await;
    assert_eq!(deleted["isError"], false, "{deleted:?}");
    let listed = call_tool(&router, &session, "list_recordings", json!({})).await;
    let recs = listed["structuredContent"]["recordings"]
        .as_array()
        .unwrap();
    assert!(
        recs.iter().all(|r| r["rec"] != json!(rec)),
        "deleted recording still listed: {listed:?}"
    );
}

/// `list_decoders` then `decode` over a live recording of `awic-fake`'s
/// D5 UART-like signal (SPEC §6, §8.2, T21) — the same MCP round trip an
/// agent would use, bounded output included.
/// `decode` in its agent shape (live test F-02/F-05): transactions instead
/// of annotations, and a baud rate detected from the line rather than given.
/// `awic-fake`'s D5 carries `b"AWIC"` at 9600 baud (see `awic_fake::digital`).
#[tokio::test]
async fn decode_returns_transactions_and_detects_the_baud_rate() {
    let router = router().await;
    let session = initialize(&router).await;
    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_digital_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );
    let started = call_tool(
        &router,
        &session,
        "start_recording",
        // Every channel, not just the UART one: the ripple counters on
        // D0-D3 produce far more edges than D5 does, and auto-baud must
        // still find the UART line's own (live test, 2026-09-16).
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0, 1, 2, 3, 4, 5, 6, 7] }),
    )
    .await;
    let rec = started["structuredContent"]["rec"].as_u64().unwrap();
    let mut decoded = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        decoded = call_tool(
            &router,
            &session,
            "decode",
            json!({
                "rec": rec, "decoder": "uart",
                "channels": [{ "role": "rx", "channel": 5 }],
                "options": [{ "id": "baud", "value": "auto" }],
                "format": "transactions",
            }),
        )
        .await;
        // Too little traffic yet is a named error, not a wrong answer.
        if decoded["isError"] == false {
            break;
        }
    }
    assert_eq!(decoded["isError"], false, "{decoded:?}");
    let result = &decoded["structuredContent"];
    assert_eq!(result["detected_baud"], 9600, "{result}");
    let transactions = result["transactions"].as_array().expect("transactions");
    assert!(!transactions.is_empty(), "{result}");
    // B-43: the summary is there and counts the same transactions, each
    // folded repeat included (F-14).
    let listed: u64 = transactions
        .iter()
        .map(|t| t["repeat"].as_u64().unwrap_or(1))
        .sum();
    assert_eq!(
        result["summary"]["transactions"].as_u64(),
        Some(listed),
        "{result}"
    );
    assert_eq!(
        result["summary"]["folded"].as_u64().unwrap_or(0),
        listed - transactions.len() as u64,
        "{result}"
    );
    assert_eq!(result["summary"]["nacks"].as_u64(), Some(0), "{result}");
    // Every burst is the repeating message, as text and as hex.
    let text: String = transactions
        .iter()
        .map(|t| t["text"].as_str().unwrap_or_default())
        .collect();
    assert!(text.contains("AWIC"), "{text:?}");
    assert!(
        transactions[0]["data"]
            .as_str()
            .unwrap()
            .starts_with("41 57 49 43")
            || text.starts_with("WIC"),
        "{}",
        transactions[0]
    );
    // The transactions format carries only its own list (B-84).
    assert!(result.get("annotations").is_none(), "{result}");

    // The same decode in annotation form defaults to frames and fields
    // only: no bit-level annotations unless asked for.
    let annotated = call_tool(
        &router,
        &session,
        "decode",
        json!({
            "rec": rec, "decoder": "uart",
            "channels": [{ "role": "rx", "channel": 5 }],
            "options": [{ "id": "baud", "value": 9600 }],
        }),
    )
    .await;
    let anns = annotated["structuredContent"]["annotations"]
        .as_array()
        .unwrap();
    assert!(!anns.is_empty());
    assert!(
        anns.iter().all(|a| a["level"].as_u64().unwrap() <= 1),
        "default max_level is 1"
    );
    // A UART byte carries its character (live test B-12).
    assert!(
        anns.iter()
            .any(|a| a["text"].as_str().unwrap_or_default().contains('\'')),
        "{:?}",
        &anns[..anns.len().min(3)]
    );
}

/// `survey_channels` (live test F-04) over a recording: the fake digital
/// device drives a ripple counter on D0-D3 and a 9600-baud UART on D5, so
/// the survey must name the UART with its rate and describe the rest.
#[tokio::test]
async fn survey_channels_names_the_fake_devices_uart_and_clocks() {
    let router = router().await;
    let session = initialize(&router).await;
    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_digital_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );
    let started = call_tool(
        &router,
        &session,
        "start_recording",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0, 1, 2, 3, 4, 5, 6, 7] }),
    )
    .await;
    let rec = started["structuredContent"]["rec"].as_u64().unwrap();
    let mut survey = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        survey = call_tool(&router, &session, "survey_channels", json!({ "rec": rec })).await;
        assert_eq!(survey["isError"], false, "{survey:?}");
        if survey["structuredContent"]["guesses"]
            .as_array()
            .is_some_and(|g| g.iter().any(|g| g["protocol"] == "uart"))
        {
            break;
        }
    }
    let result = &survey["structuredContent"];
    let guesses = result["guesses"].as_array().expect("guesses");
    let uart = guesses
        .iter()
        .find(|g| g["protocol"] == "uart")
        .unwrap_or_else(|| panic!("no UART guess in {guesses:?}"));
    assert_eq!(uart["channel"], 5);
    assert_eq!(uart["baud"], 9600);

    let channels = result["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 8);
    let d5 = channels.iter().find(|c| c["channel"] == 5).unwrap();
    assert!(d5["summary"].as_str().unwrap().contains("9600"), "{d5}");
    // Every channel says something a reader can act on.
    for c in channels {
        assert!(!c["summary"].as_str().unwrap_or_default().is_empty(), "{c}");
    }

    // Giving two sources at once is a named argument error, not a guess.
    let both = call_tool(
        &router,
        &session,
        "survey_channels",
        json!({ "rec": rec, "capture": "capture://x" }),
    )
    .await;
    assert_eq!(both["isError"], true, "{both:?}");
}

#[tokio::test]
async fn list_decoders_then_decode_recovers_the_fake_uart_message() {
    let router = router().await;
    let session = initialize(&router).await;

    let decoders = call_tool(&router, &session, "list_decoders", json!({})).await;
    assert_eq!(decoders["isError"], false, "{decoders:?}");
    let ids: Vec<&str> = decoders["structuredContent"]["decoders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["uart", "i2c", "spi", "can", "one_wire"]);

    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_digital_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );
    let started = call_tool(
        &router,
        &session,
        "start_recording",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [5] }),
    )
    .await;
    assert_eq!(started["isError"], false, "{started:?}");
    let rec = started["structuredContent"]["rec"].as_u64().unwrap();

    // Poll rather than sleep a fixed time: how much of the fake's stream has
    // arrived depends on machine load, and a flat sleep makes this a race.
    let mut bytes: Vec<u8> = Vec::new();
    let mut decoded = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        decoded = call_tool(
            &router,
            &session,
            "decode",
            json!({
                "rec": rec, "decoder": "uart",
                "channels": [{ "role": "rx", "channel": 5 }],
                "options": [{ "id": "baud", "value": 9600 }]
            }),
        )
        .await;
        assert_eq!(decoded["isError"], false, "{decoded:?}");
        bytes = decoded["structuredContent"]["annotations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["kind"]["kind"] == "data")
            .map(|a| a["kind"]["value"].as_u64().unwrap() as u8)
            .collect();
        if bytes.len() >= 8 {
            break;
        }
    }
    assert!(bytes.len() >= 8, "{decoded:?}");
    assert!(
        bytes.chunks(4).all(|w| w == b"AWIC"),
        "decoded bytes were not the repeating AWIC message: {bytes:?}"
    );

    let stopped = call_tool(&router, &session, "stop_recording", json!({ "rec": rec })).await;
    assert_eq!(stopped["isError"], false, "{stopped:?}");
}

#[tokio::test]
async fn wait_for_pattern_times_out_cleanly_when_nothing_matches() {
    let router = router().await;
    let session = initialize(&router).await;

    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_digital_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );
    let started = call_tool(
        &router,
        &session,
        "start_recording",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0] }),
    )
    .await;
    let rec = started["structuredContent"]["rec"].as_u64().unwrap();

    // D0 toggles every sample at 1 MHz, so ">10ms" high can never occur —
    // guaranteed to time out rather than match, in well under the bound.
    let waited = call_tool(
        &router,
        &session,
        "wait_for_pattern",
        json!({ "rec": rec, "pattern": "D0 high >10ms", "timeout_ms": 300 }),
    )
    .await;
    assert_eq!(waited["isError"], false, "{waited:?}");
    assert_eq!(waited["structuredContent"]["timeout"], true, "{waited:?}");
}

#[tokio::test]
async fn set_usb_power_on_a_protected_port_is_refused_with_the_reason() {
    let router = router().await;
    let session = initialize(&router).await;

    let result = call_tool(
        &router,
        &session,
        "set_usb_power",
        json!({ "hub": "3-3", "port": 4, "on": false }),
    )
    .await;
    assert_eq!(result["isError"], true, "{result:?}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("port_protected"), "{text}");
    assert!(text.contains("protected"), "{text}");

    // The unprotected port on the same fixture must still succeed.
    let ok = call_tool(
        &router,
        &session,
        "set_usb_power",
        json!({ "hub": "3-3", "port": 1, "on": false }),
    )
    .await;
    assert_eq!(ok["isError"], false, "{ok:?}");
}

#[tokio::test]
async fn unknown_tool_is_an_mcp_tool_error_not_a_protocol_error() {
    let router = router().await;
    let session = initialize(&router).await;
    let result = call_tool(&router, &session, "not_a_real_tool", json!({})).await;
    assert_eq!(result["isError"], true);
}

#[tokio::test]
async fn missing_bearer_token_is_rejected_when_a_token_is_configured() {
    let router = router_with_config("[auth]\ntoken = \"secret\"\n").await;

    let req = post(
        rpc(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
        None,
        None,
    );
    let (status, _, _) = call(&router, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let req = post(
        rpc(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
        None,
        Some("secret"),
    );
    let (status, body, session) = call(&router, req).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(session.is_some());
}

/// Sessions are bounded (`MAX_SESSIONS`): `initialize` needs no prior state,
/// so an unbounded table would let any client grow server memory at will.
/// The least recently used session is the one evicted — a session in active
/// use survives a burst of new ones.
#[tokio::test]
async fn sessions_are_bounded_and_the_least_recently_used_is_evicted() {
    use awic_server::mcp::route::MAX_SESSIONS;

    let router = router().await;
    let first = initialize(&router).await;
    let active = initialize(&router).await;
    for _ in 0..MAX_SESSIONS {
        // Keep `active` in use while new sessions arrive.
        let (status, _, _) = call(
            &router,
            post(rpc(1, "ping", json!({})), Some(&active), None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        initialize(&router).await;
    }

    let (_, body, _) = call(&router, post(rpc(2, "ping", json!({})), Some(&first), None)).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("unknown Mcp-Session-Id"),
        "the idle first session should have been evicted: {body}"
    );
    let (status, body, _) = call(
        &router,
        post(rpc(3, "ping", json!({})), Some(&active), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("error").is_none(),
        "the active session must survive: {body}"
    );
}

/// B-42: `include_svg` used to send the document twice, as the image block and
/// as the `svg` field — and a third time in the text block. Now once.
#[tokio::test]
async fn render_view_sends_the_svg_once() {
    let router = router().await;
    let session = initialize(&router).await;
    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_analog_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );
    let configure = call_tool(
        &router,
        &session,
        "configure_acquisition",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0], "samples": 10_000 }),
    )
    .await;
    assert_eq!(configure["isError"], false, "{configure:?}");
    let captured = call_tool(
        &router,
        &session,
        "capture",
        json!({ "device": device, "timeout_s": 5.0 }),
    )
    .await;
    let capture = captured["structuredContent"]["capture"].as_str().unwrap();

    let plain = call_tool(
        &router,
        &session,
        "render_view",
        json!({ "capture": capture }),
    )
    .await;
    assert_eq!(plain["isError"], false, "{plain:?}");
    assert_eq!(plain["content"][1]["type"], "image");
    assert!(plain["structuredContent"].get("svg").is_none());

    let text = call_tool(
        &router,
        &session,
        "render_view",
        json!({ "capture": capture, "include_svg": true }),
    )
    .await;
    assert_eq!(text["isError"], false, "{text:?}");
    let svg = text["structuredContent"]["svg"].as_str().unwrap();
    assert!(svg.starts_with("<svg"));
    let blocks = text["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "no image block beside the text: {text:?}");
    // The text block is what many clients hand the model: the document asked
    // for has to be in it, once.
    let body = blocks[0]["text"].as_str().unwrap();
    assert_eq!(body.matches("<svg").count(), 1, "{body}");

    // F-09: `format: "png"` sends the same picture as a PNG image block, for
    // clients that cannot display SVG.
    let png = call_tool(
        &router,
        &session,
        "render_view",
        json!({ "capture": capture, "format": "png" }),
    )
    .await;
    assert_eq!(png["isError"], false, "{png:?}");
    assert_eq!(png["content"][1]["type"], "image");
    assert_eq!(png["content"][1]["mimeType"], "image/png");
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png["content"][1]["data"].as_str().unwrap())
        .unwrap();
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
    assert_eq!(png["structuredContent"]["bytes"], bytes.len());
    let refused = call_tool(
        &router,
        &session,
        "render_view",
        json!({ "capture": capture, "format": "png", "include_svg": true }),
    )
    .await;
    assert_eq!(refused["isError"], true, "{refused:?}");
}

/// SPEC §8.5: the MCP tab's monitor sees sessions, tool calls and the
/// pictures `render_view` returned, and answers over the same `mcp.*` ops
/// the web UI uses.
#[tokio::test]
async fn the_monitor_logs_sessions_tool_calls_and_pictures() {
    use awic_proto::{McpEntryKind, Request as WireRequest, ResponseResult};
    use awic_server::dispatch::handle_request;

    let config = Config::from_toml_str("").expect("valid test config");
    let state = AppState::new(config, Arc::new(FakeBackend::new()), Arc::new(FixedUsb));
    state.bootstrap().await;
    let router = build_router(state.clone());
    let (out_tx, _out_rx) = tokio::sync::mpsc::channel(16);
    let ask = |request: WireRequest| {
        let state = state.clone();
        let out_tx = out_tx.clone();
        async move {
            let response = handle_request(&state, &out_tx, 0, request).await;
            assert!(response.ok, "{:?}", response.error);
            response.result.unwrap()
        }
    };

    let session = initialize(&router).await;
    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_analog_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );
    call_tool(
        &router,
        &session,
        "configure_acquisition",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0], "samples": 10_000 }),
    )
    .await;
    let captured = call_tool(
        &router,
        &session,
        "capture",
        json!({ "device": device, "timeout_s": 5.0 }),
    )
    .await;
    let capture = captured["structuredContent"]["capture"].as_str().unwrap();
    call_tool(
        &router,
        &session,
        "render_view",
        json!({ "capture": capture }),
    )
    .await;
    let failed = call_tool(&router, &session, "no_such_tool", json!({})).await;
    assert_eq!(failed["isError"], true);

    let ResponseResult::McpStatus(status) = ask(WireRequest::McpStatus { v: 1, id: 1 }).await
    else {
        panic!("mcp.status answered with the wrong shape");
    };
    assert_eq!(status.endpoint_path, "/mcp");
    assert!(!status.auth_required);
    assert!(status.tool_count > 20);
    let live = &status.sessions[0];
    assert!(live.active);
    assert_eq!(live.client, "test-client");
    assert_eq!(live.tool_calls, 5);
    assert_eq!(live.errors, 1);
    assert_eq!(live.last_tool.as_deref(), Some("no_such_tool"));

    let ResponseResult::McpLog(log) = ask(WireRequest::McpLog {
        v: 1,
        id: 2,
        after: None,
        limit: None,
    })
    .await
    else {
        panic!("mcp.log answered with the wrong shape");
    };
    let tools: Vec<_> = log
        .entries
        .iter()
        .filter_map(|e| e.tool.as_deref())
        .collect();
    assert_eq!(
        tools,
        [
            "list_instruments",
            "configure_acquisition",
            "capture",
            "render_view",
            "no_such_tool"
        ]
    );
    assert_eq!(log.entries[0].method, "initialize");
    let render = log
        .entries
        .iter()
        .find(|e| e.tool.as_deref() == Some("render_view"))
        .unwrap();
    assert_eq!(render.images.len(), 1);
    assert!(render.images[0].kept);
    assert!(log.entries.last().unwrap().is_error);

    let ResponseResult::McpImage(image) = ask(WireRequest::McpImage {
        v: 1,
        id: 3,
        seq: render.seq,
        index: 0,
    })
    .await
    else {
        panic!("mcp.image answered with the wrong shape");
    };
    assert_eq!(image.mime_type, "image/svg+xml");
    assert!(!image.data_b64.is_empty());

    // Switched off, calls are counted but not logged.
    ask(WireRequest::McpLogging {
        v: 1,
        id: 4,
        enabled: Some(false),
        images: None,
        clear: Some(true),
    })
    .await;
    call_tool(&router, &session, "list_instruments", json!({})).await;
    assert!(state.mcp_monitor().log(None, None).entries.is_empty());
    assert_eq!(state.mcp_monitor().sessions()[0].tool_calls, 6);

    // A DELETE ends the session; a request without one is logged as rejected.
    state.mcp_monitor().set_logging(Some(true), None, false);
    let delete = Request::builder()
        .method("DELETE")
        .uri("/mcp")
        .header("Mcp-Session-Id", &session)
        .body(Body::empty())
        .unwrap();
    call(&router, delete).await;
    call(&router, post(rpc(9, "tools/list", json!({})), None, None)).await;
    let sessions = state.mcp_monitor().sessions();
    assert!(!sessions[0].active);
    let kinds: Vec<_> = state
        .mcp_monitor()
        .log(None, None)
        .entries
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds, [McpEntryKind::SessionEnd, McpEntryKind::Rejected]);
}

/// B-73: `start_recording` used to drop `ranges_v` without a word, so an
/// analog recording always ran at the default range. It now reaches the
/// device (an unsupported range is the device's refusal), and an argument the
/// tool does not know is refused instead of ignored.
#[tokio::test]
async fn start_recording_takes_ranges_v_and_refuses_unknown_arguments() {
    let router = router().await;
    let session = initialize(&router).await;

    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let devices = instruments["structuredContent"]["devices"]
        .as_array()
        .unwrap()
        .clone();
    let device = find_analog_device(&devices);
    let ranges = devices.iter().find(|d| d["id"] == json!(device)).unwrap()["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "analog")
        .unwrap()["ranges_v"]
        .as_array()
        .unwrap()
        .clone();
    let last = ranges.last().unwrap().as_f64().unwrap();

    let unknown = call_tool(
        &router,
        &session,
        "start_recording",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0], "range_v": last }),
    )
    .await;
    assert_eq!(unknown["isError"], true, "{unknown:?}");

    let unsupported = call_tool(
        &router,
        &session,
        "start_recording",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0], "ranges_v": [123.0] }),
    )
    .await;
    assert_eq!(unsupported["isError"], true, "{unsupported:?}");

    let started = call_tool(
        &router,
        &session,
        "start_recording",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0], "ranges_v": [last] }),
    )
    .await;
    assert_eq!(started["isError"], false, "{started:?}");
    let effective = &started["structuredContent"]["cfg"]["ranges_v"];
    assert_eq!(effective[0].as_f64(), Some(last), "{started:?}");

    let rec = started["structuredContent"]["rec"].as_u64().unwrap();
    // Every tool refuses what it does not know, not only this one.
    let misspelled = call_tool(
        &router,
        &session,
        "delete_recording",
        json!({ "rec": rec, "force": true }),
    )
    .await;
    assert_eq!(misspelled["isError"], true, "{misspelled:?}");

    let deleted = call_tool(&router, &session, "delete_recording", json!({ "rec": rec })).await;
    assert_eq!(deleted["isError"], false, "{deleted:?}");
}

/// F-15: `initialize` carries the server's `instructions`: the workflow and
/// the channel numbering, for clients that only show a model the tools it
/// loaded.
#[tokio::test]
async fn initialize_sends_instructions() {
    let router = router().await;
    let req = post(
        rpc(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
        None,
        None,
    );
    let (status, body, _) = call(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body["result"]["instructions"].as_str().unwrap();
    for needle in [
        "survey_channels",
        "decode",
        "render_view",
        "D4 is 4",
        "CH1 is 0",
        "sample0",
    ] {
        assert!(text.contains(needle), "{needle} missing from {text}");
    }
}

/// B-96: a digital trigger needs no `level`; an analog one is refused
/// without it, naming what is missing.
#[tokio::test]
async fn a_digital_trigger_needs_no_level() {
    let router = router().await;
    let session = initialize(&router).await;
    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let devices = instruments["structuredContent"]["devices"]
        .as_array()
        .unwrap();
    let digital = find_digital_device(devices);
    let configure = call_tool(
        &router,
        &session,
        "configure_acquisition",
        json!({
            "device": digital, "rate_hz": 1_000_000, "channels": [0, 1], "samples": 10_000,
            "trigger": { "channel": 1, "slope": "rising" }
        }),
    )
    .await;
    assert_eq!(configure["isError"], false, "{configure:?}");
    let captured = call_tool(
        &router,
        &session,
        "capture",
        json!({ "device": digital, "timeout_s": 5.0 }),
    )
    .await;
    assert_eq!(captured["isError"], false, "{captured:?}");
    assert_eq!(
        captured["structuredContent"]["triggered"], true,
        "{captured:?}"
    );

    let analog = find_analog_device(devices);
    let refused = call_tool(
        &router,
        &session,
        "configure_acquisition",
        json!({
            "device": analog, "rate_hz": 1_000_000, "channels": [0], "samples": 10_000,
            "trigger": { "channel": 0, "slope": "rising" }
        }),
    )
    .await;
    assert_eq!(refused["isError"], true, "{refused:?}");
    assert!(refused.to_string().contains("needs `level`"), "{refused:?}");
}

/// Issue #32: a window given as the sample numbers `decode` reports, and
/// F-09: every picture's `url` serves the same picture over HTTP.
#[tokio::test]
async fn windows_by_sample_number_and_a_url_for_every_picture() {
    let router = router().await;
    let session = initialize(&router).await;
    let instruments = call_tool(&router, &session, "list_instruments", json!({})).await;
    let device = find_analog_device(
        instruments["structuredContent"]["devices"]
            .as_array()
            .unwrap(),
    );
    call_tool(
        &router,
        &session,
        "configure_acquisition",
        json!({ "device": device, "rate_hz": 1_000_000, "channels": [0], "samples": 10_000 }),
    )
    .await;
    let captured = call_tool(
        &router,
        &session,
        "capture",
        json!({ "device": device, "timeout_s": 5.0 }),
    )
    .await;
    let capture = captured["structuredContent"]["capture"].as_str().unwrap();

    // 100 samples at full resolution, the same as the window in seconds.
    let by_sample = call_tool(
        &router,
        &session,
        "read_capture",
        json!({ "capture": capture, "channel": 0, "sample0": 1000, "sample1": 1100 }),
    )
    .await;
    assert_eq!(by_sample["isError"], false, "{by_sample:?}");
    let by_time = call_tool(
        &router,
        &session,
        "read_capture",
        json!({ "capture": capture, "channel": 0, "t0_s": 0.001, "t1_s": 0.0011 }),
    )
    .await;
    let min = by_sample["structuredContent"]["min"].as_array().unwrap();
    assert_eq!(min.len(), 100);
    assert_eq!(
        by_sample["structuredContent"]["min"],
        by_time["structuredContent"]["min"]
    );
    for bad in [
        json!({ "capture": capture, "channel": 0, "sample0": 10, "t1_s": 0.001 }),
        json!({ "capture": capture, "channel": 0, "sample0": 10, "sample1": 10 }),
    ] {
        let refused = call_tool(&router, &session, "read_capture", bad).await;
        assert_eq!(refused["isError"], true, "{refused:?}");
    }

    let view = call_tool(
        &router,
        &session,
        "render_view",
        json!({ "capture": capture, "sample0": 1000, "sample1": 3000, "include_svg": true }),
    )
    .await;
    assert_eq!(view["isError"], false, "{view:?}");
    let url = view["structuredContent"]["url"].as_str().unwrap();
    // No Host header in this test's requests, so the path alone.
    assert!(url.starts_with("/api/render?capture=capture"), "{url}");
    assert!(url.contains("t0_s=") && url.contains("t1_s="), "{url}");
    let response = router
        .clone()
        .oneshot(Request::get(url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        view["structuredContent"]["svg"].as_str().unwrap(),
        "the url serves the same picture"
    );

    // A client that named the server's host gets an absolute link.
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "bench.local:8080")
        .body(Body::from(
            rpc(1, "initialize", json!({ "protocolVersion": "2025-06-18" })).to_string(),
        ))
        .unwrap();
    let (_, _, hosted) = call(&router, req).await;
    let png = call_tool(
        &router,
        &hosted.unwrap(),
        "render_view",
        json!({ "capture": capture, "format": "png" }),
    )
    .await;
    let url = png["structuredContent"]["url"].as_str().unwrap();
    assert!(
        url.starts_with("http://bench.local:8080/api/render?"),
        "{url}"
    );
    assert!(url.ends_with("format=png"), "{url}");
}
