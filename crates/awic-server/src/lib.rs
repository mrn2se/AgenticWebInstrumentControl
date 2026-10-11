//! axum HTTP/WebSocket server (SPECIFICATION.md §3).
//!
//! This crate wires `awic-core`'s `Registry`, `EventBus`, and
//! `Backend`/`Instrument` traits onto an `axum` application: static frontend
//! serving with SPA fallback, `GET /api/health`, and `GET /ws` — the wire
//! protocol's one control+binary socket (§3.1, §3.2).
//!
//! `awic-fake` and `awic-usb` are built by parallel tasks and are still
//! empty stub crates; this crate depends only on `awic-core`'s traits (never
//! on either concrete crate) plus a small [`usb::UsbController`] trait
//! defined here for the USB-dependent operations, so wiring the real
//! backends in later touches only `main.rs`.

pub mod app;
pub mod auth;
pub mod backend;
pub mod backpressure;
pub mod cli;
pub mod decode;
pub mod dispatch;
pub mod error;
pub mod http;
pub mod mcp;
pub mod origin;
pub mod recorder;
pub mod render;
pub mod scpi;
pub mod soft_trigger;
pub mod state;
pub mod stream;
pub mod survey;
pub mod terminal;
pub mod usb;
pub mod usb_real;
pub mod ws;

pub use app::build_router;
pub use state::AppState;
