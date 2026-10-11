//! MCP monitor wire types (SPECIFICATION.md §8.5): what the web UI's MCP tab
//! reads over `/ws` — the MCP endpoint's connection details, its sessions,
//! and a bounded log of the JSON-RPC traffic on `/mcp`, including the
//! pictures `render_view` handed to agents.
//!
//! Carried by the `mcp.*` ops and the `mcp.activity` event in
//! [`crate::control`]. The event carries no entry, like `usb.changed`:
//! a client that cares fetches `mcp.log` from the last `seq` it has, so
//! nothing is pushed to connections that are not looking at the tab.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What happened, per log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum McpEntryKind {
    /// `initialize`: a session began.
    SessionStart,
    /// `DELETE /mcp`, or the session was evicted as least recently used.
    SessionEnd,
    /// A JSON-RPC request with an `id` (`tools/call`, `tools/list`, `ping`…).
    Request,
    /// A JSON-RPC notification (no `id`, no answer).
    Notification,
    /// A request the server refused before it reached a session (bad JSON,
    /// no or unknown `Mcp-Session-Id`).
    Rejected,
}

/// One picture an MCP result carried, without its bytes: those are fetched
/// with `mcp.image` so a log listing stays small.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct McpImageInfo {
    pub mime_type: String,
    /// Size of the image as sent to the agent (base64), in bytes.
    pub bytes: u32,
    /// Whether the bytes were kept (`mcp.image` can fetch them). `false`
    /// when image logging was off, or the picture alone broke the budget.
    pub kept: bool,
}

/// One logged MCP exchange.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct McpLogEntry {
    /// Monotonic per server; `mcp.log after=<seq>` resumes from here.
    pub seq: u32,
    /// Wall-clock time the request arrived, ms since the Unix epoch.
    pub t_ms: f64,
    /// Short session label (first 8 characters of its `Mcp-Session-Id`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session: Option<String>,
    /// The client's name from `initialize`, e.g. `"Visual Studio Code"`.
    pub client: String,
    pub kind: McpEntryKind,
    /// JSON-RPC method, e.g. `"tools/call"`.
    pub method: String,
    /// For `tools/call`: the tool's name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<f64>,
    /// A JSON-RPC error or a tool result with `isError: true`.
    pub is_error: bool,
    /// One line for the list: the arguments, or the error.
    pub summary: String,
    /// The request as received, pretty-printed; truncated past the
    /// server's per-message bound.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub request: Option<String>,
    /// The answer, pretty-printed, with image data replaced by a
    /// placeholder (the picture itself is in `images`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response: Option<String>,
    #[serde(default)]
    pub images: Vec<McpImageInfo>,
}

/// One MCP session as the monitor saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct McpSessionInfo {
    /// Short label (first 8 characters of the `Mcp-Session-Id`).
    pub id: String,
    pub client: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_version: Option<String>,
    pub protocol_version: String,
    pub started_ms: f64,
    pub last_seen_ms: f64,
    /// Requests and notifications after `initialize`.
    pub requests: u32,
    pub tool_calls: u32,
    pub errors: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_tool: Option<String>,
    /// `false` once deleted or evicted; ended sessions are kept for a while
    /// so a person can see what just went away.
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ended: Option<String>,
}

/// The message log's switch and its memory use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct McpLoggingState {
    /// Record requests and responses. Sessions and counters are tracked
    /// regardless; only the message log costs memory.
    pub enabled: bool,
    /// Also keep the pictures results carried (the bulk of the memory).
    pub images: bool,
    pub memory_used_bytes: u64,
    pub memory_limit_bytes: u64,
    pub entries: u32,
    /// Entries dropped (oldest first) to stay inside the limit, since start
    /// or the last clear.
    pub dropped: u32,
}

/// `mcp.status`: how to reach the MCP endpoint and what is connected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct McpStatusResult {
    /// Path of the streamable-HTTP endpoint on this server, `"/mcp"`.
    pub endpoint_path: String,
    /// The address the server is bound to, from `[server].bind`.
    pub bind: String,
    /// Whether a bearer token is configured (the token itself is never sent).
    pub auth_required: bool,
    pub server_name: String,
    pub server_version: String,
    pub protocol_version: String,
    pub tool_count: u32,
    pub max_sessions: u32,
    /// Most recent first.
    pub sessions: Vec<McpSessionInfo>,
    pub logging: McpLoggingState,
}

/// `mcp.log`: entries after the requested `seq`, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct McpLogResult {
    pub entries: Vec<McpLogEntry>,
    /// The highest `seq` assigned so far (0 when none); pass it as `after`
    /// next time.
    pub last_seq: u32,
    pub logging: McpLoggingState,
}

/// `mcp.image`: one picture from one entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct McpImageResult {
    pub mime_type: String,
    pub data_b64: String,
}
