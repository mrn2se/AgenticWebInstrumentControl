//! The `/ws` endpoint (SPECIFICATION.md §3.1): one socket per connection
//! carrying JSON text control frames and binary sample frames.
//!
//! Three tasks cooperate per connection, all funnelling through one
//! `mpsc::Sender<WsOut>` so control responses, events, and sample frames can
//! interleave freely on the single underlying socket without a shared lock:
//! - the **reader** loop below, which parses each incoming text frame and
//!   spawns its dispatch (so one slow request can't block another — this is
//!   what makes concurrent in-flight requests with distinct `id`s resolve
//!   independently, SPEC §3.1);
//! - the **event forwarder**, subscribed to the [`awic_core::EventBus`] for
//!   the lifetime of the connection;
//! - the **writer**, the only task that actually touches the socket's sink,
//!   so every message this connection sends — control or binary — goes
//!   through one place. It is also where a sample frame is considered
//!   "acked" for backpressure purposes (SPEC §3.2): once
//!   `WebSocket::send` for that frame's `Message::Binary` completes.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use awic_proto::Response;

use crate::backpressure::StreamBackpressure;
use crate::dispatch::{self, TermSubscriptions};
use crate::state::AppState;

/// Bound on how many outbound messages (control responses, events, or
/// sample frames) may be queued for one connection before the *sender*
/// backs off. Sample frames additionally go through their own per-stream
/// backpressure policy (`crate::backpressure`) before ever reaching this
/// queue, so this bound is mostly about control/event traffic; for sample
/// frames specifically, [`crate::stream::forward_stream`] uses `try_send`
/// and drops rather than waiting on this bound.
const OUT_QUEUE_CAPACITY: usize = 64;

/// One message waiting to be written to a connection's socket.
pub enum WsOut {
    /// A JSON control response or event (SPEC §3.1).
    Text(String),
    /// A binary sample frame (SPEC §3.2) plus the bookkeeping needed to
    /// release its bytes from the stream's backpressure tracker once
    /// actually sent.
    Frame {
        bytes: Vec<u8>,
        payload_len: usize,
        backpressure: Arc<StreamBackpressure>,
    },
    /// A binary view frame (SPEC §3.4, 0xA6). `in_flight` is cleared once the
    /// socket write actually completes — a follow view's conflation flag
    /// (`crate::recorder::views`), the same release-on-send-complete pattern
    /// as [`WsOut::Frame`]'s backpressure tracker, just for "at most one
    /// frame in flight" instead of a byte counter.
    ViewFrame {
        bytes: Vec<u8>,
        in_flight: Arc<AtomicBool>,
    },
    /// A binary `0xA7` terminal output frame (SPEC §3.5, §11.2): pre-encoded
    /// already, and with no stream-level backpressure bookkeeping to release
    /// on send — the drop-and-count policy for these lives in
    /// `terminal::session`, one layer upstream of this queue.
    TermFrame(Vec<u8>),
}

/// Every server that wants `term.open`'s peer address for SPEC §12's origin
/// logging must serve through `router.into_make_service_with_connect_info::
/// <SocketAddr>()` (`main.rs`, and this crate's own `tests/ws_protocol.rs`)
/// rather than plain `axum::serve(listener, router)` — `axum`'s `ConnectInfo`
/// extractor has no `Option<_>` form to fall back on when it isn't wired up.
pub async fn ws_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state, peer))
}

