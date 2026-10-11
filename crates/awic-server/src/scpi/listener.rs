//! TCP front end for the SCPI engine (SPECIFICATION.md §9): line-oriented
//! (`\n` or `\r\n`), one [`awic_scpi::Engine`] + [`super::ServerHost`] per
//! connection, many connections concurrently. Shuts down alongside the rest
//! of the server (`AppState::subscribe_shutdown`), the same signal
//! `ws.rs`/`main.rs` use to close every `/ws` socket promptly.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use awic_scpi::Engine;

use super::ServerHost;
use crate::state::AppState;

/// Binds `bind_addr` (`server.bind`'s host with `server.scpi_port`, SPEC
/// §9) and serves SCPI connections until the server's shutdown signal fires.
pub async fn serve(state: AppState, bind_addr: String) -> std::io::Result<()> {
    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!(bind = %bind_addr, "awic-scpi listening");
    serve_on(state, listener).await
}

/// Like [`serve`], but on an already-bound listener — the split point tests
/// use to bind an ephemeral port (`127.0.0.1:0`) and learn the real address
/// via `TcpListener::local_addr` before connecting.
pub async fn serve_on(state: AppState, listener: TcpListener) -> std::io::Result<()> {
    let mut shutdown = state.subscribe_shutdown();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => {
                tracing::info!("scpi listener shutting down");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!(%err, "scpi accept failed");
                        continue;
                    }
                };
                let state = state.clone();
                let conn_shutdown = state.subscribe_shutdown();
                tokio::spawn(handle_connection(socket, peer, state, conn_shutdown));
            }
        }
    }
}

/// One SCPI TCP client's whole lifetime: read a line, run it through the
/// engine, write back whatever it produced (SPEC §9 gives no explicit
/// framing for the response beyond "line-oriented", so one `\n`-terminated
/// reply per line of input that produced output — nothing for a pure
/// setter, exactly like [`awic_scpi::Engine::execute_line`]'s own contract).
async fn handle_connection(
    socket: TcpStream,
    peer: SocketAddr,
    state: AppState,
    mut shutdown: broadcast::Receiver<()>,
) {
    let origin = format!("scpi {peer}");
    tracing::info!(%peer, "scpi connection opened");
    let host = Arc::new(ServerHost::new(state, origin));
    let mut engine = Engine::new(host);

    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => break, // client closed its write side
                    Ok(_) => {
                        let text = line.trim_end_matches(['\r', '\n']);
                        if text.is_empty() {
                            continue;
                        }
                        if let Some(response) = engine.execute_line(text).await {
                            let mut out = response.into_bytes();
                            out.push(b'\n');
                            if writer.write_all(&out).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%peer, %err, "scpi read error");
                        break;
                    }
                }
            }
        }
    }
    tracing::info!(%peer, "scpi connection closed");
}
