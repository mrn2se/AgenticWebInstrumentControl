//! Regression test for a real deadlock caught during manual hardware
//! verification of the terminal module (SPEC §11.2/§11.2.1): a client that
//! opens a terminal session and then simply disconnects (closes its tab,
//! loses its network) must not leave the underlying source connection open
//! forever. It did — `dispatch::spawn_terminal_forwarder` holds its own
//! clone of the connection's outbound `mpsc::Sender<WsOut>`, so that
//! channel never saw every sender dropped, `ws.rs`'s writer task never
//! returned, and the cleanup code that would have closed the session (by
//! then unreachable, sitting after an `await` on that same writer) never
//! ran either — confirmed on the bench as `/dev/ttyACM0` staying held open
//! by the server process after the browser tab's WebSocket had already
//! gone to `CLOSE-WAIT`. Fixed by running that cleanup *before* awaiting
//! the writer (see `ws.rs`); this test exercises the same sequence without
//! needing real hardware, over a real WebSocket.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awic_core::Config;
use awic_proto::{TerminalKind, TerminalSourceInfo};
use awic_server::state::AppState;
use awic_server::terminal::{
    Connected, OpenSettings, SourceConnection, SourceEvent, TerminalError, TerminalSourceProvider,
};
use awic_server::usb::UnimplementedUsbController;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

/// A fake source whose `Connected` handle counts itself alive/dropped —
/// standing in for a real `SerialConnection`'s open file descriptor.
struct CountingConnection {
    alive: Arc<AtomicUsize>,
}

