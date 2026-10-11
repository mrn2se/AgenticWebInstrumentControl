//! Streaming CAN decoder, classic CAN 2.0A/B (SPEC §6).
//!
//! Reads one line — the RX pin of a transceiver, or CANL, or CANH, or the
//! differential CANH−CANL thresholded to a level — and finds frames by timing
//! alone: a recessive→dominant edge after at least [`IDLE_BITS`] recessive bits
//! is a start of frame and hard-synchronises the bit clock, and every later
//! recessive→dominant edge re-anchors it (resynchronisation), so a transmitter
//! whose clock is a percent or two off the configured bit rate still decodes
//! over a 130-bit frame. Bits are sampled at the configured sample point,
//! destuffed, and parsed into fields; the CRC-15 is checked.
//!
//! What happens around a frame is the part that differs from the other
//! decoders. After a frame, or after anything that is not one (a stuff error,
//! a form error, a stretch of dominant bits that is an error flag), the
//! decoder is *idle* and waits for the bus to be recessive for [`IDLE_BITS`]
//! bits before it believes another start of frame. That is what skips an
//! error flag and its delimiter, the 3-bit intermission, and the tail of a
//! frame a decode window opened in the middle of.

use crate::annotation::{LEVEL_BIT, LEVEL_FIELD, LEVEL_FRAME};
use crate::{
    Annotation, AnnotationKind, ChannelRole, Decoder, DecoderOption, LevelTracker, Transition,
};

/// Which line level is the dominant (logical 0) bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    /// The RX pin of a transceiver, or CANL: dominant is the low level.
    DominantLow,
    /// CANH, or CANH−CANL thresholded: dominant is the high level.
    DominantHigh,
}

/// CAN construction parameters (SPEC §6). The rate and channel bit are fixed
/// for the decoder's lifetime.
#[derive(Debug, Clone, Copy)]
pub struct CanConfig {
    /// Which bit of the packed transition byte carries the bus line.
    pub bit: u8,
    pub sample_rate_hz: u64,
    /// Bits per second.
    pub bitrate: u32,
    /// Where in the bit the line is sampled, in percent of the bit time.
    pub sample_point: u8,
    pub polarity: Polarity,
}

/// Minimum samples/bit below which resynchronisation and sampling can no
/// longer be trusted to land inside the right bit cell.
const MIN_SAMPLES_PER_BIT: f64 = 4.0;
/// Sample points outside this range (percent) are not any real bit timing.
const SAMPLE_POINT_RANGE: std::ops::RangeInclusive<u8> = 50..=90;
/// Recessive bits after which a dominant edge is taken as a start of frame:
/// the 11 of a bus that has been idle for a while, less one for the clock
/// error a real bit time has against the configured one. An ACK delimiter,
/// EOF and intermission already make 11, so a frame following another one
/// closely is accepted too — and so is a dominant third intermission bit,
/// which the standard also treats as a start of frame.
const IDLE_BITS: f64 = 10.0;
/// Dominant bits that make a pulse an error (or overload) flag: six, less half
/// a bit for edge placement.
const ERROR_FLAG_BITS: f64 = 5.5;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CanConfigError {
    #[error("CAN bit rate must be at least 1 bit/s")]
    NoBitrate,
    #[error("CAN sample point must be 50..=90 percent, got {0}")]
    SamplePoint(u8),
    #[error(
        "bit rate {bitrate} needs at least {min} samples/bit at a {rate} Hz sample rate; \
         this combination gives {actual:.2}"
    )]
    BitrateTooFast {
        bitrate: u32,
        rate: u64,
        min: u32,
        actual: f64,
    },
}

const OPTIONS: &[DecoderOption] = &[
    DecoderOption {
        id: "bitrate",
        description: "Bit rate in bit/s (default 500000)",
    },
    DecoderOption {
        id: "sample_point",
        description: "Sample point in percent of the bit, 50-90 (default 75)",
    },
    DecoderOption {
        id: "polarity",
        description: "dominant_low (default: RX pin or CANL) | dominant_high (CANH or CANH-CANL)",
    },
];

const CHANNELS: [ChannelRole; 1] = [ChannelRole::Can];

/// The CAN CRC-15 (polynomial 0x4599) after one more bit, `true` being the
/// recessive (logical 1) level.
fn crc15_step(crc: u16, bit: bool) -> u16 {
    let next = u16::from(bit) ^ (crc >> 14 & 1);
    let crc = (crc << 1) & 0x7FFF;
    if next == 1 {
        crc ^ 0x4599
    } else {
        crc
    }
}

/// Where a frame's parse stands. Ordered: everything up to and including
/// [`Stage::CrcDelim`] is bit-stuffed (a stuff bit may even sit between the
/// last CRC bit and the delimiter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Sof,
    BaseId,
    /// RTR in a base frame, SRR in an extended one.
    RtrSrr,
    Ide,
    ExtId,
    ExtRtr,
    /// r0 in a base frame, r1 and r0 in an extended one.
    Reserved,
    Dlc,
    Data,
    Crc,
    CrcDelim,
    AckSlot,
    AckDelim,
    Eof,
}

/// What a sampled bit did to the frame in progress.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    /// The frame ran through EOF.
    Done,
    /// The frame ended early: a stuff or form error, or an error flag. The
    /// string is set when the offending dominant bit, if it turns out to be a
    /// short pulse rather than an error flag, deserves a form error.
    Abort(Option<&'static str>),
    /// The start of frame bit read recessive: a glitch, not a frame.
    Glitch,
}

#[derive(Debug)]
struct FrameState {
    /// The sample of the start-of-frame edge.
    start: u64,
    /// The start of bit `origin_bit`, in samples: the bit clock's phase,
    /// re-anchored by every recessive→dominant edge.
    origin: f64,
    origin_bit: u64,
    /// The next raw bit (stuff bits included) to sample; the SOF is bit 0.
    next_bit: u64,
    stage: Stage,
    /// Bits taken into the current stage and their value, first bit highest.
    stage_bits: u8,
    stage_val: u32,
    field_start: u64,
    field_end: u64,
    /// Destuffing: the level and length of the current run of equal bits.
    run_val: bool,
    run_len: u8,
    crc: u16,
    destuffed: u32,
    id: u32,
    id_start: u64,
    id_end: u64,
    rtr: bool,
    rtr_span: (u64, u64),
    extended: bool,
    ide_span: (u64, u64),
    dlc: u8,
    data: Vec<u8>,
    crc_ok: bool,
    acked: bool,
    ack_start: u64,
    eof_bits: u8,
    eof_start: u64,
}

