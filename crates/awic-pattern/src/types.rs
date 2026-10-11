//! Small input/output types (DEVELOPMENT.md §2: this crate defines its
//! own, since it must build without depending on `awic-record`).

/// One channel a pattern may name, with enough calibration to convert
/// between the pattern's units (Hz, seconds, volts) and samples/codes.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelInfo {
    /// Exactly as it appears in a pattern: `"D0"`..`"D7"`, `"CH1"`, `"CH2"`.
    pub name: String,
    pub kind: ChannelKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelKind {
    /// Bit `bit` of the packed digital byte (SPEC §2.7, §3.2 encoding 2).
    Digital { bit: u8 },
    /// `index` into the interleaved analog stream (0-based). `volts = code *
    /// scale + offset`, matching `awic-core::ChannelCalibration`.
    Analog { index: u8, scale: f32, offset: f32 },
}

/// The recording's fixed parameters, needed to convert a pattern's `f=`,
/// durations and voltages into samples and raw codes at compile time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordingInfo {
    pub rate_hz: f64,
    /// Analog sample width in bits: 8 (`u8` codes) or 16 (`i16` codes).
    /// Ignored for purely digital patterns.
    pub sample_bits: u8,
    /// Number of interleaved analog channels in the stream fed to
    /// [`crate::Matcher::feed_analog`]. Ignored for purely digital patterns.
    pub channel_count: u8,
}

/// One decoded, contiguous run of the packed digital byte: `value` held for
/// `len` samples starting at `start`. Matches `awic-record`'s `Run`
/// verbatim (SPEC §2.8.1) so the storage glue (T18) can pass its runs
/// straight through with no reshaping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub start: u64,
    pub len: u64,
    pub value: u8,
}

/// One occurrence of a pattern (SPEC §2.9.2 rule 4, §2.9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Sample at which the matched event starts (run start + k/f for a bit
    /// pattern; the pulse/level onset for a pulse; the edge sample for an
    /// edge/state/clocked/analog pattern).
    pub sample: u64,
    /// Sample at which the match became certain. Equal to `sample` for
    /// edge-like patterns; later than `sample` when a pulse needed its
    /// closing edge, or when a bit pattern's tail was decided once the
    /// still-open run grew far enough (SPEC §2.9.3, task note on early
    /// decisions).
    pub decided_at: u64,
}
