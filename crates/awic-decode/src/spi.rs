//! Streaming SPI decoder (SPEC §6).
//!
//! Like I²C, SPI carries its own clock, so decoding is edge-driven rather
//! than rate-based: CPOL/CPHA pick which clock edge is the sampling edge,
//! and MOSI/MISO are simply read at that instant.

use crate::annotation::{LEVEL_FIELD, LEVEL_FRAME};
use crate::{
    Annotation, AnnotationKind, ChannelRole, Decoder, DecoderOption, LevelTracker, Transition,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiMode {
    Mode0,
    Mode1,
    Mode2,
    Mode3,
}

impl SpiMode {
    fn cpol(self) -> bool {
        matches!(self, SpiMode::Mode2 | SpiMode::Mode3)
    }
    fn cpha(self) -> bool {
        matches!(self, SpiMode::Mode1 | SpiMode::Mode3)
    }
    /// Whether the bit is latched on the clock's rising edge. Derived from
    /// CPOL/CPHA: the sampling edge is the *leading* edge when CPHA=0, the
    /// *trailing* edge when CPHA=1, and which physical direction the
    /// leading edge is depends on CPOL (idle level) — working through the
    /// four combinations collapses to "rising iff CPOL == CPHA".
    fn sample_on_rising(self) -> bool {
        self.cpol() == self.cpha()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOrder {
    /// First bit clocked is the most-significant bit (the common SPI
    /// convention, and this decoder's default).
    Msb,
    Lsb,
}

/// SPI construction parameters (SPEC §6). Any of `mosi`/`miso`/`cs` may be
/// absent; `clk` is always required.
#[derive(Debug, Clone, Copy)]
pub struct SpiConfig {
    pub clk: u8,
    pub mosi: Option<u8>,
    pub miso: Option<u8>,
    pub cs: Option<u8>,
    /// `false` (default) = CS active low.
    pub cs_active_high: bool,
    pub mode: SpiMode,
    pub bit_order: BitOrder,
    /// Bits per word, 1..=32 (default 8).
    pub word_size: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpiConfigError {
    #[error("no data line configured: at least one of MOSI/MISO must be set")]
    NoDataLine,
    #[error("word size must be 1..=32, got {0}")]
    WordSize(u8),
    #[error("bit {bit} is used by more than one role")]
    BitCollision { bit: u8 },
}

const OPTIONS: &[DecoderOption] = &[
    DecoderOption {
        id: "mode",
        description: "CPOL/CPHA mode 0-3",
    },
    DecoderOption {
        id: "bit_order",
        description: "msb (default) | lsb",
    },
    DecoderOption {
        id: "word_size",
        description: "bits per word, default 8",
    },
    DecoderOption {
        id: "cs_active_high",
        description: "chip-select polarity; default active low",
    },
];

fn bits_to_word(bits: &[bool], order: BitOrder) -> u32 {
    match order {
        BitOrder::Msb => bits.iter().fold(0u32, |acc, &b| (acc << 1) | u32::from(b)),
        BitOrder::Lsb => {
            let mut v = 0u32;
            for (i, &b) in bits.iter().enumerate() {
                if b {
                    v |= 1 << i;
                }
            }
            v
        }
    }
}

#[derive(Debug, Clone, Default)]
struct WordState {
    mosi_bits: Vec<bool>,
    miso_bits: Vec<bool>,
    start: u64,
    count: u8,
}

/// Streaming SPI decoder over CLK (+ optional MOSI/MISO/CS) (SPEC §6).
#[derive(Debug)]
pub struct SpiDecoder {
    clk_bit: u8,
    mosi_bit: Option<u8>,
    miso_bit: Option<u8>,
    cs_bit: Option<u8>,
    cs_active_high: bool,
    sample_on_rising: bool,
    bit_order: BitOrder,
    word_size: u8,
    tracker: LevelTracker,
    /// Whether the slave is currently selected. Always `true` when no CS is
    /// configured (SPEC §6: "without CS, words are framed by bit count from
    /// the first clock edge").
    cs_active: bool,
    channels: Vec<ChannelRole>,
    word: Option<WordState>,
}

impl SpiDecoder {
    pub fn new(cfg: SpiConfig) -> Result<Self, SpiConfigError> {
        if cfg.mosi.is_none() && cfg.miso.is_none() {
            return Err(SpiConfigError::NoDataLine);
        }
        if !(1..=32).contains(&cfg.word_size) {
            return Err(SpiConfigError::WordSize(cfg.word_size));
        }
        let mut seen = vec![cfg.clk];
        for bit in [cfg.mosi, cfg.miso, cfg.cs].into_iter().flatten() {
            if seen.contains(&bit) {
                return Err(SpiConfigError::BitCollision { bit });
            }
            seen.push(bit);
        }

        let mut channels = vec![ChannelRole::Clk];
        if cfg.mosi.is_some() {
            channels.push(ChannelRole::Mosi);
        }
        if cfg.miso.is_some() {
            channels.push(ChannelRole::Miso);
        }
        if cfg.cs.is_some() {
            channels.push(ChannelRole::Cs);
        }

        // Idle CLK level is CPOL; idle CS is deasserted.
        let idle_clk = if cfg.mode.cpol() { 1u8 << cfg.clk } else { 0 };
        let idle_cs = match cfg.cs {
            Some(cs) if !cfg.cs_active_high => 1u8 << cs, // deasserted = high
            _ => 0,
        };

        Ok(SpiDecoder {
            clk_bit: cfg.clk,
            mosi_bit: cfg.mosi,
            miso_bit: cfg.miso,
            cs_bit: cfg.cs,
            cs_active_high: cfg.cs_active_high,
            sample_on_rising: cfg.mode.sample_on_rising(),
            bit_order: cfg.bit_order,
            word_size: cfg.word_size,
            tracker: LevelTracker::new(idle_clk | idle_cs),
            cs_active: cfg.cs.is_none(),
            channels,
            word: None,
        })
    }

    fn cs_is_active(&self, byte: u8) -> bool {
        let Some(cs) = self.cs_bit else { return true };
        let level = (byte >> cs) & 1 == 1;
        level == self.cs_active_high
    }

    fn flush_word(&mut self, sample: u64, out: &mut Vec<Annotation>, incomplete: bool) {
        let Some(w) = self.word.take() else { return };
        if w.count == 0 {
            return;
        }
        let mosi = self
            .mosi_bit
            .map(|_| bits_to_word(&w.mosi_bits, self.bit_order));
        let miso = self
            .miso_bit
            .map(|_| bits_to_word(&w.miso_bits, self.bit_order));
        let kind = if incomplete {
            AnnotationKind::Incomplete
        } else {
            AnnotationKind::Word { mosi, miso }
        };
        out.push(Annotation::new(w.start, sample, LEVEL_FIELD, kind));
    }

    fn on_sample_edge(&mut self, sample: u64, byte: u8, out: &mut Vec<Annotation>) {
        if !self.cs_active {
            return;
        }
        let word = self.word.get_or_insert_with(|| WordState {
            start: sample,
            ..Default::default()
        });
        if let Some(bit) = self.mosi_bit {
            word.mosi_bits.push((byte >> bit) & 1 == 1);
        }
        if let Some(bit) = self.miso_bit {
            word.miso_bits.push((byte >> bit) & 1 == 1);
        }
        word.count += 1;
        if word.count == self.word_size {
            self.flush_word(sample, out, false);
        }
    }
}

impl Decoder for SpiDecoder {
    fn name(&self) -> &str {
        "SPI"
    }

    fn required_channels(&self) -> &[ChannelRole] {
        &self.channels
    }

    fn options(&self) -> &[DecoderOption] {
        OPTIONS
    }

    fn feed(&mut self, transitions: &[Transition], valid_until: u64) -> Vec<Annotation> {
        self.tracker.push(transitions, valid_until);
        let mut mask = 1u8 << self.clk_bit;
        if let Some(cs) = self.cs_bit {
            mask |= 1 << cs;
        }
        let mut out = Vec::new();
        while let Some((sample, before, after)) = self.tracker.next_relevant(mask) {
            let clk_changed = (before >> self.clk_bit) & 1 != (after >> self.clk_bit) & 1;
            if clk_changed {
                let rising = (after >> self.clk_bit) & 1 == 1;
                if rising == self.sample_on_rising {
                    self.on_sample_edge(sample, after, &mut out);
                }
                continue;
            }
            // Otherwise CS changed (it's the only other bit in `mask`).
            let now_active = self.cs_is_active(after);
            if now_active && !self.cs_active {
                self.cs_active = true;
                self.word = None;
                out.push(Annotation::new(
                    sample,
                    sample,
                    LEVEL_FRAME,
                    AnnotationKind::ChipSelect { active: true },
                ));
            } else if !now_active && self.cs_active {
                self.flush_word(sample, &mut out, false);
                self.cs_active = false;
                out.push(Annotation::new(
                    sample,
                    sample,
                    LEVEL_FRAME,
                    AnnotationKind::ChipSelect { active: false },
                ));
            }
        }
        out
    }

    fn seed(&mut self, levels: u8) {
        self.tracker.seed(levels);
    }

    fn finish(&mut self, end_sample: u64) -> Vec<Annotation> {
        let mut out = Vec::new();
        self.flush_word(end_sample, &mut out, true);
        if self.cs_bit.is_some() && self.cs_active {
            out.push(Annotation::new(
                end_sample,
                end_sample,
                LEVEL_FRAME,
                AnnotationKind::Incomplete,
            ));
            self.cs_active = false;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::render_spi;
    use crate::test_util::{assert_all_split_points, assert_same_result, build_plan};
    use proptest::proptest;

    fn word_bits(value: u32, word_size: u8, order: BitOrder) -> Vec<bool> {
        match order {
            BitOrder::Msb => (0..word_size)
                .rev()
                .map(|i| (value >> i) & 1 == 1)
                .collect(),
            BitOrder::Lsb => (0..word_size).map(|i| (value >> i) & 1 == 1).collect(),
        }
    }

    /// Drives CLK/MOSI/MISO/CS the way a real SPI master would for a given
    /// CPOL/CPHA mode, so the decoder is exercised against realistic edge
    /// timing rather than hand-picked transitions.
    struct Waveform {
        clk_bit: u8,
        mosi_bit: Option<u8>,
        miso_bit: Option<u8>,
        cs_bit: Option<u8>,
        cpol: bool,
        cpha: bool,
        cs_active_high: bool,
        half: u64,
        cursor: u64,
        clk: bool,
        mosi: bool,
        miso: bool,
        cs: bool,
        last_value: u8,
        transitions: Vec<Transition>,
    }

    impl Waveform {
        fn new(
            clk_bit: u8,
            mosi_bit: Option<u8>,
            miso_bit: Option<u8>,
            cs_bit: Option<u8>,
            mode: SpiMode,
            cs_active_high: bool,
            half: u64,
        ) -> Self {
            let cpol = mode.cpol();
            let cs = cs_bit.is_some() && !cs_active_high;
            let mut w = Waveform {
                clk_bit,
                mosi_bit,
                miso_bit,
                cs_bit,
                cpol,
                cpha: mode.cpha(),
                cs_active_high,
                half,
                cursor: 0,
                clk: cpol,
                mosi: false,
                miso: false,
                cs,
                last_value: 0,
                transitions: Vec::new(),
            };
            w.last_value = w.value();
            w
        }

        fn value(&self) -> u8 {
            let mut v = 0u8;
            if self.clk {
                v |= 1 << self.clk_bit;
            }
            if let Some(b) = self.mosi_bit {
                if self.mosi {
                    v |= 1 << b;
                }
            }
            if let Some(b) = self.miso_bit {
                if self.miso {
                    v |= 1 << b;
                }
            }
            if let Some(b) = self.cs_bit {
                if self.cs {
                    v |= 1 << b;
                }
            }
            v
        }

        fn commit(&mut self) {
            let v = self.value();
            if v != self.last_value {
                self.transitions.push(Transition {
                    sample_index: self.cursor,
                    value: v,
                });
                self.last_value = v;
            }
        }

        fn advance(&mut self) {
            self.cursor += self.half;
        }

        fn idle(&mut self, half_periods: u64) {
            for _ in 0..half_periods {
                self.advance();
            }
        }

        fn select(&mut self) {
            self.cs = self.cs_active_high;
            self.commit();
            self.advance();
        }

        fn deselect(&mut self) {
            self.cs = !self.cs_active_high;
            self.commit();
            self.advance();
        }

        /// Clocks one word. CPHA=1 changes data exactly on the leading edge
        /// (the same transition, since that's genuinely simultaneous in
        /// real SPI); CPHA=0 needs data valid a half-period *before* the
        /// first leading edge, then changes it on each trailing edge ready
        /// for the next bit.
        fn clock_word(&mut self, mosi_bits: &[bool], miso_bits: &[bool]) {
            let n = mosi_bits.len().max(miso_bits.len());
            if self.cpha {
                for i in 0..n {
                    self.mosi = mosi_bits.get(i).copied().unwrap_or(false);
                    self.miso = miso_bits.get(i).copied().unwrap_or(false);
                    self.clk = !self.cpol;
                    self.commit();
                    self.advance();
                    self.clk = self.cpol;
                    self.commit();
                    self.advance();
                }
            } else {
                if n > 0 {
                    self.mosi = mosi_bits.first().copied().unwrap_or(false);
                    self.miso = miso_bits.first().copied().unwrap_or(false);
                    self.commit();
                    self.advance();
                }
                for i in 0..n {
                    self.clk = !self.cpol;
                    self.commit();
                    self.advance();
                    self.clk = self.cpol;
                    if i + 1 < n {
                        self.mosi = mosi_bits.get(i + 1).copied().unwrap_or(false);
                        self.miso = miso_bits.get(i + 1).copied().unwrap_or(false);
                    }
                    self.commit();
                    self.advance();
                }
            }
        }

        fn finish(self) -> (Vec<Transition>, u64) {
            (self.transitions, self.cursor)
        }
    }

    #[allow(clippy::too_many_arguments)] // test helper mirroring SpiConfig's fields 1:1
    fn cfg(
        clk: u8,
        mosi: Option<u8>,
        miso: Option<u8>,
        cs: Option<u8>,
        cs_active_high: bool,
        mode: SpiMode,
        bit_order: BitOrder,
        word_size: u8,
    ) -> SpiConfig {
        SpiConfig {
            clk,
            mosi,
            miso,
            cs,
            cs_active_high,
            mode,
            bit_order,
            word_size,
        }
    }

    fn decode_all(c: SpiConfig, transitions: &[Transition], end: u64) -> Vec<Annotation> {
        let mut d = SpiDecoder::new(c).unwrap();
        let mut out = d.feed(transitions, end);
        out.extend(d.finish(end));
        out
    }

    #[test]
    fn mode0_with_cs_two_words_one_line() {
        let c = cfg(
            0,
            Some(1),
            Some(2),
            Some(3),
            false,
            SpiMode::Mode0,
            BitOrder::Msb,
            8,
        );
        let mut w = Waveform::new(0, Some(1), Some(2), Some(3), SpiMode::Mode0, false, 10);
        w.idle(4);
        w.select();
        w.clock_word(
            &word_bits(0xA5, 8, BitOrder::Msb),
            &word_bits(0x3C, 8, BitOrder::Msb),
        );
        w.clock_word(
            &word_bits(0x01, 8, BitOrder::Msb),
            &word_bits(0xFE, 8, BitOrder::Msb),
        );
        w.deselect();
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(c, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::ChipSelect { active: true },
                AnnotationKind::Word {
                    mosi: Some(0xA5),
                    miso: Some(0x3C)
                },
                AnnotationKind::Word {
                    mosi: Some(0x01),
                    miso: Some(0xFE)
                },
                AnnotationKind::ChipSelect { active: false },
            ]
        );
        assert_eq!(
            render_spi(&anns),
            vec!["MOSI: A5 01  MISO: 3C FE".to_string()]
        );
    }

    #[test]
    fn all_four_modes_decode_correctly_at_1mhz() {
        // 1 MHz SPI clock at an 8 MHz sample rate: 4-sample half-period.
        for mode in [
            SpiMode::Mode0,
            SpiMode::Mode1,
            SpiMode::Mode2,
            SpiMode::Mode3,
        ] {
            let c = cfg(0, Some(1), None, None, false, mode, BitOrder::Msb, 8);
            let mut w = Waveform::new(0, Some(1), None, None, mode, false, 4);
            w.idle(4);
            w.clock_word(&word_bits(0x5A, 8, BitOrder::Msb), &[]);
            w.idle(4);
            let (transitions, end) = w.finish();
            let anns = decode_all(c, &transitions, end);
            let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
            assert_eq!(
                kinds,
                vec![AnnotationKind::Word {
                    mosi: Some(0x5A),
                    miso: None
                }],
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn no_cs_frames_purely_by_word_count() {
        let c = cfg(
            0,
            Some(1),
            None,
            None,
            false,
            SpiMode::Mode0,
            BitOrder::Msb,
            8,
        );
        let mut w = Waveform::new(0, Some(1), None, None, SpiMode::Mode0, false, 10);
        w.idle(4);
        w.clock_word(&word_bits(0x11, 8, BitOrder::Msb), &[]);
        w.clock_word(&word_bits(0x22, 8, BitOrder::Msb), &[]);
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(c, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Word {
                    mosi: Some(0x11),
                    miso: None
                },
                AnnotationKind::Word {
                    mosi: Some(0x22),
                    miso: None
                },
            ]
        );
        assert_eq!(
            render_spi(&anns),
            vec!["MOSI: 11".to_string(), "MOSI: 22".to_string()]
        );
    }

    #[test]
    fn lsb_first_bit_order() {
        let c = cfg(
            0,
            Some(1),
            None,
            None,
            false,
            SpiMode::Mode0,
            BitOrder::Lsb,
            8,
        );
        let mut w = Waveform::new(0, Some(1), None, None, SpiMode::Mode0, false, 10);
        w.idle(4);
        w.clock_word(&word_bits(0b1100_0001, 8, BitOrder::Lsb), &[]);
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(c, &transitions, end);
        assert_eq!(
            anns[0].kind,
            AnnotationKind::Word {
                mosi: Some(0b1100_0001),
                miso: None
            }
        );
    }

    #[test]
    fn cs_active_high() {
        let c = cfg(
            0,
            Some(1),
            None,
            Some(2),
            true,
            SpiMode::Mode0,
            BitOrder::Msb,
            8,
        );
        let mut w = Waveform::new(0, Some(1), None, Some(2), SpiMode::Mode0, true, 10);
        w.idle(4);
        w.select();
        w.clock_word(&word_bits(0x7F, 8, BitOrder::Msb), &[]);
        w.deselect();
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(c, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::ChipSelect { active: true },
                AnnotationKind::Word {
                    mosi: Some(0x7F),
                    miso: None
                },
                AnnotationKind::ChipSelect { active: false },
            ]
        );
    }

    #[test]
    fn rejects_no_data_line() {
        let c = cfg(0, None, None, None, false, SpiMode::Mode0, BitOrder::Msb, 8);
        assert_eq!(SpiDecoder::new(c).unwrap_err(), SpiConfigError::NoDataLine);
    }

    #[test]
    fn rejects_bad_word_size() {
        let c = cfg(
            0,
            Some(1),
            None,
            None,
            false,
            SpiMode::Mode0,
            BitOrder::Msb,
            0,
        );
        assert_eq!(SpiDecoder::new(c).unwrap_err(), SpiConfigError::WordSize(0));
    }

    #[test]
    fn rejects_bit_collision() {
        let c = cfg(
            0,
            Some(0),
            None,
            None,
            false,
            SpiMode::Mode0,
            BitOrder::Msb,
            8,
        );
        assert_eq!(
            SpiDecoder::new(c).unwrap_err(),
            SpiConfigError::BitCollision { bit: 0 }
        );
    }

    #[test]
    fn finish_flushes_partial_word_as_incomplete() {
        let c = cfg(
            0,
            Some(1),
            None,
            None,
            false,
            SpiMode::Mode0,
            BitOrder::Msb,
            8,
        );
        let mut w = Waveform::new(0, Some(1), None, None, SpiMode::Mode0, false, 10);
        w.idle(4);
        w.clock_word(&word_bits(0b1010, 4, BitOrder::Msb), &[]); // only 4 of 8 bits
        let (transitions, end) = w.finish();
        let anns = decode_all(c, &transitions, end);
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].kind, AnnotationKind::Incomplete);
    }

    #[test]
    fn chunking_invariant() {
        let c = cfg(
            0,
            Some(1),
            Some(2),
            Some(3),
            false,
            SpiMode::Mode3,
            BitOrder::Msb,
            8,
        );
        let mut w = Waveform::new(0, Some(1), Some(2), Some(3), SpiMode::Mode3, false, 10);
        w.idle(3);
        w.select();
        w.clock_word(
            &word_bits(0x12, 8, BitOrder::Msb),
            &word_bits(0x9A, 8, BitOrder::Msb),
        );
        w.clock_word(
            &word_bits(0x34, 8, BitOrder::Msb),
            &word_bits(0xBC, 8, BitOrder::Msb),
        );
        w.clock_word(
            &word_bits(0x56, 8, BitOrder::Msb),
            &word_bits(0xDE, 8, BitOrder::Msb),
        );
        w.deselect();
        w.idle(3);
        let (transitions, end) = w.finish();
        let make = || SpiDecoder::new(c).unwrap();
        assert_all_split_points(&make, &transitions, end);

        proptest!(|(
            cuts in proptest::collection::vec(0usize..transitions.len(), 0..8),
            slacks in proptest::collection::vec(0u64..50, 0..8)
        )| {
            let plan = build_plan(&transitions, end, &cuts, &slacks);
            assert_same_result(&make, &transitions, end, &plan);
        });
    }
}
