//! Digital matchers: one per pattern shape (SPEC §2.9.2), all consuming
//! [`Run`]s incrementally. Shared plumbing (run dedup/merge across chunk
//! boundaries) lives in [`support`].

mod support;

mod bitseq;
mod clocked;
mod edge;
mod pulse;
mod state;

pub use bitseq::BitSeqMatcher;
pub use clocked::ClockedMatcher;
pub use edge::EdgeMatcher;
pub use pulse::PulseMatcher;
pub use state::StateMatcher;