impl FrameState {
    fn new(start: u64) -> Self {
        FrameState {
            start,
            origin: start as f64,
            origin_bit: 0,
            next_bit: 0,
            stage: Stage::Sof,
            stage_bits: 0,
            stage_val: 0,
            field_start: start,
            field_end: start,
            run_val: false,
            run_len: 0,
            crc: 0,
            destuffed: 0,
            id: 0,
            id_start: start,
            id_end: start,
            rtr: false,
            rtr_span: (start, start),
            extended: false,
            ide_span: (start, start),
            dlc: 0,
            data: Vec::new(),
            crc_ok: false,
            acked: false,
            ack_start: start,
            eof_bits: 0,
            eof_start: start,
        }
    }

    fn bit_start(&self, bit: u64, samples_per_bit: f64) -> f64 {
        self.origin + (bit - self.origin_bit) as f64 * samples_per_bit
    }

    /// Bits in the current stage.
    fn width(&self) -> u8 {
        match self.stage {
            Stage::BaseId => 11,
            Stage::ExtId => 18,
            Stage::Reserved if self.extended => 2,
            Stage::Dlc => 4,
            Stage::Crc => 15,
            Stage::Data => 8,
            _ => 1,
        }
    }

    /// Number of data bytes this frame carries: a remote frame none whatever
    /// its DLC, and a DLC of 9..=15 is 8 bytes in classic CAN.
    fn data_len(&self) -> usize {
        if self.rtr {
            0
        } else {
            usize::from(self.dlc.min(8))
        }
    }

    fn frame(&self, end: u64) -> Annotation {
        Annotation::new(
            self.start,
            end,
            LEVEL_FRAME,
            AnnotationKind::CanFrame {
                id: self.id,
                extended: self.extended,
                remote: self.rtr,
                dlc: self.dlc,
                data: self.data.clone(),
                crc_ok: self.crc_ok,
                acked: self.acked,
            },
        )
    }

    /// Takes one sampled raw bit: destuffs it and parses what is left.
    fn push_bit(
        &mut self,
        dominant: bool,
        start: u64,
        end: u64,
        out: &mut Vec<Annotation>,
    ) -> Flow {
        let value = !dominant;
        if self.stage <= Stage::CrcDelim && self.run_len == 5 {
            if value == self.run_val {
                // The SOF and nothing but recessive bits after it: not a
                // broken frame but one dominant bit on its own — another
                // node's ACK on a line that does not see the frame it
                // acknowledges (CANH of a node that never drives it, #32).
                let text = if self.destuffed == 6 && self.run_val {
                    "lone dominant bit, not a frame: an ACK, or a frame this line does not carry"
                } else {
                    "stuff error"
                };
                out.push(Annotation::new(
                    self.start,
                    end,
                    LEVEL_FRAME,
                    AnnotationKind::Error(text.into()),
                ));
                return Flow::Abort(None);
            }
            out.push(Annotation::new(
                start,
                end,
                LEVEL_BIT,
                AnnotationKind::StuffBit,
            ));
            self.run_val = value;
            self.run_len = 1;
            return Flow::Continue;
        }
        if self.stage < Stage::CrcDelim {
            if self.stage == Stage::Sof && value {
                return Flow::Glitch;
            }
            if self.run_len > 0 && value == self.run_val {
                self.run_len += 1;
            } else {
                self.run_val = value;
                self.run_len = 1;
            }
            return self.field_bit(value, start, end, out);
        }
        self.fixed_bit(dominant, start, end, out)
    }

    /// One destuffed bit of the stuffed region, SOF through the CRC.
    fn field_bit(&mut self, value: bool, start: u64, end: u64, out: &mut Vec<Annotation>) -> Flow {
        self.destuffed += 1;
        if self.stage < Stage::Crc {
            self.crc = crc15_step(self.crc, value);
        }
        if self.stage_bits == 0 {
            self.field_start = start;
        }
        self.field_end = end;
        self.stage_bits += 1;
        self.stage_val = (self.stage_val << 1) | u32::from(value);
        if self.stage_bits < self.width() {
            return Flow::Continue;
        }
        let val = self.stage_val;
        let span = (self.field_start, self.field_end);
        self.stage_bits = 0;
        self.stage_val = 0;
        let mut field = |kind: AnnotationKind, (from, to): (u64, u64)| {
            out.push(Annotation::new(from, to, LEVEL_FIELD, kind));
        };
        match self.stage {
            Stage::Sof => {
                field(AnnotationKind::Start, span);
                self.stage = Stage::BaseId;
            }
            Stage::BaseId => {
                self.id = val;
                self.id_start = span.0;
                self.id_end = span.1;
                self.stage = Stage::RtrSrr;
            }
            Stage::RtrSrr => {
                self.rtr = value;
                self.rtr_span = span;
                self.stage = Stage::Ide;
            }
            Stage::Ide => {
                self.extended = value;
                self.ide_span = span;
                if value {
                    self.stage = Stage::ExtId;
                } else {
                    field(
                        AnnotationKind::CanId {
                            id: self.id,
                            extended: false,
                        },
                        (self.id_start, self.id_end),
                    );
                    field(AnnotationKind::CanRtr { remote: self.rtr }, self.rtr_span);
                    field(AnnotationKind::CanIde { extended: false }, span);
                    self.stage = Stage::Reserved;
                }
            }
            Stage::ExtId => {
                self.id = (self.id << 18) | val;
                field(
                    AnnotationKind::CanId {
                        id: self.id,
                        extended: true,
                    },
                    (self.id_start, span.1),
                );
                field(AnnotationKind::CanIde { extended: true }, self.ide_span);
                self.stage = Stage::ExtRtr;
            }
            Stage::ExtRtr => {
                // The SRR before the IDE was recessive by the standard and
                // means nothing; this is the real RTR.
                self.rtr = value;
                field(AnnotationKind::CanRtr { remote: value }, span);
                self.stage = Stage::Reserved;
            }
            Stage::Reserved => {
                // r0 of a base frame, r1 of an extended one, is FDF in CAN FD:
                // recessive there means the rest is laid out (and clocked)
                // differently. Say so rather than misread it as a CRC error.
                let fdf = if self.extended { val >> 1 } else { val };
                if fdf == 1 {
                    out.push(Annotation::new(
                        self.start,
                        span.1,
                        LEVEL_FRAME,
                        AnnotationKind::Error("CAN FD frame (FDF set): not decoded".into()),
                    ));
                    return Flow::Abort(None);
                }
                self.stage = Stage::Dlc;
            }
            Stage::Dlc => {
                self.dlc = val as u8;
                field(AnnotationKind::CanDlc(self.dlc), span);
                self.stage = if self.data_len() > 0 {
                    Stage::Data
                } else {
                    Stage::Crc
                };
            }
            Stage::Data => {
                self.data.push(val as u8);
                field(AnnotationKind::Data(val as u16), span);
                if self.data.len() >= self.data_len() {
                    self.stage = Stage::Crc;
                }
            }
            Stage::Crc => {
                self.crc_ok = val as u16 == self.crc;
                let mut ann = Annotation::new(
                    span.0,
                    span.1,
                    LEVEL_FIELD,
                    AnnotationKind::CanCrc {
                        value: val as u16,
                        ok: self.crc_ok,
                    },
                );
                if !self.crc_ok {
                    ann.text = format!("CRC 0x{val:04X} (expected 0x{:04X})", self.crc);
                }
                out.push(ann);
                self.stage = Stage::CrcDelim;
            }
            _ => unreachable!("not a stuffed stage"),
        }
        Flow::Continue
    }

