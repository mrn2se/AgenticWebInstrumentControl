//! The `POST /mcp` (and `DELETE /mcp`) HTTP endpoint (SPECIFICATION.md
//! §8.1: "Streamable HTTP ... on the same axum server"). Sits behind the
//! same `require_auth` layer every other route does (`app.rs`), so the
//! optional bearer token gates it exactly like the web UI.
//!
//! One [`McpHost`] (and therefore one recorder connection id, one serial
//! buffer set) backs one MCP session, minted at `initialize` and identified
//! afterwards by the `Mcp-Session-Id` response/request header, per the MCP
//! streamable-HTTP transport. This server always answers with a single JSON
//! response body (never upgrades to an SSE stream): every tool here is a
//! synchronous request/response, so there is nothing this server would ever
//! push to a client outside of answering its own request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use awic_proto::McpEntryKind;

use super::host::McpHost;
use super::monitor;
use super::protocol::{
    error, negotiate_protocol_version, success, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
};
use super::tools;
use crate::state::AppState;

pub const SESSION_HEADER: &str = "Mcp-Session-Id";

/// Live MCP sessions, keyed by the `Mcp-Session-Id` minted at `initialize`.
/// Lives on [`AppState`] (see `AppState::mcp_sessions`) rather than as a
/// module-level global so that independent `AppState`s — every test spawns
/// its own — never share sessions.
pub type McpSessions = Arc<AsyncMutex<SessionTable>>;

pub fn new_sessions() -> McpSessions {
    Arc::new(AsyncMutex::new(SessionTable::default()))
}

/// At most this many sessions are kept. Each holds a recorder connection and
/// serial buffers, and `initialize` needs no prior state, so without a bound
/// any client that can reach the port could grow the server's memory at will
/// (clients rarely send `DELETE`). Past the bound the least recently used
/// session goes; its client gets "unknown session" and re-initializes, as the
/// MCP transport expects.
pub const MAX_SESSIONS: usize = 32;

/// Sessions with a use counter for least-recently-used eviction.
#[derive(Default)]
pub struct SessionTable {
    hosts: HashMap<String, (Arc<McpHost>, u64)>,
    clock: u64,
}

impl SessionTable {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Adds a session, evicting the least recently used one if full;
    /// returns the ids evicted.
    pub fn insert(&mut self, id: String, host: Arc<McpHost>) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.hosts.len() >= MAX_SESSIONS {
            let oldest = self
                .hosts
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(oldest) => {
                    tracing::info!(session = %oldest, "evicting least recently used MCP session");
                    self.hosts.remove(&oldest);
                    evicted.push(oldest);
                }
                None => break,
            }
        }
        let now = self.tick();
        self.hosts.insert(id, (host, now));
        evicted
    }

    /// Looks a session up and marks it used.
    pub fn get(&mut self, id: &str) -> Option<Arc<McpHost>> {
        let now = self.tick();
        let (host, used) = self.hosts.get_mut(id)?;
        *used = now;
        Some(host.clone())
    }

    pub fn remove(&mut self, id: &str) {
        self.hosts.remove(id);
    }

    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

