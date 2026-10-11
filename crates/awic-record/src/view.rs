//! View data: the representation choice of SPECIFICATION.md §3.4, produced
//! by [`crate::recording::Recording::view`] so it encodes straight into an
//! `awic-proto` view frame (kind byte, header fields and payload layout all
//! match the wire format 1:1 — the server just adds `view_id`/`sequence`).

/// Matches the `kind` byte of the view frame header (SPEC §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    /// `item_count` x (`value: u8`, `length: LEB128`) — exact edges.
    DigitalRuns = 1,
    /// `item_count` x (`AND: u8`, `OR: u8`).
    DigitalSummary = 2,
    /// Raw codes, channels interleaved, `sample_bits` wide, little-endian.
    AnalogRaw = 3,
    /// Channel-major; per channel `item_count` x (`min`, `max`), `sample_bits` wide.
    AnalogSummary = 4,
}

impl ViewKind {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A view answer, shaped to drop straight into an `awic-proto` view frame
/// payload; `first_sample`/`samples_per_item`/`item_count` map directly onto
/// the frame header fields of the same name.
#[derive(Debug, Clone)]
pub struct ViewData {
    pub kind: ViewKind,
    pub first_sample: u64,
    /// 1 for [`ViewKind::DigitalRuns`] and [`ViewKind::AnalogRaw`]; a power
    /// of two otherwise, chosen so item boundaries are absolute and stable
    /// while a window scrolls (SPEC §3.4).
    pub samples_per_item: u64,
    pub item_count: u64,
    pub channel_count: u8,
    /// 8 or 16 for analog; 8 for digital.
    pub sample_bits: u8,
    pub payload: Vec<u8>,
}

/// Smallest power-of-two item span, and the first_sample aligned down to a
/// multiple of it, such that the number of items covering
/// `[aligned_first_sample, last_covered)` is at most `columns`.
///
/// `last_covered` is `end` clipped to whatever data actually exists
/// (`head_sample`); the returned item_count reflects that, not the original
/// `end`, so the final item may be partial (SPEC §3.4).
pub fn choose_item_span(start: u64, last_covered: u64, columns: u32) -> (u64, u64, u64) {
    let columns = columns.max(1) as u64;
    let mut span = 1u64;
    loop {
        let aligned_first = (start / span) * span;
        let covered = last_covered.saturating_sub(aligned_first);
        let items = covered.div_ceil(span).max(1);
        if items <= columns || span >= (1u64 << 62) {
            return (aligned_first, span, items);
        }
        span *= 2;
    }
}