    /// One bit of the fixed-form tail: CRC delimiter, ACK slot and
    /// delimiter, EOF. No stuffing there, and a dominant bit where a
    /// recessive one is required is a form error.
    fn fixed_bit(
        &mut self,
        dominant: bool,
        start: u64,
        end: u64,
        out: &mut Vec<Annotation>,
    ) -> Flow {
        match self.stage {
            Stage::CrcDelim => {
                if dominant {
                    out.push(Annotation::new(
                        self.start,
                        end,
                        LEVEL_FRAME,
                        AnnotationKind::Error("form error: CRC delimiter".into()),
                    ));
                    return Flow::Abort(None);
                }
                self.stage = Stage::AckSlot;
            }
            Stage::AckSlot => {
                self.acked = dominant;
                self.ack_start = start;
                let (kind, text) = if dominant {
                    (AnnotationKind::Ack, "ACK")
                } else {
                    (AnnotationKind::Nack, "no ACK")
                };
                let mut ann = Annotation::new(start, end, LEVEL_BIT, kind);
                ann.text = text.to_string();
                out.push(ann);
                self.stage = Stage::AckDelim;
            }
            Stage::AckDelim => {
                // A dominant delimiter after a recessive slot is the
                // transmitter's own error flag for the missing ACK: the
                // frame ends here, and the flag is recognised as such once
                // it ends.
                let end_of_frame = if dominant { start } else { end };
                let (kind, text) = if self.acked {
                    (AnnotationKind::Ack, "ACK")
                } else {
                    (AnnotationKind::Nack, "no ACK")
                };
                let mut ann = Annotation::new(self.ack_start, end_of_frame, LEVEL_FIELD, kind);
                ann.text = text.to_string();
                out.push(ann);
                if dominant {
                    out.push(self.frame(start));
                    return Flow::Abort(self.acked.then_some("form error: ACK delimiter"));
                }
                self.stage = Stage::Eof;
            }
            Stage::Eof => {
                if self.eof_bits == 0 {
                    self.eof_start = start;
                }
                // A dominant last EOF bit is an overload flag or another
                // node's error flag; the frame itself was received.
                if dominant && self.eof_bits < 6 {
                    out.push(self.frame(start));
                    return Flow::Abort(Some("form error: EOF"));
                }
                self.eof_bits += 1;
                if self.eof_bits == 7 {
                    out.push(Annotation::new(
                        self.eof_start,
                        end,
                        LEVEL_FIELD,
                        AnnotationKind::CanEof,
                    ));
                    out.push(self.frame(end));
                    return Flow::Done;
                }
            }
            _ => unreachable!("not a fixed-form stage"),
        }
        Flow::Continue
    }
}

#[derive(Debug)]
enum Phase {
    Idle,
    Frame(Box<FrameState>),
}

/// Streaming CAN decoder over one bus line (SPEC §6).
#[derive(Debug)]
pub struct CanDecoder {
    cfg: CanConfig,
    samples_per_bit: f64,
    /// How far into the bit the sample point lies, in samples.
    sample_offset: f64,
    tracker: LevelTracker,
    /// Whether the line is dominant, as of the last edge taken.
    dominant: bool,
    last_rise: u64,
    last_fall: u64,
    /// No input seen yet and no `seed`: the bus is assumed idle, so a first
    /// dominant edge is a start of frame.
    assume_idle: bool,
    /// Seeded at this sample and no bus idle seen since: activity here is
    /// the tail of a frame the input opened in, reported, not decoded.
    unsynced_since: Option<u64>,
    /// The first such edge, once one was skipped.
    skipped_from: Option<u64>,
    /// Idle and the line went dominant at this sample without it being a
    /// start of frame: an error flag if it lasts six bits.
    flag_since: Option<u64>,
    /// What to report if that dominant pulse is too short for an error flag.
    flag_form_error: Option<&'static str>,
    phase: Phase,
    name: String,
}

impl CanDecoder {
    pub fn new(cfg: CanConfig) -> Result<Self, CanConfigError> {
        if cfg.bitrate == 0 {
            return Err(CanConfigError::NoBitrate);
        }
        if !SAMPLE_POINT_RANGE.contains(&cfg.sample_point) {
            return Err(CanConfigError::SamplePoint(cfg.sample_point));
        }
        let samples_per_bit = cfg.sample_rate_hz as f64 / f64::from(cfg.bitrate);
        if samples_per_bit < MIN_SAMPLES_PER_BIT {
            return Err(CanConfigError::BitrateTooFast {
                bitrate: cfg.bitrate,
                rate: cfg.sample_rate_hz,
                min: MIN_SAMPLES_PER_BIT as u32,
                actual: samples_per_bit,
            });
        }
        let recessive_byte = match cfg.polarity {
            Polarity::DominantLow => 1u8 << cfg.bit,
            Polarity::DominantHigh => 0,
        };
        Ok(CanDecoder {
            cfg,
            samples_per_bit,
            sample_offset: samples_per_bit * f64::from(cfg.sample_point) / 100.0,
            tracker: LevelTracker::new(recessive_byte),
            dominant: false,
            last_rise: 0,
            last_fall: 0,
            assume_idle: true,
            unsynced_since: None,
            skipped_from: None,
            flag_since: None,
            flag_form_error: None,
            phase: Phase::Idle,
            name: format!("CAN @ {} bit/s", cfg.bitrate),
        })
    }

    fn is_dominant(&self, high: bool) -> bool {
        high == (self.cfg.polarity == Polarity::DominantHigh)
    }

    /// Takes the next edge of the bus line at or before `limit` from known
    /// input, keeping the line state and the last-edge times current.
    /// Transitions of other channels are skipped.
    fn next_edge(&mut self, limit: u64) -> Option<(u64, bool)> {
        loop {
            if self.tracker.next_index()? > limit {
                return None;
            }
            let (sample, before, after) = self.tracker.advance()?;
            let (was, high) = (
                (before >> self.cfg.bit) & 1 == 1,
                (after >> self.cfg.bit) & 1 == 1,
            );
            if was == high {
                continue;
            }
            let dominant = self.is_dominant(high);
            self.dominant = dominant;
            if dominant {
                self.last_fall = sample;
            } else {
                self.last_rise = sample;
                self.assume_idle = false;
            }
            return Some((sample, dominant));
        }
    }

