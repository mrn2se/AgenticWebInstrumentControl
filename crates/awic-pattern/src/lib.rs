//! `awic-pattern` — the agent-facing event language (SPECIFICATION.md
//! §2.9): parses a one-line pattern (§2.9.1) and finds it in a digital run
//! stream or an analog sample stream, both over history (`search`) and
//! incrementally as a recording grows (`wait`).
//!
//! A pure crate (DEVELOPMENT.md §2): no I/O, no tokio, no dependency on
//! `awic-record`. It defines its own small input types ([`Run`],
//! [`ChannelInfo`], [`RecordingInfo`]) so it builds and tests independently
//! of the storage crate; the server glue (T18) passes `awic-record`'s runs
//! and summary buckets straight through — [`Run`] matches `awic-record`'s
//! `Run` field-for-field.
//!
//! ```
//! use awic_pattern::{parse, ChannelInfo, ChannelKind, Matcher, RecordingInfo, Run};
//!
//! let channels = vec![ChannelInfo { name: "D0".into(), kind: ChannelKind::Digital { bit: 0 } }];
//! let rec = RecordingInfo { rate_hz: 1_000_000.0, sample_bits: 8, channel_count: 0 };
//! let pattern = parse("D0 rising", &channels, rec).unwrap();
//!
//! let mut matcher = Matcher::new(&pattern);
//! let runs = [Run { start: 0, len: 100, value: 0 }, Run { start: 100, len: 50, value: 1 }];
//! let matches = matcher.feed_runs(&runs, 150);
//! assert_eq!(matches[0].sample, 100);
//! ```

pub mod analog;
pub mod digital;
mod error;
mod matcher;
mod parse;
mod pattern;
mod types;

pub use error::{ParseError, Span};
pub use matcher::{search_runs, Matcher, SearchResult};
pub use parse::parse;
pub use pattern::{
    AnalogPattern, BitSeqPattern, BitVal, ClockedPattern, Cmp, EdgeDir, EdgePattern, FireOn,
    Pattern, PulsePattern, StatePattern,
};
pub use types::{ChannelInfo, ChannelKind, Match, RecordingInfo, Run};
