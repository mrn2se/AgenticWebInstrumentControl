//! `awic-proto` — the AWIC wire protocol.
//!
//! This crate is the single source of truth for everything that crosses the
//! `/ws` boundary: the JSON control types (SPECIFICATION.md §3.1), the
//! binary sample frame header (§3.2), the binary view frame header (§3.4),
//! and the binary terminal frame header (§3.5). It has no dependency on any
//! other `awic-*` crate, so it can be depended on by both `awic-core`
//! (server side) and, via generated TypeScript, the frontend.
//!
//! Every type here also derives [`ts_rs::TS`]; running `cargo test
//! export_bindings` regenerates `web/src/proto/types.ts` from these
//! definitions (§3.3) — see `.cargo/config.toml` at the workspace root for
//! how the output path is pinned regardless of the test's working directory.

mod control;
mod decode;
pub mod frame;
mod mcp_monitor;
mod recorder;
mod render;
mod survey;
pub mod term_frame;
mod terminal;
pub mod view_frame;

pub use control::*;
pub use decode::*;
pub use mcp_monitor::*;
pub use recorder::*;
pub use render::*;
pub use survey::*;
pub use terminal::*;