    /// Back to idle after a frame ended (or was abandoned). A dominant line
    /// right now is the start of a pulse that may be an error flag.
    fn go_idle(&mut self, form_error: Option<&'static str>) {
        self.phase = Phase::Idle;
        self.flag_since = self.dominant.then_some(self.last_fall);
        self.flag_form_error = if self.dominant { form_error } else { None };
    }

    /// Once the bus has been idle (or the input ends): if the input opened
    /// on a busy bus, says so over the stretch that was skipped, rather than
    /// leaving it silently blank.
    fn report_skipped(&mut self, out: &mut Vec<Annotation>) {
        if let (Some(_), Some(from)) = (self.unsynced_since.take(), self.skipped_from.take()) {
            out.push(Annotation {
                start_sample: from,
                end_sample: self.last_rise.max(from),
                level: LEVEL_FRAME,
                kind: AnnotationKind::Incomplete,
                text: "bus activity before the first idle: a frame already under way when \
                       the input began, not decoded"
                    .to_string(),
            });
        }
    }

    /// The dominant pulse that began at `since` ended at `end`.
    fn end_of_pulse(&mut self, since: u64, end: u64, out: &mut Vec<Annotation>) {
        let form_error = self.flag_form_error.take();
        if (end - since) as f64 >= ERROR_FLAG_BITS * self.samples_per_bit {
            out.push(Annotation::new(
                since,
                end,
                LEVEL_FRAME,
                AnnotationKind::CanErrorFrame,
            ));
        } else if let Some(message) = form_error {
            out.push(Annotation::new(
                since,
                end,
                LEVEL_FIELD,
                AnnotationKind::Error(message.into()),
            ));
        }
    }
}

