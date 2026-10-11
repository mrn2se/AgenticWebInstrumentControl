//! WebSocket integration test for the `scpi` terminal source (SPECIFICATION.md
//! §11.2, §9): opens `term.open{source:"scpi"}` over a real `/ws` connection,
//! writes `*IDN?\n`, and reads the `0xA7` terminal frame back — proving the
//! browser console and the TCP listener (`tests/scpi_tcp.rs`) really are the
//! same `awic_scpi::Engine`, driven this time through
//! `awic_server::terminal::SessionManager` instead of a raw socket.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use awic_core::Config;
use awic_proto::term_frame::{decode_header, HEADER_LEN};
use awic_server::scpi::terminal_source::ScpiTerminalSource;
use awic_server::state::AppState;
use awic_server::terminal::TerminalSourceProvider;
use awic_server::usb::UnimplementedUsbController;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

async fn spawn_server() -> String {
    // `ScpiTerminalSource` needs the very `AppState` it's about to be
    // registered into (its `connect()` dispatches through it) — the cell
    // `main.rs` also uses to break that construction-order cycle (see
    // `LateAppState`'s doc comment).
    let late_state = Arc::new(OnceLock::new());
    let provider: Arc<dyn TerminalSourceProvider> =
        Arc::new(ScpiTerminalSource::new(late_state.clone()));
    let state = AppState::with_terminal_providers(
        Config::default(),
        Arc::new(awic_server::backend::PlaceholderBackend),
        Arc::new(UnimplementedUsbController),
        vec![provider],
    );
    late_state.set(state.clone()).ok().unwrap();

    let router = awic_server::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("server error");
    });
    format!("ws://{addr}/ws")
}

/// Reads WebSocket messages until a `0xA7` terminal frame for `session`
/// arrives whose decoded text contains `needle`, or times out.
async fn expect_term_output(
    ws: &mut (impl futures::Stream<Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin),
    session: u16,
    needle: &str,
) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = ws.next().await.expect("stream open").expect("ws message");
            if let ClientMessage::Binary(bytes) = msg {
                let header = decode_header(&bytes).expect("valid term frame");
                if header.session_id != session {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes[HEADER_LEN..]).to_string();
                if text.contains(needle) {
                    return text;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?} on session {session}"))
}

#[tokio::test]
async fn scpi_console_answers_idn_over_the_terminal_source() {
    let url = spawn_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    ws.send(ClientMessage::Text(
        json!({ "v": 1, "id": 1, "op": "term.open", "source": "scpi" })
            .to_string()
            .into(),
    ))
    .await
    .expect("send term.open");

    let session: u16 = loop {
        let msg = ws.next().await.expect("stream open").expect("ws message");
        if let ClientMessage::Text(text) = msg {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value.get("id") == Some(&json!(1)) {
                assert_eq!(value["ok"], true, "{value:?}");
                break value["result"]["session"].as_u64().unwrap() as u16;
            }
        }
    };

    // The prompt is replayed as scrollback as soon as the session opens.
    expect_term_output(&mut ws, session, "SCPI> ").await;

    ws.send(ClientMessage::Text(
        json!({ "v": 1, "id": 2, "op": "term.write", "session": session, "data": "*IDN?\n" })
            .to_string()
            .into(),
    ))
    .await
    .expect("send term.write");

    let text = expect_term_output(&mut ws, session, "AWIC,awic,0,").await;
    assert!(text.contains("AWIC,awic,0,"), "{text}");
}