fn json_response(status: StatusCode, body: Value) -> Response {
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The peer-address/client-name label logged with every mutation (SPEC
/// §8.4/§12): `"mcp <clientInfo.name>"` when the client named itself at
/// `initialize` (every real MCP client does), else `"mcp unknown"`.
fn client_origin(params: &Value) -> String {
    let name = params
        .get("clientInfo")
        .and_then(|c| c.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("unknown");
    format!("mcp {name}")
}

/// What [`respond`] decided, before it becomes an HTTP response: kept as
/// data so [`handle_post`] can hand the exchange to the MCP monitor (SPEC
/// §8.5) in one place, whichever branch produced it.
struct Reply {
    status: StatusCode,
    /// `None` for an accepted notification (202, no body).
    body: Option<Value>,
    /// The full session id the request belonged to, if any.
    session: Option<String>,
    /// Set on `initialize`: the session id to return in the header.
    minted: Option<String>,
    kind: McpEntryKind,
}

impl Reply {
    fn rejected(status: StatusCode, body: Value) -> Self {
        Self {
            status,
            body: Some(body),
            session: None,
            minted: None,
            kind: McpEntryKind::Rejected,
        }
    }
}

pub async fn handle_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started_ms = monitor::now_ms();
    let clock = Instant::now();
    let (request, reply) = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => {
            let reply = respond(&state, &headers, &value).await;
            (value, reply)
        }
        Err(err) => (
            Value::String(String::from_utf8_lossy(&body).into_owned()),
            Reply::rejected(
                StatusCode::BAD_REQUEST,
                error(Value::Null, PARSE_ERROR, format!("invalid JSON: {err}")),
            ),
        ),
    };

    state.mcp_monitor().record(monitor::Exchange {
        session: reply.session.as_deref(),
        kind: reply.kind,
        method: request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("(none)"),
        request: &request,
        response: reply.body.as_ref(),
        started_ms,
        duration_ms: clock.elapsed().as_secs_f64() * 1000.0,
    });

    let mut response = match reply.body {
        Some(body) => json_response(reply.status, body),
        None => reply.status.into_response(),
    };
    if let Some(value) = reply
        .minted
        .as_deref()
        .and_then(|sid| HeaderValue::from_str(sid).ok())
    {
        response.headers_mut().insert(SESSION_HEADER, value);
    }
    response
}

async fn respond(state: &AppState, headers: &HeaderMap, value: &Value) -> Reply {
    let id = value.get("id").cloned();
    let is_notification = id.is_none();
    let Some(method) = value
        .get("method")
        .and_then(|m| m.as_str())
        .map(str::to_string)
    else {
        return Reply::rejected(
            StatusCode::BAD_REQUEST,
            error(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "request has no \"method\"",
            ),
        );
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    if method == "initialize" {
        let requested = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(super::protocol::LATEST_PROTOCOL_VERSION);
        let version = negotiate_protocol_version(requested);
        let session_id = Uuid::new_v4().to_string();
        let host = McpHost::new(state.clone(), client_origin(&params));
        // F-09: a rendered view's `url` points back at the server the client
        // reached, as it reached it (`awic mcp-stdio` sends its own Host).
        if let Some(authority) = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .filter(|h| !h.is_empty() && !h.contains(['/', ' ', '?', '#']))
        {
            host.set_base_url(format!("http://{authority}"));
        }
        state
            .mcp_monitor()
            .session_started(&session_id, &params, version);
        let evicted = state
            .mcp_sessions()
            .lock()
            .await
            .insert(session_id.clone(), host);
        for gone in evicted {
            state
                .mcp_monitor()
                .session_ended(&gone, "evicted (least recently used)");
        }

        let result = json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "awic", "version": env!("CARGO_PKG_VERSION") },
            "instructions": super::protocol::SERVER_INSTRUCTIONS,
        });
        return Reply {
            status: StatusCode::OK,
            body: Some(success(id.unwrap_or(Value::Null), result)),
            session: Some(session_id.clone()),
            minted: Some(session_id),
            kind: McpEntryKind::Request,
        };
    }

    let session_id = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let host = match &session_id {
        Some(sid) => state.mcp_sessions().lock().await.get(sid),
        None => None,
    };
    let Some(host) = host else {
        // A session this server does not know and a missing header are
        // different answers (MCP 2025-06-18, Streamable HTTP session
        // management): an unknown session **MUST** be 404, and a client that
        // gets one starts a new session by re-initializing, while a 400 is a
        // malformed request it has no recovery for. Both were 400 here, so
        // after a restart of awic every tool call from an already-connected
        // client failed until someone reconnected it by hand (B-69).
        let (status, text) = match &session_id {
            Some(sid) => (
                StatusCode::NOT_FOUND,
                format!(
                    "unknown Mcp-Session-Id {sid:?}: this server has no such session (it was \
                     restarted, or the session expired). Call \"initialize\" again to start a new \
                     one."
                ),
            ),
            None => (
                StatusCode::BAD_REQUEST,
                "missing Mcp-Session-Id; call \"initialize\" first".to_string(),
            ),
        };
        return Reply::rejected(
            status,
            error(id.unwrap_or(Value::Null), INVALID_REQUEST, &text),
        );
    };

    let kind = if is_notification {
        McpEntryKind::Notification
    } else {
        McpEntryKind::Request
    };
    let reply = |status: StatusCode, body: Option<Value>| Reply {
        status,
        body,
        session: session_id.clone(),
        minted: None,
        kind,
    };

    match method.as_str() {
        "notifications/initialized"
        | "notifications/cancelled"
        | "notifications/roots/list_changed" => reply(StatusCode::ACCEPTED, None),
        "ping" => reply(
            StatusCode::OK,
            Some(success(id.unwrap_or(Value::Null), json!({}))),
        ),
        "tools/list" => {
            let tools: Vec<Value> = tools::tool_defs()
                .into_iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();
            reply(
                StatusCode::OK,
                Some(success(
                    id.unwrap_or(Value::Null),
                    json!({ "tools": tools }),
                )),
            )
        }
        "tools/call" => {
            let Some(id) = id else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    Some(error(
                        Value::Null,
                        INVALID_REQUEST,
                        "tools/call must carry an id",
                    )),
                );
            };
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            let result = tools::call_tool(&host, &name, arguments).await;
            reply(StatusCode::OK, Some(success(id, result)))
        }
        other => {
            if is_notification {
                // Unknown notifications are ignored, not errors (JSON-RPC/MCP convention).
                reply(StatusCode::ACCEPTED, None)
            } else {
                reply(
                    StatusCode::OK,
                    Some(error(
                        id.unwrap_or(Value::Null),
                        METHOD_NOT_FOUND,
                        format!("unknown method {other:?}"),
                    )),
                )
            }
        }
    }
}

/// Ends a session early (the MCP client is done with it). Sessions left
/// undeleted are simply forgotten when the process exits; there is no
/// server-side idle timeout today (see `mcp/mod.rs`'s doc comment).
pub async fn handle_delete(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(session_id) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
        state.mcp_sessions().lock().await.remove(session_id);
        state
            .mcp_monitor()
            .session_ended(session_id, "deleted by client");
    }
    StatusCode::NO_CONTENT.into_response()
}
