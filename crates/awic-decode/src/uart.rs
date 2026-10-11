//! Streaming UART decoder (SPEC §6).
//!
//! Resynchronises on every start bit's falling edge rather than free-running
//! from the configured baud, so accumulated clock-vs-baud drift can never
//! walk a decode off frame boundaries the way it could on a long, un-resynced
//! run.

use crate::annotation::LEVEL_FRAME;
use crate::{
    Annotation, AnnotationKind, ChannelRole, Decoder, DecoderOption, LevelTracker, Transition,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopBits {
    One,
    OneAndHalf,
    Two,
}

impl StopBits {
    fn units(self) -> f64 {
        match self {
            StopBits::One => 1.0,
            StopBits::OneAndHalf => 1.5,
            StopBits::Two => 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOrder {
    /// First bit received is the least-significant bit (the common case).
    Lsb,
    /// First bit received is the most-significant bit.
    Msb,
}

/// UART construction parameters (SPEC §6). The rate and channel bit are
/// fixed for the decoder's lifetime.
#[derive(Debug, Clone, Copy)]
pub struct UartConfig {
    /// Which bit of the packed transition byte carries the data line.
    pub bit: u8,
    pub sample_rate_hz: u64,
    pub baud: u32,
    /// 5..=9.
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub bit_order: BitOrder,
    /// Idle-high is the default; set for a line that idles low.
    pub invert: bool,
}

/// Minimum samples/bit below which start-bit resynchronisation and center
/// sampling can no longer be trusted to land inside the right bit cell.
const MIN_SAMPLES_PER_BIT: f64 = 4.0;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum UartConfigError {
    #[error("UART data bits must be 5..=9, got {0}")]
    DataBits(u8),
    #[error(
        "baud {baud} needs at least {min} samples/bit at a {rate} Hz sample rate; \
         this combination gives {actual:.2}"
    )]
    BaudTooFast {
        baud: u32,
        rate: u64,
        min: u32,
        actual: f64,
    },
}

const OPTIONS: &[DecoderOption] = &[
    DecoderOption {
        id: "baud",
        description: "Baud rate, any integer bit rate the sample rate can resolve",
    },
    DecoderOption {
        id: "data_bits",
        description: "Data bits per frame, 5-9",
    },
    DecoderOption {
        id: "parity",
        description: "none | even | odd",
    },
    DecoderOption {
        id: "stop_bits",
        description: "1 | 1.5 | 2",
    },
    DecoderOption {
        id: "bit_order",
        description: "lsb (default) | msb",
    },
    DecoderOption {
        id: "invert",
        description: "invert idle polarity: line idles low instead of high",
    },
];

const CHANNELS: [ChannelRole; 1] = [ChannelRole::Rx];

#[derive(Debug, Clone)]
struct FrameState {
    start_sample: u64,
    /// Sampled mark(`true`)/space(`false`) value of each data bit, in the
    /// order received.
    data_bits: Vec<bool>,
    parity_bit: Option<bool>,
    /// Index into the frame's phase sequence: 0 = start bit, 1..=data_bits =
    /// data bits, then the parity bit if configured, then the stop bit.
    next_idx: u8,
}

#[derive(Debug)]
enum Phase {
    Idle,
    Frame(FrameState),
}

/// Streaming UART decoder over one data line (SPEC §6).
#[derive(Debug)]
pub struct UartDecoder {
    cfg: UartConfig,
    samples_per_bit: f64,
    /// The raw line level (true = electrically high) that represents mark
    /// (idle / logical 1). `!invert`.
    mark_level: bool,
    tracker: LevelTracker,
    phase: Phase,
    name: String,
}

fn phase_center(start: u64, idx: u8, samples_per_bit: f64) -> u64 {
    start + ((idx as f64 + 0.5) * samples_per_bit).round() as u64
}

enum Step {
    Done(Annotation),
    Pending,
}

impl UartDecoder {
    pub fn new(cfg: UartConfig) -> Result<Self, UartConfigError> {
        if !(5..=9).contains(&cfg.data_bits) {
            return Err(UartConfigError::DataBits(cfg.data_bits));
        }
        let samples_per_bit = cfg.sample_rate_hz as f64 / cfg.baud as f64;
        if samples_per_bit < MIN_SAMPLES_PER_BIT {
            return Err(UartConfigError::BaudTooFast {
                baud: cfg.baud,
                rate: cfg.sample_rate_hz,
                min: MIN_SAMPLES_PER_BIT as u32,
                actual: samples_per_bit,
            });
        }
        let mark_level = !cfg.invert;
        let initial_byte = if mark_level { 1u8 << cfg.bit } else { 0 };
        Ok(UartDecoder {
            cfg,
            samples_per_bit,
            mark_level,
            tracker: LevelTracker::new(initial_byte),
            phase: Phase::Idle,
            name: format!("UART @ {} baud", cfg.baud),
        })
    }

    fn stop_idx(&self) -> u8 {
        1 + self.cfg.data_bits + u8::from(self.cfg.parity != Parity::None)
    }

    fn frame_end(&self, start: u64) -> u64 {
        let total_units = 1.0 // start bit
            + self.cfg.data_bits as f64
            + f64::from(u8::from(self.cfg.parity != Parity::None))
            + self.cfg.stop_bits.units();
        start + (total_units * self.samples_per_bit).round() as u64
    }

    /// Advance one frame as far as known input allows. Returns `Step::Done`
    /// with the finished annotation (success or error) once the frame's
    /// stop bit (or a false-start abort) has been resolved, else
    /// `Step::Pending` when more input is needed.
    fn step_frame(&mut self, state: &mut FrameState) -> Step {
        let stop_idx = self.stop_idx();
        loop {
            let idx = state.next_idx;
            let center = phase_center(state.start_sample, idx, self.samples_per_bit);
            if center > self.tracker.known_until() {
                return Step::Pending;
            }
            let is_mark = self.tracker.bit_at(center, self.cfg.bit) == self.mark_level;
            if idx == 0 {
                if is_mark {
                    // The line returned to mark before the start bit's
                    // center: a glitch, not a real start. Report it rather
                    // than silently mis-framing everything after it (this
                    // is what keeps the CAL-square-wave-at-wrong-baud case
                    // from cascading into nonsense).
                    return Step::Done(Annotation::new(
                        state.start_sample,
                        center,
                        LEVEL_FRAME,
                        AnnotationKind::Error("false start bit".into()),
                    ));
                }
            } else if idx <= self.cfg.data_bits {
                state.data_bits.push(is_mark);
            } else if idx == stop_idx {
                let end = self.frame_end(state.start_sample);
                return Step::Done(self.finalize(state, is_mark, end));
            } else {
                // Parity bit position.
                state.parity_bit = Some(is_mark);
            }
            state.next_idx += 1;
        }
    }

    fn assemble_byte(&self, bits: &[bool]) -> u16 {
        let mut value: u16 = 0;
        for (i, &mark) in bits.iter().enumerate() {
            if !mark {
                continue;
            }
            let pos = match self.cfg.bit_order {
                BitOrder::Lsb => i,
                BitOrder::Msb => bits.len() - 1 - i,
            };
            value |= 1 << pos;
        }
        value
    }

    fn finalize(&self, state: &FrameState, stop_is_mark: bool, end: u64) -> Annotation {
        let all_space = state.data_bits.iter().all(|&b| !b)
            && state.parity_bit.map(|p| !p).unwrap_or(true)
            && !stop_is_mark;
        let kind = if all_space {
            AnnotationKind::Break
        } else if !stop_is_mark {
            AnnotationKind::FramingError
        } else if let Some(parity_bit) = state.parity_bit {
            let ones = state.data_bits.iter().filter(|&&b| b).count() + usize::from(parity_bit);
            let ok = match self.cfg.parity {
                Parity::Even => ones.is_multiple_of(2),
                Parity::Odd => ones % 2 == 1,
                Parity::None => unreachable!("parity_bit only set when parity is configured"),
            };
            if ok {
                AnnotationKind::Data(self.assemble_byte(&state.data_bits))
            } else {
                AnnotationKind::ParityError
            }
        } else {
            AnnotationKind::Data(self.assemble_byte(&state.data_bits))
        };
        let mut annotation = Annotation::new(state.start_sample, end, LEVEL_FRAME, kind);
        // A UART byte is usually text: show the character too (`0x68 'h'`),
        // so a reader — or an agent — needs no ASCII table (live test B-12).
        if let AnnotationKind::Data(v) = annotation.kind {
            if (0x20..=0x7E).contains(&v) {
                annotation.text = format!("0x{v:02X} '{}'", v as u8 as char);
            }
        }
        annotation
    }
}

impl Decoder for UartDecoder {
    fn name(&self) -> &str {
        &self.name
    }

    fn required_channels(&self) -> &[ChannelRole] {
        &CHANNELS
    }

    fn options(&self) -> &[DecoderOption] {
        OPTIONS
    }

    fn feed(&mut self, transitions: &[Transition], valid_until: u64) -> Vec<Annotation> {
        self.tracker.push(transitions, valid_until);
        let mut out = Vec::new();
        loop {
            if matches!(self.phase, Phase::Idle) {
                let mask = 1 << self.cfg.bit;
                match self.tracker.next_relevant(mask) {
                    Some((sample, _before, after)) => {
                        let bit_is_mark = (after >> self.cfg.bit) & 1 == 1;
                        if bit_is_mark == self.mark_level {
                            // Transitioned into mark while idle: no-op, keep
                            // scanning for the falling edge.
                            continue;
                        }
                        self.phase = Phase::Frame(FrameState {
                            start_sample: sample,
                            data_bits: Vec::with_capacity(self.cfg.data_bits as usize),
                            parity_bit: None,
                            next_idx: 0,
                        });
                    }
                    None => break,
                }
                continue;
            }
            let Phase::Frame(mut state) = std::mem::replace(&mut self.phase, Phase::Idle) else {
                unreachable!()
            };
            match self.step_frame(&mut state) {
                Step::Done(ann) => out.push(ann),
                Step::Pending => {
                    self.phase = Phase::Frame(state);
                    break;
                }
            }
        }
        out
    }

    /// Deliberately ignored. A line at space (low) on the first sample is
    /// most often a start bit — a capture that begins on one — and taking it
    /// as one is then exactly right; input that starts mid-frame misframes
    /// until the next idle gap whichever way it is read, because nothing on
    /// a UART line marks a frame boundary except idle. Unlike I²C's START or
    /// SPI's CS, there is no later event to wait for instead.
    fn seed(&mut self, _levels: u8) {}

    fn finish(&mut self, end_sample: u64) -> Vec<Annotation> {
        let Phase::Frame(state) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return Vec::new();
        };
        let mut text = format!("incomplete frame: {} bit(s) sampled", state.data_bits.len());
        if !state.data_bits.is_empty() {
            let byte = self.assemble_byte(&state.data_bits);
            text = format!("{text} (partial value 0x{byte:X})");
        }
        vec![Annotation {
            start_sample: state.start_sample,
            end_sample: end_sample.max(state.start_sample),
            level: LEVEL_FRAME,
            kind: AnnotationKind::Incomplete,
            text,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{assert_all_split_points, assert_same_result, build_plan};
    use proptest::proptest;

    /// Synthesises the transition list a real UART line would produce: an
    /// edge-driven waveform builder, mirroring the decoder's own notion of
    /// mark/space and center sampling, so tests exercise the decoder
    /// against something realistic rather than hand-picked transitions.
    struct Waveform {
        bit: u8,
        data_bits: u8,
        parity: Parity,
        stop_bits: StopBits,
        bit_order: BitOrder,
        invert: bool,
        samples_per_bit: f64,
        cursor: u64,
        mark: bool,
        transitions: Vec<Transition>,
    }

    fn natural_parity_bit(ones_in_data: usize, parity: Parity) -> bool {
        match parity {
            Parity::Even => ones_in_data % 2 == 1,
            Parity::Odd => ones_in_data.is_multiple_of(2),
            Parity::None => false,
        }
    }

    impl Waveform {
        fn new(cfg: &UartConfig) -> Self {
            Waveform {
                bit: cfg.bit,
                data_bits: cfg.data_bits,
                parity: cfg.parity,
                stop_bits: cfg.stop_bits,
                bit_order: cfg.bit_order,
                invert: cfg.invert,
                samples_per_bit: cfg.sample_rate_hz as f64 / cfg.baud as f64,
                cursor: 0,
                mark: true,
                transitions: Vec::new(),
            }
        }

        fn set_mark(&mut self, mark: bool) {
            if mark != self.mark {
                let physical_high = mark == !self.invert;
                let value = if physical_high { 1u8 << self.bit } else { 0 };
                self.transitions.push(Transition {
                    sample_index: self.cursor,
                    value,
                });
                self.mark = mark;
            }
        }

        fn advance_units(&mut self, units: f64) {
            self.cursor += (units * self.samples_per_bit).round() as u64;
        }

        fn idle(&mut self, units: f64) {
            self.set_mark(true);
            self.advance_units(units);
        }

        fn bits_for(&self, value: u16) -> Vec<bool> {
            (0..self.data_bits)
                .map(|i| {
                    let pos = match self.bit_order {
                        BitOrder::Lsb => i,
                        BitOrder::Msb => self.data_bits - 1 - i,
                    };
                    (value >> pos) & 1 == 1
                })
                .collect()
        }

        /// Sends one frame. `parity_override` forces the parity bit's
        /// value (to inject a parity error); `stop_mark` forces the first
        /// stop bit's level (`false` to inject a framing error).
        fn send_custom(&mut self, value: u16, parity_override: Option<bool>, stop_mark: bool) {
            self.set_mark(false);
            self.advance_units(1.0);
            let bits = self.bits_for(value);
            for bit in &bits {
                self.set_mark(*bit);
                self.advance_units(1.0);
            }
            if self.parity != Parity::None {
                let ones = bits.iter().filter(|&&b| b).count();
                let p = parity_override.unwrap_or_else(|| natural_parity_bit(ones, self.parity));
                self.set_mark(p);
                self.advance_units(1.0);
            }
            self.set_mark(stop_mark);
            self.advance_units(self.stop_bits.units());
        }

        fn send(&mut self, value: u16) {
            self.send_custom(value, None, true);
        }

        /// Holds space through a whole frame duration and into where the
        /// stop bit should be: SPEC's "line low longer than a frame".
        fn send_break(&mut self) {
            let total_units = 1.0
                + self.data_bits as f64
                + f64::from(u8::from(self.parity != Parity::None))
                + self.stop_bits.units();
            self.set_mark(false);
            self.advance_units(total_units);
            self.set_mark(true);
        }

        fn finish(self) -> (Vec<Transition>, u64) {
            (self.transitions, self.cursor)
        }
    }

    #[allow(clippy::too_many_arguments)] // test helper mirroring UartConfig's fields 1:1
    fn cfg(
        bit: u8,
        sample_rate_hz: u64,
        baud: u32,
        data_bits: u8,
        parity: Parity,
        stop_bits: StopBits,
        bit_order: BitOrder,
        invert: bool,
    ) -> UartConfig {
        UartConfig {
            bit,
            sample_rate_hz,
            baud,
            data_bits,
            parity,
            stop_bits,
            bit_order,
            invert,
        }
    }

    fn decode_all(c: UartConfig, transitions: &[Transition], end: u64) -> Vec<Annotation> {
        let mut d = UartDecoder::new(c).unwrap();
        let mut out = d.feed(transitions, end);
        out.extend(d.finish(end));
        out
    }

    #[test]
    fn decodes_bytes_115200_at_1mhz() {
        let c = cfg(
            0,
            1_000_000,
            115_200,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(3.0);
        for &b in &[0x41u16, 0x00, 0xFF, 0xAA, 0x55] {
            w.send(b);
        }
        w.idle(3.0);
        let (transitions, end) = w.finish();
        let kinds: Vec<_> = decode_all(c, &transitions, end)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Data(0x41),
                AnnotationKind::Data(0x00),
                AnnotationKind::Data(0xFF),
                AnnotationKind::Data(0xAA),
                AnnotationKind::Data(0x55),
            ]
        );
    }

    #[test]
    fn decodes_bytes_115200_at_24mhz() {
        let c = cfg(
            0,
            24_000_000,
            115_200,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(3.0);
        for &b in &[0x12u16, 0x34, 0x56] {
            w.send(b);
        }
        w.idle(3.0);
        let (transitions, end) = w.finish();
        let kinds: Vec<_> = decode_all(c, &transitions, end)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Data(0x12),
                AnnotationKind::Data(0x34),
                AnnotationKind::Data(0x56)
            ]
        );
    }

    #[test]
    fn msb_first_and_inverted_idle() {
        let c = cfg(
            0,
            1_000_000,
            9600,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Msb,
            true,
        );
        let mut w = Waveform::new(&c);
        w.idle(3.0);
        w.send(0b1011_0001);
        w.idle(3.0);
        let (transitions, end) = w.finish();
        let kinds: Vec<_> = decode_all(c, &transitions, end)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(kinds, vec![AnnotationKind::Data(0b1011_0001)]);
    }

    #[test]
    fn parity_ok_and_parity_error() {
        let c = cfg(
            0,
            1_000_000,
            115_200,
            8,
            Parity::Even,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(3.0);
        w.send(0x41); // 2 ones: correct even parity computed automatically
        w.send_custom(0x41, Some(true), true); // force the wrong parity bit
        w.idle(3.0);
        let (transitions, end) = w.finish();
        let kinds: Vec<_> = decode_all(c, &transitions, end)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![AnnotationKind::Data(0x41), AnnotationKind::ParityError]
        );
    }

    #[test]
    fn odd_parity_ok() {
        let c = cfg(
            0,
            1_000_000,
            115_200,
            8,
            Parity::Odd,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(3.0);
        w.send(0x00); // 0 ones -> odd parity bit must be 1
        w.idle(3.0);
        let (transitions, end) = w.finish();
        let kinds: Vec<_> = decode_all(c, &transitions, end)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(kinds, vec![AnnotationKind::Data(0x00)]);
    }

    #[test]
    fn framing_error_on_low_stop_bit() {
        let c = cfg(
            0,
            1_000_000,
            115_200,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(3.0);
        w.send_custom(0x3C, None, false);
        w.idle(3.0);
        let (transitions, end) = w.finish();
        let kinds: Vec<_> = decode_all(c, &transitions, end)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(kinds, vec![AnnotationKind::FramingError]);
    }

    #[test]
    fn break_is_reported_and_decoding_resumes() {
        let c = cfg(
            0,
            1_000_000,
            115_200,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(3.0);
        w.send_break();
        w.idle(3.0);
        w.send(0x41);
        w.idle(3.0);
        let (transitions, end) = w.finish();
        let kinds: Vec<_> = decode_all(c, &transitions, end)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![AnnotationKind::Break, AnnotationKind::Data(0x41)]
        );
    }

    #[test]
    fn rejects_unresolvable_baud() {
        let c = cfg(
            0,
            1_000_000,
            500_000,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        assert!(matches!(
            UartDecoder::new(c),
            Err(UartConfigError::BaudTooFast { .. })
        ));
    }

    #[test]
    fn rejects_bad_data_bits() {
        let c = cfg(
            0,
            1_000_000,
            9600,
            4,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        assert_eq!(
            UartDecoder::new(c).unwrap_err(),
            UartConfigError::DataBits(4)
        );
    }

    #[test]
    fn finish_flushes_incomplete_frame() {
        let c = cfg(
            0,
            1_000_000,
            115_200,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(1.0);
        w.set_mark(false); // start bit only, nothing else
        w.advance_units(1.0);
        w.set_mark(true);
        w.advance_units(2.0);
        let (transitions, end) = w.finish();
        let mut d = UartDecoder::new(c).unwrap();
        let mid = d.feed(&transitions, end);
        assert!(mid.is_empty(), "frame should still be open: {mid:?}");
        let flushed = d.finish(end);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].kind, AnnotationKind::Incomplete);
    }

    #[test]
    fn chunking_invariant() {
        let c = cfg(
            0,
            1_000_000,
            115_200,
            8,
            Parity::Even,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let mut w = Waveform::new(&c);
        w.idle(2.0);
        for &b in &[0x00u16, 0x41, 0xFF, 0x7E, 0x80] {
            w.send(b);
        }
        w.idle(2.0);
        let (transitions, end) = w.finish();
        let make = || UartDecoder::new(c).unwrap();
        assert_all_split_points(&make, &transitions, end);

        proptest!(|(
            cuts in proptest::collection::vec(0usize..transitions.len(), 0..6),
            slacks in proptest::collection::vec(0u64..2000, 0..6)
        )| {
            let plan = build_plan(&transitions, end, &cuts, &slacks);
            assert_same_result(&make, &transitions, end, &plan);
        });
    }

    #[test]
    fn cal_square_wave_does_not_panic() {
        // The bench's 1 kHz calibration square wave, fed to a UART decoder
        // configured for a baud that has nothing to do with it (SPEC's
        // "what does garbage input produce" acceptance case).
        let c = cfg(
            0,
            1_000_000,
            2000,
            8,
            Parity::None,
            StopBits::One,
            BitOrder::Lsb,
            false,
        );
        let half_period = 500u64; // 1 kHz at 1 MHz sample rate
        let mut transitions = Vec::new();
        let mut cursor = 0u64;
        let mut mark = true;
        for _ in 0..40 {
            cursor += half_period;
            mark = !mark;
            transitions.push(Transition {
                sample_index: cursor,
                value: if mark { 1 } else { 0 },
            });
        }
        let end = cursor + half_period;
        let mut d = UartDecoder::new(c).unwrap();
        let mut anns = d.feed(&transitions, end);
        anns.extend(d.finish(end));
        for a in &anns {
            assert!(a.start_sample <= a.end_sample);
            assert!(a.end_sample <= end);
        }
    }
}
