//! Pragmatic SCPI parser and command engine (SPECIFICATION.md §9).
//!
//! This crate is pure: no sockets, no files, no tokio API of its own (only
//! `async-trait` to describe [`ScpiHost`], the boundary a transport
//! implements). Two front ends use it — `awic-server`'s TCP listener on
//! `server.scpi_port` and the `scpi` terminal source (SPEC §11.2) — both
//! calling exactly the same [`Engine`], which is the whole point: the
//! console in the browser is the same parser a Python script talks to.
//!
//! ```text
//! line of text --parser::parse_message--> Vec<Command>
//!              --Engine::execute_one----> host calls, response text
//! ```
//!
//! Deliberately **not** a strict IEEE-488.2/SCPI-99 implementation — see the
//! doc comments on [`value`] (number formatting/parsing) and [`engine`]
//! (error reporting, `*RST` scope) for the specific, documented deviations.
//! The goal (task brief) is an interface a script or an AI assistant can
//! drive without reading a manual, not conformance testing.

mod context;
mod engine;
mod error;
mod host;
mod parser;
mod value;

pub use engine::Engine;
pub use error::{HostError, ScpiError};
pub use host::{AcqSettings, EdgeTrigger, Measurement, PortLock, RecSummary, ScpiHost, Slope};
pub use parser::{parse_message, Command, Segment};
pub use value::{format_scpi_number, format_seconds, parse_number, Param};
