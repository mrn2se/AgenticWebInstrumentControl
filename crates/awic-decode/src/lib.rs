//! Streaming UART, I²C, SPI, CAN and 1-Wire protocol decoders
//! (SPECIFICATION.md §6).
//!
//! Pure, `std`-only (DEVELOPMENT.md §2): a decoder consumes the same
//! transition list the capture store derives (§2.7) and a recording's runs
//! are made of (§2.8.1) — one streaming decoder therefore serves a finished
//! capture, a live recording, and (via [`terminal`]) the decode terminal
//! source (§11.2). This crate defines its own [`Transition`] rather than
//! depending on `awic-core`: same shape as `awic_core::capture_store::Transition`
//! (`sample_index` + a packed-channel byte), so the server converts trivially,
//! and a recording's run boundaries convert the same way (a run start is a
//! transition).
//!
//! Positions are sample indices, never nanoseconds (D-10): exact, and the
//! caller already knows the rate.

mod annotation;
mod level_tracker;
#[cfg(test)]
pub(crate) mod test_util;

pub mod baud;
pub mod can;
pub mod i2c;
pub mod onewire;
pub mod spi;
pub mod survey;
pub mod terminal;
pub mod transactions;
pub mod uart;

pub use annotation::{Annotation, AnnotationKind, LEVEL_BIT, LEVEL_FIELD, LEVEL_FRAME};
pub(crate) use level_tracker::LevelTracker;

/// One entry of the transition list a decoder is fed: the packed-channel
/// byte changed to `value` at `sample_index`. Same shape as
/// `awic_core::capture_store::Transition` (there: `channel_mask`) so the
/// server's conversion from a capture's or recording's transitions/runs is a
/// field rename, not a re-derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub sample_index: u64,
    pub value: u8,
}

/// A single, logical role a decoder needs a channel bound to (SPEC §6):
/// UART needs one data line, I²C needs SCL+SDA, SPI needs CLK plus any of
/// MOSI/MISO/CS, CAN and 1-Wire one line each. The UI presents these names when asking the user to map
/// acquired channels onto a decoder instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelRole {
    /// UART's single data line (works for either RX or TX; which physical
    /// signal it is doesn't change how it's decoded).
    Rx,
    Scl,
    Sda,
    Clk,
    Mosi,
    Miso,
    Cs,
    /// CAN's single bus line: RX pin or CANL (dominant low), or CANH or
    /// CANH−CANL (dominant high) — see `can::Polarity`.
    Can,
    /// 1-Wire's one open-drain line (`owr`, as sigrok names it).
    Owr,
}

/// Describes one construction-time option a decoder accepts, for a UI to
/// render before the decoder is built (options are plain constructor
/// fields — see each decoder's `*Config` — this is metadata only, never a
/// live, mutable value: SPEC §6 fixes the rate and channel mapping "at
/// construction").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderOption {
    pub id: &'static str,
    pub description: &'static str,
}

/// A decoder over one protocol, streaming transitions of the packed-channel
/// byte in and annotations out (SPEC §6).
pub trait Decoder: Send {
    fn name(&self) -> &str;
    /// Channel roles this decoder instance needs bound, in no particular
    /// order beyond what each decoder's constructor documents.
    fn required_channels(&self) -> &[ChannelRole];
    /// Construction-time options this decoder type accepts (metadata for a
    /// UI; already fixed by the time a `Decoder` exists).
    fn options(&self) -> &[DecoderOption];
    /// Feed transitions in strictly increasing `sample_index` order, plus
    /// the sample index through which the input is known (a gap up to
    /// `valid_until` with no transition means the level held). Annotations
    /// are returned as soon as they are complete — a decoder may hold state
    /// across calls for a frame or transaction still in progress.
    fn feed(&mut self, transitions: &[Transition], valid_until: u64) -> Vec<Annotation>;
    /// The packed line levels at the first sample of the input, given before
    /// the first `feed`. Without it a decoder assumes its bus idle there, so
    /// input that starts mid-transaction — a decode window opened at an
    /// arbitrary time — reads as an edge out of idle at its first sample: for
    /// I²C a phantom START and misaligned bytes after it. Seeded with the real
    /// levels, a decoder instead waits for the next genuine frame boundary.
    /// A protocol without such a boundary event may ignore it (UART: a low
    /// first sample is most likely a start bit).
    fn seed(&mut self, levels: u8);
    /// [`seed`](Self::seed), for input whose first sample is `sample`
    /// rather than 0 — a decode window opened in a recording, or a live tail.
    /// A decoder that times bus idle from the start of its input (CAN) needs
    /// to know where that is; the others ignore it.
    fn seed_at(&mut self, sample: u64, levels: u8) {
        let _ = sample;
        self.seed(levels);
    }
    /// End of input: flush whatever frame or transaction is still open,
    /// marked incomplete (`AnnotationKind::Incomplete`), and reset to idle.
    fn finish(&mut self, end_sample: u64) -> Vec<Annotation>;
}