async fn handle_socket(socket: WebSocket, state: AppState, peer: SocketAddr) {
    let origin = format!("ws {peer}");
    let term_subs: TermSubscriptions = Arc::new(AsyncMutex::new(HashMap::new()));
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(OUT_QUEUE_CAPACITY);
    let mut shutdown_rx = state.subscribe_shutdown();
    // Views (SPEC §3.4) belong to this connection and must die with it; the
    // recorder tracks them under this id, cleaned up below once the reader
    // loop ends (a follow view's task holds no reference to the socket
    // itself, so nothing else would ever stop it).
    let conn_id = state.recorder().new_connection();

    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
                msg = out_rx.recv() => {
                    match msg {
                        Some(WsOut::Text(text)) => {
                            if sink.send(Message::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                        Some(WsOut::Frame { bytes, payload_len, backpressure }) => {
                            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                                break;
                            }
                            // "Acked": the write to the socket completed.
                            backpressure.release(payload_len);
                        }
                        Some(WsOut::ViewFrame { bytes, in_flight }) => {
                            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                                break;
                            }
                            in_flight.store(false, std::sync::atomic::Ordering::Release);
                        }
                        Some(WsOut::TermFrame(bytes)) => {
                            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    let mut event_rx = state.event_bus().subscribe();
    let event_out_tx = out_tx.clone();
    let events_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let Ok(text) = serde_json::to_string(&event) else {
                continue;
            };
            if event_out_tx.send(WsOut::Text(text)).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { break };
        match msg {
            Message::Text(text) => {
                let state = state.clone();
                let out_tx = out_tx.clone();
                let origin = origin.clone();
                let term_subs = term_subs.clone();
                tokio::spawn(async move {
                    let response = handle_text_message(
                        &state,
                        &out_tx,
                        conn_id,
                        text.as_str(),
                        &origin,
                        &term_subs,
                    )
                    .await;
                    if let Some(response) = response {
                        let _ = out_tx.send(WsOut::Text(response)).await;
                    }
                });
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    state.recorder().close_connection(conn_id);
    events_task.abort();

    // Per-connection terminal subscriptions are cleaned up here, not left
    // for the session to notice on its own: a serial session's last
    // subscriber leaving is what closes its port (SPEC §11.2.1), and a
    // client that simply disconnects (crashed tab, closed laptop) without
    // ever sending `term.close` must not leave that port held open forever.
    //
    // This must happen *before* `writer.await` below, not after: each open
    // session has its own forwarder task (`dispatch::spawn_terminal_forwarder`)
    // holding a clone of `out_tx` for as long as the session keeps producing
    // output, so `out_rx` — and therefore the writer's own `select!` loop —
    // never sees every sender dropped, and `writer.await` never returns,
    // until `state.terminal().close()` removes this connection's
    // subscription and lets that forwarder's channel close in turn. Awaiting
    // the writer first (as an earlier version of this function did) is a
    // deadlock: cleanup would never run because it was waiting on the very
    // thing cleanup itself unblocks.
    for (session_id, subscriber_id) in term_subs.lock().await.drain() {
        state.terminal().close(session_id, subscriber_id).await.ok();
    }

    drop(out_tx);
    let _ = writer.await;
}

/// Parses and dispatches one incoming text frame, returning the JSON to send
/// back — `None` only if serializing the response itself somehow fails
/// (never for a bad request, which still gets a `Response`).
async fn handle_text_message(
    state: &AppState,
    out_tx: &mpsc::Sender<WsOut>,
    conn_id: u64,
    text: &str,
    origin: &str,
    term_subs: &TermSubscriptions,
) -> Option<String> {
    let response = match parse_request(text) {
        Ok(request) => {
            dispatch::handle_request_with_origin(state, out_tx, conn_id, request, origin, term_subs)
                .await
        }
        Err(response) => *response,
    };
    serde_json::to_string(&response).ok()
}

/// Parses `text` into a [`awic_proto::Request`], or builds the best
/// [`Response`] it can if that fails: a request with an unrecognized (or
/// malformed) `op` must not kill the connection (SPECIFICATION.md §3.1) —
/// it gets `ok: false` with a stable error code instead.
fn parse_request(text: &str) -> Result<awic_proto::Request, Box<Response>> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| {
        Box::new(Response::err(
            0,
            "invalid_request",
            format!("malformed JSON: {err}"),
        ))
    })?;
    let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    serde_json::from_value(value).map_err(|err| {
        let (code, prefix) = classify_parse_error(&err.to_string());
        Box::new(Response::err(id, code, format!("{prefix}: {err}")))
    })
}

/// Tells "there is no such op" apart from "that op exists, these fields are
/// wrong". Both arrive as one serde error on an internally tagged enum, and
/// both used to answer `unknown_op` — which told an agent that had merely
/// mistyped a field that the operation itself does not exist, so the sensible
/// thing to do was to stop using it. Serde names an unrecognized tag
/// `unknown variant ...` and lists the ops it knows, which is exactly the
/// message an agent needs; anything else is a field-level complaint about an
/// op we do recognize.
fn classify_parse_error(msg: &str) -> (&'static str, &'static str) {
    if msg.starts_with("unknown variant") || msg.starts_with("missing field `op`") {
        ("unknown_op", "unrecognized request")
    } else {
        ("invalid_params", "bad arguments")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_rejects_malformed_json_without_id() {
        let response = parse_request("{not json").unwrap_err();
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_request");
    }

    #[test]
    fn parse_request_rejects_unknown_op_but_keeps_the_id() {
        let response = parse_request(r#"{"v":1,"id":42,"op":"totally.unknown"}"#).unwrap_err();
        assert!(!response.ok);
        assert_eq!(response.id, 42);
        assert_eq!(response.error.unwrap().code, "unknown_op");
    }

    #[test]
    fn a_known_op_with_bad_fields_is_not_an_unknown_op() {
        // The op exists; `cfg` is missing. Answering `unknown_op` here sent
        // an agent looking for another way to configure the device.
        let response = parse_request(r#"{"v":1,"id":7,"op":"acq.configure","device":"3-3.4.1.1"}"#)
            .unwrap_err();
        assert_eq!(response.id, 7);
        let error = response.error.unwrap();
        assert_eq!(error.code, "invalid_params");
        assert!(error.msg.contains("cfg"), "{}", error.msg);
    }

    #[test]
    fn a_request_without_an_op_is_an_unknown_op() {
        let response = parse_request(r#"{"v":1,"id":8}"#).unwrap_err();
        assert_eq!(response.error.unwrap().code, "unknown_op");
    }

    #[test]
    fn an_unknown_op_still_lists_the_ops_that_do_exist() {
        let response = parse_request(r#"{"v":1,"id":42,"op":"totally.unknown"}"#).unwrap_err();
        let msg = response.error.unwrap().msg;
        assert!(msg.contains("device.list"), "{msg}");
    }

    #[test]
    fn parse_request_accepts_a_known_op() {
        let request = parse_request(r#"{"v":1,"id":1,"op":"device.list"}"#).unwrap();
        assert_eq!(request.id(), 1);
    }

    #[test]
    fn protocol_version_constant_is_stable() {
        assert_eq!(awic_proto::PROTOCOL_VERSION, 1);
    }
}
