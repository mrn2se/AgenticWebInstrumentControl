//! MCP (Model Context Protocol) server (SPECIFICATION.md §8,
//! M7): the agent-facing transport, alongside `/ws` (§3) and SCPI (§9).
//!
//! # Transport and SDK choice
//!
//! §8.1 asks for "Streamable HTTP at `POST /mcp` on the same axum server"
//! plus a stdio shim. The official Rust SDK, [`rmcp`] (crates.io, v3.3.0 at
//! the time of writing), was evaluated first, per the task brief:
//!
//! - It has **no C dependency** (`cargo tree` shows only `libc`/`getrandom`,
//!   both pure-Rust-wrapper crates already in this workspace's tree via
//!   `nusb`/`tokio`) and does not hard-depend on axum at all — its
//!   `transport-streamable-http-server` feature is a `tower::Service`, so
//!   there is no axum-version conflict with this workspace's axum 0.8 to
//!   worry about.
//! - It was **not adopted**, for a narrower reason: it models a tool server
//!   around its own `ServerHandler` trait, `#[tool]`/`#[tool_router]` macros
//!   with `schemars`-derived input schemas, and its own session/event-store
//!   abstraction for resumable SSE streams. Adopting it would mean
//!   restating every tool as an `rmcp`-macro handler and wiring its session
//!   store *underneath* this server's existing per-request
//!   `dispatch::handle_request_with_origin` pipeline — the single place
//!   port protection, soft limits and the SPEC §12 mutation log happen
//!   (`crate::scpi`'s own doc comment explains why every transport goes
//!   through it). None of AWIC's ~19 tools needs resumable streams or
//!   server-initiated messages (every tool is one request, one response),
//!   so the abstraction `rmcp` brings pays for a capability this transport
//!   never uses, at the cost of a second, SDK-shaped way to reach
//!   `AppState` alongside `scpi::ServerHost`'s existing one.
//!
//! Given that, this module implements the small JSON-RPC surface MCP
//! actually needs by hand, against protocol revision `2025-06-18`
//! ([`protocol::LATEST_PROTOCOL_VERSION`]): `initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call`, `ping`, and
//! session teardown via `DELETE /mcp`. [`route::handle_post`]/
//! [`route::handle_delete`] are the axum handlers `app.rs` wires up;
//! [`host::McpHost`] is the `scpi::ServerHost`-shaped adapter that turns a
//! tool call into an [`awic_proto::Request`] and calls
//! `dispatch::handle_request_with_origin` with origin `"mcp <client>"`
//! (SPEC §8.4) — so every MCP mutation is logged and inherits port
//! protection/soft limits exactly as the SCPI transport does, with no
//! separate code path that could skip them. [`tools`] holds the tool
//! catalog (name, agent-facing description, JSON Schema) and the
//! `tools/call` dispatcher; [`monitor`] records sessions and the message
//! log the web UI's MCP tab shows (SPEC §8.5); [`protocol`] holds the
//! JSON-RPC/MCP wire shapes; [`serial_buffer`] backs the `serial_read`/`serial_expect` tools;
//! [`stdio`] is the `awic mcp-stdio` subcommand.
//!
//! One [`host::McpHost`] backs one MCP session end to end — including its
//! own recorder connection id and serial-session buffers, torn down when
//! the session is deleted or the process exits. There is currently no idle
//! session reaper (a client that never sends `DELETE /mcp` and stops
//! talking leaks one small `McpHost`) — acceptable for a LAN bench tool
//! with a handful of concurrent agents, called out here rather than left
//! for someone to discover.

pub mod host;
pub mod monitor;
pub mod protocol;
pub mod route;
pub mod serial_buffer;
pub mod stdio;
pub mod tools;
