//! `awic-record` — the recorder's storage and query engine
//! (SPECIFICATION.md §2.8).
//!
//! A pure data structure crate: no I/O, no tokio. It holds a continuous
//! acquisition (§2.3.2) in a bounded, compressed, in-memory history —
//! digital as run-length blocks (§2.8.1), analog as codec blocks (§2.8.2) —
//! kept alongside a multi-resolution summary tree (§2.8.3), and answers
//! every query the server (T18) and `awic-pattern` (T14) need: runs, masked
//! runs (for pattern search), extraction into a capture snapshot (§2.8.5),
//! exact summaries, and the view representation of §3.4. The server owns
//! the tasks and the memory budget; this crate provides
//! [`Recording::evict_oldest_block`] as the primitive.

pub mod analog;
pub mod digital;
pub mod error;
pub mod recording;
pub mod summary;
mod varint;
pub mod view;

pub use digital::Run;
pub use error::{RecordError, Result, MAX_EXTRACT_SAMPLES};
pub use recording::{Recording, RecordingKind, RecordingSpec, Summary};
pub use summary::{AnalogBucket, AnalogChannelStat, DigitalBucket};
pub use view::{ViewData, ViewKind};