impl Drop for CountingConnection {
    fn drop(&mut self) {
        self.alive.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl SourceConnection for CountingConnection {
    async fn recv(&mut self) -> SourceEvent {
        // A quiet UART with nothing connected: never produces output on its
        // own. The test only cares whether this connection gets dropped.
        std::future::pending().await
    }

    async fn write(&mut self, _data: &[u8]) -> Result<(), TerminalError> {
        Ok(())
    }
}

struct CountingProvider {
    alive: Arc<AtomicUsize>,
}

#[async_trait]
impl TerminalSourceProvider for CountingProvider {
    fn kind(&self) -> TerminalKind {
        TerminalKind::Serial
    }

    fn list(&self) -> Vec<TerminalSourceInfo> {
        vec![TerminalSourceInfo {
            source: "serial:test:1.0".to_string(),
            kind: TerminalKind::Serial,
            label: "test".to_string(),
            device: Some("test".to_string()),
            available: true,
            unavailable_reason: None,
        }]
    }

    fn persistent(&self) -> bool {
        false
    }

    async fn connect(
        &self,
        _source_id: &str,
        _settings: OpenSettings,
    ) -> Result<Connected, TerminalError> {
        self.alive.fetch_add(1, Ordering::SeqCst);
        Ok(Connected {
            connection: Box::new(CountingConnection {
                alive: self.alive.clone(),
            }),
            initial_backlog: Vec::new(),
            label: "test".to_string(),
        })
    }
}

async fn spawn_server(alive: Arc<AtomicUsize>) -> (String, tokio::task::JoinHandle<()>) {
    let config = Config::default();
    let provider: Arc<dyn TerminalSourceProvider> = Arc::new(CountingProvider { alive });
    let state = AppState::with_terminal_providers(
        config,
        Arc::new(awic_server::backend::PlaceholderBackend),
        Arc::new(UnimplementedUsbController),
        vec![provider],
    );

    let router = awic_server::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
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

#[tokio::test]
async fn a_client_that_disconnects_without_term_close_still_releases_the_session() {
    let alive = Arc::new(AtomicUsize::new(0));
    let (url, _server) = spawn_server(alive.clone()).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    ws.send(ClientMessage::Text(
        json!({ "v": 1, "id": 1, "op": "term.open", "source": "serial:test:1.0",
                "serial": { "baud": 115200, "data_bits": 8, "parity": "none", "stop_bits": "one" } })
        .to_string()
        .into(),
    ))
    .await
    .expect("send term.open");

    // Read until the term.open response (an event could arrive first).
    loop {
        let msg = ws.next().await.expect("stream open").expect("ws message");
        if let ClientMessage::Text(text) = msg {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value.get("id") == Some(&json!(1)) {
                assert_eq!(value["ok"], true, "{value:?}");
                break;
            }
        }
    }
    assert_eq!(
        alive.load(Ordering::SeqCst),
        1,
        "the source must be connected while the session is open"
    );

    // The client vanishes without ever sending `term.close` — a crashed
    // tab, a lost network link.
    drop(ws);

    tokio::time::timeout(Duration::from_secs(5), async {
        while alive.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect(
        "the session's connection must be dropped once the socket closes, not held open forever",
    );
}

// ---- The same promise over MCP (live test B-21, 2026-09-19) ----------------
//
// An agent's serial sessions used to outlive it: no tool closed one, and
// ending the MCP session (`DELETE /mcp`, or LRU eviction) did not release
// them either — the bench server still held `ttyACM0`/`ttyACM1` afterwards.

mod mcp {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    fn router(alive: Arc<AtomicUsize>) -> axum::Router {
        let provider: Arc<dyn TerminalSourceProvider> = Arc::new(CountingProvider { alive });
        awic_server::build_router(AppState::with_terminal_providers(
            Config::default(),
            Arc::new(awic_server::backend::PlaceholderBackend),
            Arc::new(UnimplementedUsbController),
            vec![provider],
        ))
    }

    async fn send(
        router: &axum::Router,
        method: &str,
        body: Option<Value>,
        session: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let mut req = Request::builder()
            .method(method)
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(session) = session {
            req = req.header("Mcp-Session-Id", session);
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let response = router.clone().oneshot(req).await.expect("router call");
        let status = response.status();
        let minted = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, minted)
    }

    async fn initialize(router: &axum::Router) -> String {
        let (status, _, session) = send(
            router,
            "POST",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                         "params": { "protocolVersion": "2025-06-18",
                                     "clientInfo": { "name": "test" } } })),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        session.expect("a session id")
    }

    async fn tool(router: &axum::Router, session: &str, name: &str, args: Value) -> Value {
        let (status, body, _) = send(
            router,
            "POST",
            Some(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                         "params": { "name": name, "arguments": args } })),
            Some(session),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["result"]["isError"], false, "{body}");
        body["result"]["structuredContent"].clone()
    }

    async fn open_one(router: &axum::Router, alive: &AtomicUsize) -> (String, u64) {
        let session = initialize(router).await;
        let opened = tool(
            router,
            &session,
            "serial_open",
            json!({ "source": "serial:test:1.0", "baud": 115200 }),
        )
        .await;
        assert_eq!(alive.load(Ordering::SeqCst), 1, "the port is open");
        (
            session,
            opened["session"].as_u64().expect("a terminal session id"),
        )
    }

    async fn released(alive: &AtomicUsize) -> bool {
        for _ in 0..40 {
            if alive.load(Ordering::SeqCst) == 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn serial_close_releases_the_port() {
        let alive = Arc::new(AtomicUsize::new(0));
        let router = router(alive.clone());
        let (session, terminal) = open_one(&router, &alive).await;

        let closed = tool(
            &router,
            &session,
            "serial_close",
            json!({ "session": terminal }),
        )
        .await;
        assert_eq!(closed["closed"], true);
        assert!(released(&alive).await, "serial_close must release the port");

        // Closing twice is not an error.
        tool(
            &router,
            &session,
            "serial_close",
            json!({ "session": terminal }),
        )
        .await;
    }

    #[tokio::test]
    async fn ending_the_mcp_session_releases_its_serial_ports() {
        let alive = Arc::new(AtomicUsize::new(0));
        let router = router(alive.clone());
        let (session, _) = open_one(&router, &alive).await;

        let (status, _, _) = send(&router, "DELETE", None, Some(&session)).await;
        assert!(status.is_success(), "DELETE /mcp answered {status}");
        assert!(
            released(&alive).await,
            "the port must be released when the MCP session that opened it ends"
        );
    }
}
