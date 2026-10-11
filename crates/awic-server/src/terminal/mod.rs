//! Terminal session manager (SPECIFICATION.md §11.2/§11.2.1, §3.5): sessions
//! are shared per source — opening an already-open serial port joins it —
//! with 64 KiB scrollback replayed on join, per-connection output queues
//! that drop-and-count under backpressure, and the last connection leaving a
//! serial session closing the underlying port.
//!
//! [`TerminalSourceProvider`] is the plug point later tasks' `scpi` and
//! `decode` sources use without touching [`SessionManager`] itself — the
//! same "abstraction so the next driver doesn't touch the core" shape as
//! `awic-core::Backend`/`Instrument` (DEVELOPMENT.md §1 rule 1), applied
//! here to terminal sources instead of instruments.

mod session;

pub mod decode_source;
pub mod log_source;
pub mod serial_source;

pub use session::{
    Connected, OpenSettings, SessionManager, SourceConnection, SourceEvent, TerminalError,
    TerminalSourceProvider,
};

/// Scrollback kept per session, replayed in full to a connection joining it
/// (SPEC §11.2.1). Oldest bytes drop off the front once exceeded.
pub const SCROLLBACK_CAPACITY: usize = 64 * 1024;

/// Bound on one connection's own pending terminal output before the
/// drop-and-count policy (SPEC §3.5) kicks in for *that* connection — a slow
/// tab falls behind without slowing down every other subscriber of the same
/// session.
pub const SUBSCRIBER_QUEUE_CAPACITY: usize = 512;
