//! Errors for the recorder engine (SPECIFICATION.md §2.8).
//!
//! Kept specific per DEVELOPMENT.md §1 ("errors are specific") — a caller
//! (the server, in T18) turns these into `rec.*` error responses, so the
//! message must say exactly what was wrong and, where useful, what the
//! caller can do about it.

/// Cap on `extract_packed`/`extract_analog`: 64 Mi samples (SPEC §2.8.5).
pub const MAX_EXTRACT_SAMPLES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// `append_packed`/`append_analog` called with a byte count that isn't a
    /// whole number of samples (analog: not a multiple of the sample width).
    #[error("misaligned append: {len} bytes is not a whole number of samples ({bytes_per_sample} bytes each)")]
    MisalignedAppend { len: usize, bytes_per_sample: usize },

    /// `append_packed` called on an analog recording, or vice versa.
    #[error("wrong recording kind: this recording is {actual}, not {expected}")]
    WrongKind {
        expected: &'static str,
        actual: &'static str,
    },

    /// A query range starts before the retained span (`first_sample`) — the
    /// data has been evicted, not merely absent.
    #[error(
        "range start {start} is before the retained span; recording holds [{first_sample}, {head_sample})"
    )]
    Evicted {
        start: u64,
        first_sample: u64,
        head_sample: u64,
    },

    /// A query range is empty or backwards.
    #[error("empty or inverted range [{start}, {end})")]
    EmptyRange { start: u64, end: u64 },

    /// `extract_packed`/`extract_analog` asked for more than
    /// [`MAX_EXTRACT_SAMPLES`].
    #[error("extract of {samples} samples exceeds the {MAX_EXTRACT_SAMPLES} sample cap")]
    ExtractTooLarge { samples: u64 },
}

pub type Result<T> = std::result::Result<T, RecordError>;