impl Decoder for CanDecoder {
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
        let spb = self.samples_per_bit;
        let mut out = Vec::new();
        loop {
            if matches!(self.phase, Phase::Idle) {
                let Some((at, dominant)) = self.next_edge(u64::MAX) else {
                    break;
                };
                if dominant {
                    let bus_idle =
                        self.assume_idle || (at - self.last_rise) as f64 >= IDLE_BITS * spb;
                    if bus_idle {
                        self.assume_idle = false;
                        self.flag_since = None;
                        self.report_skipped(&mut out);
                        self.phase = Phase::Frame(Box::new(FrameState::new(at)));
                    } else {
                        if self.unsynced_since.is_some() {
                            self.skipped_from.get_or_insert(at);
                        }
                        self.flag_since = Some(at);
                        self.flag_form_error = None;
                    }
                } else if let Some(since) = self.flag_since.take() {
                    self.end_of_pulse(since, at, &mut out);
                }
                continue;
            }
            let Phase::Frame(mut frame) = std::mem::replace(&mut self.phase, Phase::Idle) else {
                unreachable!()
            };
            let sample = (frame.bit_start(frame.next_bit, spb) + self.sample_offset).round() as u64;
            // Edges before the sample point first: a recessive→dominant one
            // moves the bit clock, and with it the sample point.
            if let Some((at, dominant)) = self.next_edge(sample) {
                if dominant {
                    let phase = (at as f64 - frame.bit_start(frame.next_bit, spb)) / spb;
                    if (-0.5..0.5).contains(&phase) {
                        frame.origin = at as f64;
                        frame.origin_bit = frame.next_bit;
                    }
                }
                self.phase = Phase::Frame(frame);
                continue;
            }
            if sample > self.tracker.known_until() {
                self.phase = Phase::Frame(frame);
                break;
            }
            let start = frame.bit_start(frame.next_bit, spb);
            let (start, end) = (start.round() as u64, (start + spb).round() as u64);
            frame.next_bit += 1;
            match frame.push_bit(self.dominant, start, end, &mut out) {
                Flow::Continue => self.phase = Phase::Frame(frame),
                Flow::Done => self.go_idle(None),
                Flow::Abort(form_error) => self.go_idle(form_error),
                Flow::Glitch => self.flag_since = None,
            }
        }
        out
    }

    /// A window opened mid-frame must not read the tail of that frame as the
    /// start of another: after seeding, only [`IDLE_BITS`] recessive bits in a
    /// row, counted from the first sample, make a dominant edge a frame start.
    fn seed(&mut self, levels: u8) {
        self.seed_at(0, levels);
    }

    /// Idle is counted from `sample`, where the input begins, not from 0: a
    /// window opened a million samples into a recording has seen no idle
    /// at all yet.
    fn seed_at(&mut self, sample: u64, levels: u8) {
        self.tracker.seed(levels);
        self.dominant = self.is_dominant((levels >> self.cfg.bit) & 1 == 1);
        self.assume_idle = false;
        self.last_rise = sample;
        self.last_fall = sample;
        self.unsynced_since = Some(sample);
        // Dominant at the first sample: a frame (or flag) is already on.
        self.skipped_from = self.dominant.then_some(sample);
        self.flag_since = self.dominant.then_some(sample);
    }

    fn finish(&mut self, end_sample: u64) -> Vec<Annotation> {
        let mut out = Vec::new();
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            // Cut inside EOF: everything the frame carries, ACK included,
            // was already read.
            Phase::Frame(frame) if frame.stage == Stage::Eof => out.push(frame.frame(end_sample)),
            Phase::Frame(frame) => out.push(Annotation {
                start_sample: frame.start,
                end_sample: end_sample.max(frame.start),
                level: LEVEL_FRAME,
                kind: AnnotationKind::Incomplete,
                text: format!("incomplete CAN frame: {} bit(s) decoded", frame.destuffed),
            }),
            Phase::Idle => {
                if let Some(since) = self.flag_since.take() {
                    self.end_of_pulse(since, end_sample.max(since), &mut out);
                }
            }
        }
        self.report_skipped(&mut out);
        self.unsynced_since = None;
        self.flag_since = None;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::assert_all_split_points;

    /// The bits of a classic CAN frame as they appear on the line, SOF
    /// through EOF, `true` being recessive: the fields, CRC-15 and stuff
    /// bits computed here independently of the decoder. `ack` pulls the ACK
    /// slot dominant, as a receiving node would. For a remote frame
    /// `data.len()` is the DLC and no data bytes are sent.
    fn encode_frame(id: u32, extended: bool, remote: bool, data: &[u8], ack: bool) -> Vec<bool> {
        encode_with_dlc(id, extended, remote, data.len() as u8, data, ack)
    }

    /// [`encode_frame`] with a DLC that need not match `data.len()`.
    fn encode_with_dlc(
        id: u32,
        extended: bool,
        remote: bool,
        dlc: u8,
        data: &[u8],
        ack: bool,
    ) -> Vec<bool> {
        fn push(bits: &mut Vec<bool>, value: u32, width: u32) {
            for i in (0..width).rev() {
                bits.push((value >> i) & 1 == 1);
            }
        }
        let mut bits = vec![false]; // SOF
        if extended {
            push(&mut bits, id >> 18, 11);
            bits.push(true); // SRR
            bits.push(true); // IDE
            push(&mut bits, id & 0x3FFFF, 18);
            bits.push(remote);
            push(&mut bits, 0, 2); // r1, r0
        } else {
            push(&mut bits, id, 11);
            bits.push(remote);
            bits.push(false); // IDE
            bits.push(false); // r0
        }
        push(&mut bits, u32::from(dlc), 4);
        if !remote {
            for &byte in data {
                push(&mut bits, u32::from(byte), 8);
            }
        }
        let crc = bits.iter().fold(0, |crc, &b| crc15_step(crc, b));
        push(&mut bits, u32::from(crc), 15);

        let mut line = Vec::new();
        let (mut run_val, mut run) = (false, 0);
        for &b in &bits {
            line.push(b);
            if run > 0 && b == run_val {
                run += 1;
            } else {
                run_val = b;
                run = 1;
            }
            if run == 5 {
                line.push(!b);
                run_val = !b;
                run = 1;
            }
        }
        line.push(true); // CRC delimiter
        line.push(!ack); // ACK slot
        line.push(true); // ACK delimiter
        line.extend([true; 7]); // EOF
        line
    }

    fn idle(bits: usize) -> Vec<bool> {
        vec![true; bits]
    }

    /// Turns line bits (`true` recessive) at `samples_per_bit` into the
    /// transitions the decoder is fed, for `polarity` on channel bit 0.
    fn waveform(bits: &[bool], samples_per_bit: f64, polarity: Polarity) -> Vec<Transition> {
        let level =
            |recessive: bool| -> u8 { u8::from(recessive == (polarity == Polarity::DominantLow)) };
        let mut out = vec![Transition {
            sample_index: 0,
            value: level(true),
        }];
        let mut prev = true;
        for (i, &b) in bits.iter().enumerate() {
            if b != prev {
                out.push(Transition {
                    sample_index: (i as f64 * samples_per_bit).round() as u64,
                    value: level(b),
                });
                prev = b;
            }
        }
        out
    }

    fn config(rate: u64, bitrate: u32, polarity: Polarity) -> CanConfig {
        CanConfig {
            bit: 0,
            sample_rate_hz: rate,
            bitrate,
            sample_point: 75,
            polarity,
        }
    }

    fn decode_with(
        cfg: CanConfig,
        transitions: &[Transition],
        seed: Option<u8>,
        end: u64,
    ) -> Vec<Annotation> {
        let mut decoder = CanDecoder::new(cfg).unwrap();
        if let Some(levels) = seed {
            decoder.seed(levels);
        }
        let mut out = decoder.feed(transitions, end);
        out.extend(decoder.finish(end));
        out
    }

    /// Decodes `bits` as played at `actual` samples per bit by a decoder
    /// configured for `nominal` (so any difference is a bit-rate error).
    fn decode_bits(bits: &[bool], rate: u64, bitrate: u32, actual: f64) -> Vec<Annotation> {
        let transitions = waveform(bits, actual, Polarity::DominantLow);
        let end = (bits.len() as f64 * actual).round() as u64 + 100;
        decode_with(
            config(rate, bitrate, Polarity::DominantLow),
            &transitions,
            None,
            end,
        )
    }

    fn frames(anns: &[Annotation]) -> Vec<&Annotation> {
        anns.iter()
            .filter(|a| matches!(a.kind, AnnotationKind::CanFrame { .. }))
            .collect()
    }

    fn kinds(anns: &[Annotation], level: u8) -> Vec<&AnnotationKind> {
        anns.iter()
            .filter(|a| a.level == level)
            .map(|a| &a.kind)
            .collect()
    }

    const BENCH_DATA: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];

    #[test]
    fn crc_of_the_bench_frame_is_0x0d02() {
        // Decoded by hand from the scope: ID 0x123, DLC 8, DE AD BE EF 01 02 03 04.
        let mut bits = vec![false];
        for i in (0..11).rev() {
            bits.push((0x123u32 >> i) & 1 == 1);
        }
        bits.extend([false, false, false]); // RTR, IDE, r0
        bits.extend([true, false, false, false]); // DLC 8
        for byte in BENCH_DATA {
            for i in (0..8).rev() {
                bits.push((byte >> i) & 1 == 1);
            }
        }
        let crc = bits.iter().fold(0, |crc, &b| crc15_step(crc, b));
        assert_eq!(crc, 0x0D02);
    }

    #[test]
    fn decodes_a_base_frame_at_every_level() {
        let mut bits = idle(12);
        bits.extend(encode_frame(0x123, false, false, &BENCH_DATA, true));
        bits.extend(idle(3));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);

        let frame = frames(&anns);
        assert_eq!(frame.len(), 1, "{anns:#?}");
        assert_eq!(frame[0].text, "0x123 [8] DE AD BE EF 01 02 03 04");
        assert_eq!(frame[0].level, LEVEL_FRAME);
        assert_eq!(
            frame[0].kind,
            AnnotationKind::CanFrame {
                id: 0x123,
                extended: false,
                remote: false,
                dlc: 8,
                data: BENCH_DATA.to_vec(),
                crc_ok: true,
                acked: true,
            }
        );
        // SOF, RTR... in order, then the data bytes, CRC and EOF.
        let fields = kinds(&anns, LEVEL_FIELD);
        assert_eq!(fields[0], &AnnotationKind::Start);
        assert_eq!(
            fields[1],
            &AnnotationKind::CanId {
                id: 0x123,
                extended: false
            }
        );
        assert!(fields.contains(&&AnnotationKind::CanDlc(8)));
        let bytes: Vec<u16> = fields
            .iter()
            .filter_map(|k| match k {
                AnnotationKind::Data(b) => Some(*b),
                _ => None,
            })
            .collect();
        assert_eq!(bytes, BENCH_DATA.map(u16::from));
        assert!(fields.contains(&&AnnotationKind::CanCrc {
            value: 0x0D02,
            ok: true
        }));
        assert!(fields.contains(&&AnnotationKind::CanEof));
        // Stuff bits at bit level, plus the ACK slot.
        let bit_level = kinds(&anns, LEVEL_BIT);
        assert!(bit_level.contains(&&AnnotationKind::StuffBit));
        assert!(bit_level.contains(&&AnnotationKind::Ack));
        // The frame spans SOF to the end of EOF.
        let sof_edge = (12.0f64 * 32.0) as u64;
        assert_eq!(frame[0].start_sample, sof_edge);
        let nbits = encode_frame(0x123, false, false, &BENCH_DATA, true).len() as u64;
        assert_eq!(frame[0].end_sample, sof_edge + nbits * 32);
    }

    #[test]
    fn decodes_an_extended_frame() {
        let mut bits = idle(11);
        bits.extend(encode_frame(0x1ABC_DEF0, true, false, &[0x55, 0xAA], true));
        bits.extend(idle(11));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        let frame = frames(&anns);
        assert_eq!(frame.len(), 1, "{anns:#?}");
        assert_eq!(frame[0].text, "0x1ABCDEF0 ext [2] 55 AA");
        assert!(kinds(&anns, LEVEL_FIELD).contains(&&AnnotationKind::CanId {
            id: 0x1ABC_DEF0,
            extended: true
        }));
        assert!(kinds(&anns, LEVEL_FIELD).contains(&&AnnotationKind::CanIde { extended: true }));
    }

    #[test]
    fn decodes_a_remote_frame_without_data() {
        let mut bits = idle(11);
        bits.extend(encode_frame(0x7FF, false, true, &[0; 4], true));
        bits.extend(idle(11));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        let frame = frames(&anns);
        assert_eq!(frame.len(), 1, "{anns:#?}");
        assert_eq!(frame[0].text, "0x7FF RTR [4]");
        let AnnotationKind::CanFrame { remote, data, .. } = &frame[0].kind else {
            unreachable!()
        };
        assert!(*remote && data.is_empty());
        // And an extended remote frame.
        let mut bits = idle(11);
        bits.extend(encode_frame(0x1000, true, true, &[0; 1], true));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        assert_eq!(frames(&anns)[0].text, "0x00001000 ext RTR [1]");
    }

    #[test]
    fn decodes_dlc_zero_and_dlc_above_eight() {
        let mut bits = idle(11);
        bits.extend(encode_frame(0x001, false, false, &[], true));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        assert_eq!(frames(&anns)[0].text, "0x001 [0]");

        // DLC 12 carries 8 bytes.
        let mut bits = idle(11);
        bits.extend(encode_with_dlc(0x2A5, false, false, 12, &BENCH_DATA, true));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        assert_eq!(
            frames(&anns)[0].text,
            "0x2A5 [12] DE AD BE EF 01 02 03 04",
            "{anns:#?}"
        );
    }

    #[test]
    fn back_to_back_frames_with_a_three_bit_intermission() {
        let mut bits = idle(11);
        for (i, id) in [0x100u32, 0x101, 0x1FF, 0x7FF].into_iter().enumerate() {
            bits.extend(encode_frame(id, false, false, &[i as u8; 3], true));
            bits.extend(idle(3));
        }
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        let ids: Vec<_> = frames(&anns)
            .iter()
            .map(|a| a.text.split(' ').next().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["0x100", "0x101", "0x1FF", "0x7FF"], "{anns:#?}");
    }

    #[test]
    fn decodes_through_bit_rate_error_and_fractional_samples_per_bit() {
        let mut bits = idle(11);
        bits.extend(encode_frame(0x123, false, false, &BENCH_DATA, true));
        bits.extend(idle(3));
        bits.extend(encode_frame(0x1ABC_DEF0, true, false, &BENCH_DATA, false));
        bits.extend(idle(11));
        // 16 MS/s at 125 kbit/s is 128 samples/bit; 12 MS/s at 500k is 24;
        // 16 MS/s at 83.333k is 192; 1 MS/s at 125k is 8. The transmitter
        // runs +1 % slow and -1.5 % fast against what is configured.
        for (rate, bitrate, spb) in [
            (16_000_000u64, 500_000u32, 32.0),
            (16_000_000, 125_000, 128.0),
            (1_000_000, 125_000, 8.0),
            (6_000_000, 500_000, 12.0),
            (16_000_000, 1_000_000, 16.0),
            (7_000_000, 500_000, 14.0),
        ] {
            for error in [1.0, 1.01, 0.985, 0.98, 1.02] {
                // At 8 samples/bit, the whole bit is 8 edges' worth of
                // rounding: 2 % is beyond what that resolution can show.
                if spb < 10.0 && (error - 1.0f64).abs() > 0.015 {
                    continue;
                }
                let anns = decode_bits(&bits, rate, bitrate, spb * error);
                let found = frames(&anns);
                assert_eq!(
                    found.len(),
                    2,
                    "{rate} Hz {bitrate} bit/s error {error}: {anns:#?}"
                );
                assert!(found[0].text.starts_with("0x123 [8] DE AD"));
                assert!(found[1].text.starts_with("0x1ABCDEF0 ext [8]"));
                assert!(found.iter().all(|f| !f.text.contains("CRC error")));
            }
        }
    }

    #[test]
    fn unacknowledged_frame_error_flag_and_retransmission() {
        // The ACK slot stays recessive, the transmitter raises an error flag
        // (six dominant bits) at the ACK delimiter, an error delimiter and the
        // intermission follow, and it sends the frame again.
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, false);
        let tail = 10; // CRC delimiter... EOF: ack slot is at len - 9
        let mut bits = idle(11);
        for _ in 0..2 {
            bits.extend(&frame[..frame.len() - tail + 2]); // through the ACK slot
            bits.extend([false; 6]); // error flag in place of delimiter + EOF
            bits.extend(idle(8 + 3)); // error delimiter, intermission
        }
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        let found = frames(&anns);
        assert_eq!(found.len(), 2, "{anns:#?}");
        for f in &found {
            assert_eq!(f.text, "0x123 [8] DE AD BE EF 01 02 03 04 no ACK");
        }
        let flags = anns
            .iter()
            .filter(|a| a.kind == AnnotationKind::CanErrorFrame)
            .count();
        assert_eq!(flags, 2);
        assert!(!anns
            .iter()
            .any(|a| matches!(a.kind, AnnotationKind::Error(_))));
    }

    #[test]
    fn unacknowledged_frame_with_a_passive_error_flag_is_one_complete_frame() {
        // What the bench shows: an error-passive transmitter's flag is six
        // recessive bits, invisible on the line — the frame just ends with no
        // ACK and a long idle gap.
        let mut bits = idle(11);
        for _ in 0..3 {
            bits.extend(encode_frame(0x123, false, false, &BENCH_DATA, false));
            bits.extend(idle(14 + 3 + 8));
        }
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        let found = frames(&anns);
        assert_eq!(found.len(), 3, "{anns:#?}");
        assert!(found.iter().all(|f| f.text.ends_with("no ACK")));
        assert!(!anns.iter().any(|a| a.kind == AnnotationKind::CanErrorFrame));
    }

    #[test]
    fn a_corrupted_crc_is_reported() {
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        // Flip a data bit well inside the frame (not a stuff-sensitive spot:
        // choose one whose flip does not create a stuff violation).
        let (flipped, anns) = (20..60)
            .find_map(|i| {
                let mut f = frame.clone();
                f[i] = !f[i];
                let mut bits = idle(11);
                bits.extend(&f);
                bits.extend(idle(11));
                let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
                let ok = frames(&anns)
                    .first()
                    .is_some_and(|a| a.text.starts_with("0x123 [8]"));
                ok.then_some((i, anns))
            })
            .expect("some flip keeps the frame well-formed");
        let frame_ann = frames(&anns)[0];
        assert!(
            frame_ann.text.ends_with("CRC error"),
            "bit {flipped}: {}",
            frame_ann.text
        );
        let AnnotationKind::CanFrame { crc_ok, .. } = frame_ann.kind else {
            unreachable!()
        };
        assert!(!crc_ok);
    }

    #[test]
    fn a_stuff_error_aborts_the_frame_and_the_next_one_decodes() {
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        // Six dominant bits in a row right after the ID: a stuff error.
        let mut broken = frame[..14].to_vec();
        broken.extend([false; 8]);
        let mut bits = idle(11);
        bits.extend(&broken);
        bits.extend(idle(8 + 3));
        bits.extend(&frame);
        bits.extend(idle(11));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        let errors: Vec<_> = anns
            .iter()
            .filter(|a| a.level == LEVEL_FRAME)
            .map(|a| a.text.as_str())
            .collect();
        assert_eq!(
            errors,
            [
                "stuff error",
                "error frame",
                "0x123 [8] DE AD BE EF 01 02 03 04"
            ],
            "{anns:#?}"
        );
    }

    /// #32: on CANH of a node that never drives it, the other node's ACK is
    /// the only dominant bit of a frame. It was reported as a stuff error.
    #[test]
    fn a_lone_dominant_bit_is_named_as_such() {
        let mut bits = idle(11);
        bits.push(false);
        bits.extend(idle(30));
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        bits.extend(&frame);
        bits.extend(idle(11));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        let frames: Vec<_> = anns
            .iter()
            .filter(|a| a.level == LEVEL_FRAME)
            .map(|a| a.text.as_str())
            .collect();
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert!(frames[0].starts_with("lone dominant bit"), "{frames:?}");
        assert_eq!(frames[1], "0x123 [8] DE AD BE EF 01 02 03 04");
    }

    #[test]
    fn a_dominant_crc_delimiter_is_a_form_error() {
        let mut frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        let delim = frame.len() - 10;
        frame[delim] = false;
        let mut bits = idle(11);
        bits.extend(&frame);
        bits.extend(idle(11));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        assert!(
            anns.iter()
                .any(|a| a.kind == AnnotationKind::Error("form error: CRC delimiter".into())),
            "{anns:#?}"
        );
        assert!(frames(&anns).is_empty());
    }

    #[test]
    fn a_short_dominant_pulse_in_eof_is_a_form_error_and_the_frame_is_kept() {
        let mut frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        let eof = frame.len() - 7;
        frame[eof + 2] = false;
        let mut bits = idle(11);
        bits.extend(&frame);
        bits.extend(idle(11));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        assert_eq!(frames(&anns).len(), 1);
        let form = anns
            .iter()
            .find(|a| matches!(&a.kind, AnnotationKind::Error(m) if m == "form error: EOF"))
            .unwrap_or_else(|| panic!("{anns:#?}"));
        assert_eq!(form.level, LEVEL_FIELD);
    }

    #[test]
    fn a_window_opened_mid_frame_waits_for_bus_idle() {
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        let mut bits = Vec::new();
        bits.extend(&frame[40..]); // the window starts in the data field
        bits.extend(idle(3));
        bits.extend(encode_frame(0x456, false, false, &[1, 2], true));
        bits.extend(idle(11));
        // The line level at the window's first sample is whatever bit 40 is.
        let transitions: Vec<Transition> = {
            let mut t = waveform(&bits, 32.0, Polarity::DominantLow);
            // `waveform` starts recessive; replace the opening level.
            t[0].value = u8::from(bits[0]);
            t
        };
        let end = bits.len() as u64 * 32 + 100;
        let cfg = config(16_000_000, 500_000, Polarity::DominantLow);
        let seeded = decode_with(cfg, &transitions, Some(u8::from(bits[0])), end);
        let found = frames(&seeded);
        assert_eq!(found.len(), 1, "{seeded:#?}");
        assert!(found[0].text.starts_with("0x456 [2] 01 02"));
        assert!(
            !seeded
                .iter()
                .any(|a| matches!(a.kind, AnnotationKind::Error(_))),
            "{seeded:#?}"
        );
        // The skipped tail is said, not left blank.
        let skipped: Vec<_> = seeded
            .iter()
            .filter(|a| a.kind == AnnotationKind::Incomplete)
            .collect();
        assert_eq!(skipped.len(), 1, "{seeded:#?}");
        assert!(skipped[0]
            .text
            .starts_with("bus activity before the first idle"));
        assert!(skipped[0].end_sample <= found[0].start_sample);
    }

    /// The same window, a million samples into a recording: idle is counted
    /// from where the input begins, not from sample 0.
    #[test]
    fn a_window_opened_mid_frame_far_into_a_recording() {
        const AT: u64 = 1_000_000;
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        let mut bits = Vec::new();
        bits.extend(&frame[40..]);
        bits.extend(idle(3));
        bits.extend(encode_frame(0x456, false, false, &[1, 2], true));
        bits.extend(idle(11));
        let mut transitions = waveform(&bits, 32.0, Polarity::DominantLow);
        transitions[0].value = u8::from(bits[0]);
        for t in &mut transitions {
            t.sample_index += AT;
        }
        let end = AT + bits.len() as u64 * 32 + 100;
        let mut decoder =
            CanDecoder::new(config(16_000_000, 500_000, Polarity::DominantLow)).unwrap();
        decoder.seed_at(AT, u8::from(bits[0]));
        let mut anns = decoder.feed(&transitions, end);
        anns.extend(decoder.finish(end));
        let found = frames(&anns);
        assert_eq!(found.len(), 1, "{anns:#?}");
        assert!(found[0].text.starts_with("0x456 [2] 01 02"));
        assert!(
            !anns
                .iter()
                .any(|a| matches!(a.kind, AnnotationKind::Error(_))),
            "{anns:#?}"
        );
    }

    /// A window opened on a recessive line just before a frame: too little
    /// idle to be sure, so the frame is not decoded — but it is reported.
    #[test]
    fn a_frame_right_after_the_window_start_is_reported_not_dropped() {
        let mut bits = idle(4);
        bits.extend(encode_frame(0x123, false, false, &BENCH_DATA, true));
        bits.extend(idle(11));
        let transitions = waveform(&bits, 32.0, Polarity::DominantLow);
        let end = bits.len() as u64 * 32 + 100;
        let cfg = config(16_000_000, 500_000, Polarity::DominantLow);
        let anns = decode_with(cfg, &transitions, Some(1), end);
        assert!(frames(&anns).is_empty(), "{anns:#?}");
        let skipped = anns
            .iter()
            .find(|a| a.kind == AnnotationKind::Incomplete)
            .unwrap_or_else(|| panic!("{anns:#?}"));
        assert_eq!(skipped.start_sample, 4 * 32);
    }

    #[test]
    fn a_can_fd_frame_is_named_not_misread() {
        // A base frame whose r0 (FDF) is recessive: CAN FD.
        let mut frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        // SOF, 11 ID bits, RTR, IDE, then r0 at index 14 (0x123 has no stuff
        // bit before it: 0 00100100011 0 0).
        frame[14] = true;
        let mut bits = idle(11);
        bits.extend(frame);
        bits.extend(idle(11));
        let anns = decode_bits(&bits, 16_000_000, 500_000, 32.0);
        assert!(frames(&anns).is_empty(), "{anns:#?}");
        assert!(
            anns.iter().any(|a| a.level == LEVEL_FRAME
                && a.kind == AnnotationKind::Error("CAN FD frame (FDF set): not decoded".into())),
            "{anns:#?}"
        );
    }

    #[test]
    fn a_frame_cut_inside_eof_is_still_a_frame() {
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, false);
        let mut bits = idle(11);
        bits.extend(&frame[..frame.len() - 4]); // three of seven EOF bits
        let transitions = waveform(&bits, 32.0, Polarity::DominantLow);
        let end = bits.len() as u64 * 32;
        let anns = decode_with(
            config(16_000_000, 500_000, Polarity::DominantLow),
            &transitions,
            None,
            end,
        );
        let found = frames(&anns);
        assert_eq!(found.len(), 1, "{anns:#?}");
        let AnnotationKind::CanFrame {
            id, acked, crc_ok, ..
        } = &found[0].kind
        else {
            unreachable!()
        };
        assert_eq!((*id, *acked, *crc_ok), (0x123, false, true));
        assert!(!anns.iter().any(|a| a.kind == AnnotationKind::Incomplete));
    }

    #[test]
    fn finish_flushes_an_open_frame_as_incomplete() {
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        let mut bits = idle(11);
        bits.extend(&frame[..50]);
        let transitions = waveform(&bits, 32.0, Polarity::DominantLow);
        let end = bits.len() as u64 * 32;
        let anns = decode_with(
            config(16_000_000, 500_000, Polarity::DominantLow),
            &transitions,
            None,
            end,
        );
        let last = anns.last().unwrap();
        assert_eq!(last.kind, AnnotationKind::Incomplete);
        assert_eq!(last.level, LEVEL_FRAME);
        assert!(
            last.text.starts_with("incomplete CAN frame"),
            "{}",
            last.text
        );
        assert_eq!(last.start_sample, 11 * 32);
    }

    #[test]
    fn dominant_high_polarity_decodes_the_inverted_line() {
        let mut bits = idle(12);
        bits.extend(encode_frame(0x123, false, false, &BENCH_DATA, true));
        bits.extend(idle(11));
        let transitions = waveform(&bits, 32.0, Polarity::DominantHigh);
        let end = bits.len() as u64 * 32;
        let anns = decode_with(
            config(16_000_000, 500_000, Polarity::DominantHigh),
            &transitions,
            None,
            end,
        );
        assert_eq!(frames(&anns)[0].text, "0x123 [8] DE AD BE EF 01 02 03 04");
        // The wrong polarity reads nothing sensible.
        let wrong = decode_with(
            config(16_000_000, 500_000, Polarity::DominantLow),
            &transitions,
            None,
            end,
        );
        assert!(frames(&wrong).is_empty());
    }

    #[test]
    fn other_channels_in_the_packed_byte_are_ignored() {
        let mut bits = idle(12);
        bits.extend(encode_frame(0x123, false, false, &BENCH_DATA, true));
        bits.extend(idle(11));
        let line = waveform(&bits, 32.0, Polarity::DominantLow);
        let last = line.last().unwrap().sample_index;
        // Channel 3 toggles every 50 samples; the CAN line is bit 2.
        let mut times: Vec<u64> = line.iter().map(|t| t.sample_index).collect();
        times.extend((0..=last).step_by(50));
        times.sort_unstable();
        times.dedup();
        let transitions: Vec<Transition> = times
            .iter()
            .map(|&at| {
                let level = line.iter().rev().find(|t| t.sample_index <= at).unwrap();
                let other = ((at / 50) % 2) as u8;
                Transition {
                    sample_index: at,
                    value: (level.value << 2) | (other << 3),
                }
            })
            .collect();
        let mut cfg = config(16_000_000, 500_000, Polarity::DominantLow);
        cfg.bit = 2;
        let anns = decode_with(cfg, &transitions, None, last + 1000);
        assert_eq!(frames(&anns).len(), 1, "{anns:#?}");
    }

    #[test]
    fn config_errors() {
        let ok = config(16_000_000, 500_000, Polarity::DominantLow);
        assert!(CanDecoder::new(ok).is_ok());
        assert_eq!(
            CanDecoder::new(CanConfig { bitrate: 0, ..ok }).unwrap_err(),
            CanConfigError::NoBitrate
        );
        assert_eq!(
            CanDecoder::new(CanConfig {
                sample_point: 95,
                ..ok
            })
            .unwrap_err(),
            CanConfigError::SamplePoint(95)
        );
        assert!(matches!(
            CanDecoder::new(CanConfig {
                sample_rate_hz: 1_000_000,
                ..ok
            })
            .unwrap_err(),
            CanConfigError::BitrateTooFast { .. }
        ));
    }

    #[test]
    fn chunking_never_changes_the_result() {
        let mut bits = idle(11);
        bits.extend(encode_frame(0x123, false, false, &BENCH_DATA, false));
        bits.extend([false; 6]);
        bits.extend(idle(11));
        bits.extend(encode_frame(0x1ABC_DEF0, true, false, &[1, 2, 3], true));
        bits.extend(idle(3));
        bits.extend(encode_frame(0x10, false, true, &[0; 2], true));
        bits.extend(idle(11));
        let transitions = waveform(&bits, 32.0, Polarity::DominantLow);
        let end = bits.len() as u64 * 32;
        let make = || CanDecoder::new(config(16_000_000, 500_000, Polarity::DominantLow)).unwrap();
        assert_all_split_points(&make, &transitions, end);
        // And with a seeded, mid-frame start and a bit-rate error.
        let frame = encode_frame(0x123, false, false, &BENCH_DATA, true);
        let mut bits = frame[30..].to_vec();
        bits.extend(idle(11));
        bits.extend(encode_frame(0x55, false, false, &[9], true));
        bits.extend(idle(11));
        let mut transitions = waveform(&bits, 32.0 * 1.01, Polarity::DominantLow);
        transitions[0].value = u8::from(bits[0]);
        let end = (bits.len() as f64 * 32.0 * 1.01) as u64;
        let make = || {
            let mut d =
                CanDecoder::new(config(16_000_000, 500_000, Polarity::DominantLow)).unwrap();
            d.seed(u8::from(bits[0]));
            d
        };
        assert_all_split_points(&make, &transitions, end);
    }
}
