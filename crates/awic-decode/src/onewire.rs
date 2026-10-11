//! Streaming 1-Wire decoder (SPECIFICATION.md §6.4).
//!
//! 1-Wire has one open-drain line and no clock: everything is timed by the
//! master's low pulses. A low of at least 480 µs is a reset, which slaves
//! answer by pulling the line low again (the presence pulse); every other
//! low is a time slot carrying one bit — short (1–15 µs) for a 1, long
//! (60–120 µs) for a 0. A read slot looks the same on the wire: the master
//! starts it with a short low and a slave stretches it to send a 0. So,
//! like the master itself, the decoder reads a slot's bit at a fixed time
//! after its falling edge (15 µs) and never needs to know who drove it.
//!
//! On top of the bits the decoder follows the ROM layer the way every device
//! does: the first byte after a reset is a ROM command; Read ROM, Match ROM
//! and Overdrive Match ROM are followed by the 64-bit ROM code (family code,
//! 48-bit serial, CRC-8 — checked), Search ROM and Alarm Search by 64
//! triplets (bit, complement, the master's choice) that spell out one ROM
//! code. Everything after that is data bytes, LSB first. Overdrive Skip ROM
//! and Overdrive Match ROM switch the bus to overdrive timing (about ten
//! times faster) until the next reset at standard speed.
//!
//! Before the first reset — a capture that starts mid-transaction — byte
//! boundaries are unknown: slots are counted, not decoded, and reported as
//! one `incomplete` frame-level annotation when the reset comes.

use crate::annotation::{LEVEL_BIT, LEVEL_FIELD, LEVEL_FRAME};
use crate::{
    Annotation, AnnotationKind, ChannelRole, Decoder, DecoderOption, LevelTracker, Transition,
};

/// Bus speed the decoder starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Standard,
    Overdrive,
}

#[derive(Debug, Clone, Copy)]
pub struct OneWireConfig {
    pub bit: u8,
    pub sample_rate_hz: u64,
    /// The speed before any reset is seen — `Overdrive` for a window opened
    /// on a bus already switched. A standard-length reset always returns
    /// to standard speed.
    pub speed: Speed,
}

/// Samples the bit-sampling time must span at least, for a slot's bit to be
/// read reliably.
const MIN_SAMPLES_PER_SAMPLE_TIME: f64 = 4.0;

/// Timing at one speed, in microseconds.
struct TimingUs {
    /// A low shorter than this is a 1 (the master samples here).
    sample: f64,
    /// Nominal slot length, for the width of a bit annotation.
    slot: f64,
    /// Longest low that is still a time slot (twice the longest write-0).
    slot_max: f64,
    /// Shortest low that is a reset (three quarters of the minimum).
    reset_min: f64,
    /// Latest a presence pulse may start after the reset's rising edge.
    presence_wait: f64,
}

const STANDARD: TimingUs = TimingUs {
    sample: 15.0,
    slot: 60.0,
    slot_max: 240.0,
    reset_min: 360.0,
    presence_wait: 120.0,
};

const OVERDRIVE: TimingUs = TimingUs {
    sample: 3.0,
    slot: 6.0,
    slot_max: 32.0,
    reset_min: 36.0,
    presence_wait: 16.0,
};

/// [`TimingUs`] in samples.
#[derive(Debug, Clone, Copy)]
struct Timing {
    sample: u64,
    slot: u64,
    slot_max: u64,
    reset_min: u64,
    presence_wait: u64,
}

impl Timing {
    fn new(us: &TimingUs, rate_hz: u64) -> Self {
        let n = |t: f64| (t * rate_hz as f64 / 1e6).round().max(1.0) as u64;
        Timing {
            sample: n(us.sample),
            slot: n(us.slot),
            slot_max: n(us.slot_max),
            reset_min: n(us.reset_min),
            presence_wait: n(us.presence_wait),
        }
    }
}

fn min_rate_hz(us: &TimingUs) -> u64 {
    (MIN_SAMPLES_PER_SAMPLE_TIME * 1e6 / us.sample).ceil() as u64
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OneWireConfigError {
    #[error(
        "1-Wire at {speed} speed needs at least {min} samples/s (a slot's bit is read \
         {sample_us} µs after its falling edge); got {rate}"
    )]
    RateTooLow {
        speed: &'static str,
        min: u64,
        sample_us: f64,
        rate: u64,
    },
}

const OPTIONS: &[DecoderOption] = &[DecoderOption {
    id: "speed",
    description: "standard (default) | overdrive: the speed before the first reset",
}];

pub const ROM_READ: u8 = 0x33;
pub const ROM_MATCH: u8 = 0x55;
pub const ROM_SKIP: u8 = 0xCC;
pub const ROM_SEARCH: u8 = 0xF0;
pub const ROM_ALARM_SEARCH: u8 = 0xEC;
pub const ROM_RESUME: u8 = 0xA5;
pub const ROM_OVERDRIVE_SKIP: u8 = 0x3C;
pub const ROM_OVERDRIVE_MATCH: u8 = 0x69;

/// The name of a ROM command, `None` for a byte that is none.
pub fn rom_command_name(code: u8) -> Option<&'static str> {
    Some(match code {
        ROM_READ => "Read ROM",
        ROM_MATCH => "Match ROM",
        ROM_SKIP => "Skip ROM",
        ROM_SEARCH => "Search ROM",
        ROM_ALARM_SEARCH => "Alarm Search",
        ROM_RESUME => "Resume",
        ROM_OVERDRIVE_SKIP => "Overdrive Skip ROM",
        ROM_OVERDRIVE_MATCH => "Overdrive Match ROM",
        _ => return None,
    })
}

/// A ROM code the way Linux's w1 subsystem names a device: family code,
/// dash, the 48-bit serial (the CRC byte is left out), e.g. `28-00000a1b2c3d`
/// — upper case here, as every other hex value AWIC prints.
pub fn format_rom(rom: u64) -> String {
    format!("{:02X}-{:012X}", rom & 0xFF, (rom >> 8) & 0xFFFF_FFFF_FFFF)
}

/// Dallas/Maxim CRC-8 (x⁸ + x⁵ + x⁴ + 1, LSB first) over `bytes`.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in bytes {
        let mut b = byte;
        for _ in 0..8 {
            let mix = (crc ^ b) & 1;
            crc >>= 1;
            if mix != 0 {
                crc ^= 0x8C;
            }
            b >>= 1;
        }
    }
    crc
}

/// Whether the last byte of a ROM code is the CRC-8 of the first seven.
pub fn rom_crc_ok(rom: u64) -> bool {
    let bytes = rom.to_le_bytes();
    crc8(&bytes[..7]) == bytes[7]
}

/// What the next bits after a reset mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    RomCommand,
    /// The 64-bit ROM code after Read/Match/Overdrive Match ROM.
    RomCode,
    /// Search ROM triplets: `step` 0 is the bit, 1 its complement, 2 the
    /// direction the master writes (the ROM bit).
    Search {
        step: u8,
        id_bit: bool,
    },
    Data,
    /// A search no device answered: bits until the next reset carry nothing.
    Lost,
}

/// Bits being gathered into a byte or a ROM code.
#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    value: u64,
    bits: u8,
    start: u64,
}

impl Acc {
    fn push(&mut self, bit: bool, start: u64) {
        if self.bits == 0 {
            self.start = start;
        }
        if bit {
            self.value |= 1 << self.bits;
        }
        self.bits += 1;
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingReset {
    start: u64,
    end: u64,
    overdrive: bool,
}

/// Streaming 1-Wire decoder over one line (SPEC §6.4).
#[derive(Debug)]
pub struct OneWireDecoder {
    bit: u8,
    rate_hz: u64,
    standard: Timing,
    overdrive_timing: Timing,
    overdrive: bool,
    tracker: LevelTracker,
    /// The line went low here and has not come back up.
    low_since: Option<u64>,
    /// A reset whose presence pulse is still awaited.
    pending_reset: Option<PendingReset>,
    /// A reset has been seen since the last loss of sync.
    synced: bool,
    /// Slots counted while not synced: `(count, first start, last end)`.
    skipped: Option<(u64, u64, u64)>,
    phase: Phase,
    acc: Acc,
    channels: [ChannelRole; 1],
}

impl OneWireDecoder {
    pub fn new(cfg: OneWireConfig) -> Result<Self, OneWireConfigError> {
        let (us, speed) = match cfg.speed {
            Speed::Standard => (&STANDARD, "standard"),
            Speed::Overdrive => (&OVERDRIVE, "overdrive"),
        };
        let min = min_rate_hz(us);
        if cfg.sample_rate_hz < min {
            return Err(OneWireConfigError::RateTooLow {
                speed,
                min,
                sample_us: us.sample,
                rate: cfg.sample_rate_hz,
            });
        }
        Ok(OneWireDecoder {
            bit: cfg.bit,
            rate_hz: cfg.sample_rate_hz,
            standard: Timing::new(&STANDARD, cfg.sample_rate_hz),
            overdrive_timing: Timing::new(&OVERDRIVE, cfg.sample_rate_hz),
            overdrive: cfg.speed == Speed::Overdrive,
            // Open drain: the line idles high.
            tracker: LevelTracker::new(1 << cfg.bit),
            low_since: None,
            pending_reset: None,
            synced: false,
            skipped: None,
            phase: Phase::RomCommand,
            acc: Acc::default(),
            channels: [ChannelRole::Owr],
        })
    }

    fn timing(&self) -> Timing {
        if self.overdrive {
            self.overdrive_timing
        } else {
            self.standard
        }
    }

    fn emit_reset(&mut self, r: PendingReset, presence: bool, end: u64, out: &mut Vec<Annotation>) {
        out.push(Annotation::new(
            r.start,
            end.max(r.end),
            LEVEL_FRAME,
            AnnotationKind::OneWireReset {
                presence,
                overdrive: r.overdrive,
            },
        ));
    }

    /// No presence pulse came in time: the reset stands alone.
    fn resolve_absent_presence(&mut self, now: u64, out: &mut Vec<Annotation>) {
        let Some(r) = self.pending_reset else { return };
        let wait = if r.overdrive {
            self.overdrive_timing.presence_wait
        } else {
            self.standard.presence_wait
        };
        if now > r.end + wait {
            self.pending_reset = None;
            self.emit_reset(r, false, r.end, out);
        }
    }

    fn on_fall(&mut self, at: u64, out: &mut Vec<Annotation>) {
        self.resolve_absent_presence(at, out);
        self.low_since = Some(at);
    }

    fn on_rise(&mut self, at: u64, out: &mut Vec<Annotation>) {
        // Seeded low, the first pulse began before the input did: no slot.
        let Some(fall) = self.low_since.take() else {
            return;
        };
        if let Some(r) = self.pending_reset.take() {
            // The fall came within the presence window (else it was resolved
            // as absent when it happened).
            self.emit_reset(r, true, at, out);
            return;
        }
        let len = at - fall;
        let t = self.timing();
        if len >= self.standard.reset_min || (self.overdrive && len >= t.reset_min) {
            self.on_reset(fall, at, out);
        } else if len > t.slot_max {
            let us = len as f64 * 1e6 / self.rate_hz as f64;
            let speed = if self.overdrive {
                "overdrive"
            } else {
                "standard"
            };
            // Where the next byte starts is unknown until the next reset.
            self.flush_partial(fall, out);
            self.synced = false;
            out.push(Annotation::new(
                fall,
                at,
                LEVEL_FRAME,
                AnnotationKind::Error(format!(
                    "low for {us:.0} µs: too long for a {speed} time slot, too short for a reset"
                )),
            ));
        } else {
            let end = at.max(fall + t.slot);
            self.on_slot(len < t.sample, fall, end, out);
        }
    }

    fn on_reset(&mut self, start: u64, end: u64, out: &mut Vec<Annotation>) {
        // A partial byte or ROM code cut by the reset.
        self.flush_partial(start, out);
        if let Some((count, first, last)) = self.skipped.take() {
            let mut a = Annotation::new(first, last, LEVEL_FRAME, AnnotationKind::Incomplete);
            a.text = format!(
                "{count} time slot{} before this reset skipped (byte boundaries unknown)",
                if count == 1 { "" } else { "s" }
            );
            out.push(a);
        }
        // A reset at standard length puts every device back to standard speed.
        let len = end - start;
        if len >= self.standard.reset_min {
            self.overdrive = false;
        }
        self.synced = true;
        self.phase = Phase::RomCommand;
        self.acc = Acc::default();
        self.pending_reset = Some(PendingReset {
            start,
            end,
            overdrive: self.overdrive,
        });
    }

    fn flush_partial(&mut self, end: u64, out: &mut Vec<Annotation>) {
        if self.acc.bits == 0 || !self.synced {
            self.acc = Acc::default();
            return;
        }
        let total = match self.phase {
            Phase::RomCode | Phase::Search { .. } => 64,
            _ => 8,
        };
        let mut a = Annotation::new(
            self.acc.start,
            end.max(self.acc.start),
            LEVEL_FIELD,
            AnnotationKind::Incomplete,
        );
        a.text = format!("incomplete: {} of {total} bits", self.acc.bits);
        out.push(a);
        self.acc = Acc::default();
    }

    fn on_slot(&mut self, bit: bool, start: u64, end: u64, out: &mut Vec<Annotation>) {
        if !self.synced {
            let s = self.skipped.get_or_insert((0, start, end));
            s.0 += 1;
            s.2 = end;
            return;
        }
        out.push(Annotation::new(
            start,
            end,
            LEVEL_BIT,
            AnnotationKind::OneWireBit(bit),
        ));
        match self.phase {
            Phase::RomCommand => {
                self.acc.push(bit, start);
                if self.acc.bits == 8 {
                    let code = self.acc.value as u8;
                    out.push(Annotation::new(
                        self.acc.start,
                        end,
                        LEVEL_FIELD,
                        AnnotationKind::OneWireRomCommand(code),
                    ));
                    self.acc = Acc::default();
                    self.phase = match code {
                        ROM_READ | ROM_MATCH | ROM_OVERDRIVE_MATCH => Phase::RomCode,
                        ROM_SEARCH | ROM_ALARM_SEARCH => Phase::Search {
                            step: 0,
                            id_bit: false,
                        },
                        _ => Phase::Data,
                    };
                    if matches!(code, ROM_OVERDRIVE_SKIP | ROM_OVERDRIVE_MATCH) {
                        self.enter_overdrive(end, out);
                    }
                }
            }
            Phase::RomCode => {
                self.acc.push(bit, start);
                if self.acc.bits == 64 {
                    self.emit_rom(end, out);
                }
            }
            Phase::Search { step: 0, .. } => {
                self.phase = Phase::Search {
                    step: 1,
                    id_bit: bit,
                };
            }
            Phase::Search { step: 1, id_bit } => {
                if id_bit && bit {
                    // Both the bit and its complement read 1: nobody is
                    // pulling the line, so no device took part.
                    out.push(Annotation::new(
                        start,
                        end,
                        LEVEL_FIELD,
                        AnnotationKind::Error(format!(
                            "search: no device answered at ROM bit {}",
                            self.acc.bits
                        )),
                    ));
                    self.acc = Acc::default();
                    self.phase = Phase::Lost;
                } else {
                    self.phase = Phase::Search { step: 2, id_bit };
                }
            }
            Phase::Search { .. } => {
                self.acc.push(bit, start);
                if self.acc.bits == 64 {
                    self.emit_rom(end, out);
                } else {
                    self.phase = Phase::Search {
                        step: 0,
                        id_bit: false,
                    };
                }
            }
            Phase::Data => {
                self.acc.push(bit, start);
                if self.acc.bits == 8 {
                    out.push(Annotation::new(
                        self.acc.start,
                        end,
                        LEVEL_FIELD,
                        AnnotationKind::Data(self.acc.value as u16),
                    ));
                    self.acc = Acc::default();
                }
            }
            Phase::Lost => {}
        }
    }

    fn emit_rom(&mut self, end: u64, out: &mut Vec<Annotation>) {
        let rom = self.acc.value;
        out.push(Annotation::new(
            self.acc.start,
            end,
            LEVEL_FIELD,
            AnnotationKind::OneWireRom {
                rom,
                crc_ok: rom_crc_ok(rom),
            },
        ));
        self.acc = Acc::default();
        self.phase = Phase::Data;
    }

    fn enter_overdrive(&mut self, at: u64, out: &mut Vec<Annotation>) {
        if self.overdrive {
            return;
        }
        self.overdrive = true;
        let min = min_rate_hz(&OVERDRIVE);
        if self.rate_hz < min {
            out.push(Annotation::new(
                at,
                at,
                LEVEL_FRAME,
                AnnotationKind::Error(format!(
                    "the bus switches to overdrive, which needs at least {min} samples/s; \
                     bits until the next standard reset are unreliable at {}",
                    self.rate_hz
                )),
            ));
        }
    }
}

impl Decoder for OneWireDecoder {
    fn name(&self) -> &str {
        "1-Wire"
    }

    fn required_channels(&self) -> &[ChannelRole] {
        &self.channels
    }

    fn options(&self) -> &[DecoderOption] {
        OPTIONS
    }

    fn feed(&mut self, transitions: &[Transition], valid_until: u64) -> Vec<Annotation> {
        self.tracker.push(transitions, valid_until);
        let mask = 1u8 << self.bit;
        let mut out = Vec::new();
        while let Some((sample, _, after)) = self.tracker.next_relevant(mask) {
            if (after >> self.bit) & 1 == 0 {
                self.on_fall(sample, &mut out);
            } else {
                self.on_rise(sample, &mut out);
            }
        }
        if self.low_since.is_none() {
            let known = self.tracker.known_until();
            self.resolve_absent_presence(known, &mut out);
        }
        out
    }

    fn seed(&mut self, levels: u8) {
        self.tracker.seed(levels);
    }

    fn finish(&mut self, end_sample: u64) -> Vec<Annotation> {
        let mut out = Vec::new();
        if let Some(r) = self.pending_reset.take() {
            // A presence pulse that began in time but outlasts the input
            // still counts.
            let presence = self.low_since.is_some();
            self.emit_reset(
                r,
                presence,
                if presence { end_sample } else { r.end },
                &mut out,
            );
        } else {
            self.flush_partial(end_sample, &mut out);
        }
        if let Some((count, first, last)) = self.skipped.take() {
            let mut a = Annotation::new(first, last, LEVEL_FRAME, AnnotationKind::Incomplete);
            a.text = format!(
                "{count} time slot{} and no reset after them (byte boundaries unknown)",
                if count == 1 { "" } else { "s" }
            );
            out.push(a);
        }
        self.low_since = None;
        self.synced = false;
        self.phase = Phase::RomCommand;
        self.acc = Acc::default();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::render_onewire;
    use crate::test_util::{assert_all_split_points, assert_same_result, build_plan};
    use crate::transactions::{GroupProtocol, TransactionGrouper};
    use proptest::proptest;

    /// Samples per µs: overdrive needs more than one.
    const SCALE: u64 = 2;
    const RATE: u64 = SCALE * 1_000_000;
    const BIT: u8 = 2;

    /// A 1-Wire bus at standard or overdrive timing; `t` in µs.
    struct Bus {
        t: u64,
        low: bool,
        transitions: Vec<Transition>,
        overdrive: bool,
    }

    impl Bus {
        fn new() -> Self {
            Bus {
                t: 100,
                low: false,
                transitions: Vec::new(),
                overdrive: false,
            }
        }

        fn level(&mut self, low: bool) {
            if low != self.low {
                self.low = low;
                self.transitions.push(Transition {
                    sample_index: self.t * SCALE,
                    value: if low { 0 } else { 1 << BIT },
                });
            }
        }

        fn pulse(&mut self, low_us: u64, high_us: u64) {
            self.level(true);
            self.t += low_us;
            self.level(false);
            self.t += high_us;
        }

        /// Reset with or without a presence pulse.
        fn reset(&mut self, presence: bool) {
            if self.overdrive {
                self.pulse(60, 3);
                if presence {
                    self.pulse(10, 50);
                } else {
                    self.t += 60;
                }
            } else {
                self.pulse(480, 30);
                if presence {
                    self.pulse(120, 330);
                } else {
                    self.t += 450;
                }
            }
        }

        fn standard_reset(&mut self, presence: bool) {
            self.overdrive = false;
            self.reset(presence);
        }

        fn bit(&mut self, one: bool) {
            match (self.overdrive, one) {
                (false, true) => self.pulse(6, 64),
                (false, false) => self.pulse(60, 10),
                (true, true) => self.pulse(1, 9),
                (true, false) => self.pulse(8, 2),
            }
        }

        /// A read slot a slave answers with `one`: a short master low,
        /// stretched by the slave for a 0.
        fn read(&mut self, one: bool) {
            if self.overdrive {
                self.pulse(if one { 1 } else { 5 }, if one { 9 } else { 5 });
            } else {
                self.pulse(if one { 3 } else { 30 }, if one { 67 } else { 40 });
            }
        }

        fn byte(&mut self, b: u8) {
            for i in 0..8 {
                self.bit((b >> i) & 1 == 1);
            }
        }

        fn rom(&mut self, rom: u64) {
            for i in 0..64 {
                self.bit((rom >> i) & 1 == 1);
            }
        }

        fn end(&self) -> u64 {
            (self.t + 1000) * SCALE
        }
    }

    fn make() -> OneWireDecoder {
        OneWireDecoder::new(OneWireConfig {
            bit: BIT,
            sample_rate_hz: RATE,
            speed: Speed::Standard,
        })
        .unwrap()
    }

    fn decode(bus: &Bus) -> Vec<Annotation> {
        let mut d = make();
        let mut out = d.feed(&bus.transitions, bus.end());
        out.extend(d.finish(bus.end()));
        out
    }

    fn fields(anns: &[Annotation]) -> Vec<String> {
        anns.iter()
            .filter(|a| a.level <= LEVEL_FIELD)
            .map(|a| a.text.clone())
            .collect()
    }

    /// A DS18B20's ROM code: family 0x28, serial, CRC.
    fn ds18b20() -> u64 {
        let body: u64 = 0x0000_000A_1B2C_3D28;
        let crc = crc8(&body.to_le_bytes()[..7]);
        body | (u64::from(crc) << 56)
    }

    #[test]
    fn crc8_matches_the_maxim_check_value() {
        // Maxim AN27's example ROM: 02 1C B8 01 00 00 00 A2.
        assert_eq!(crc8(&[0x02, 0x1C, 0xB8, 0x01, 0x00, 0x00, 0x00]), 0xA2);
        assert!(rom_crc_ok(0xA200_0000_01B8_1C02));
        assert!(!rom_crc_ok(0xA300_0000_01B8_1C02));
    }

    #[test]
    fn match_rom_then_a_function_command() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_MATCH);
        bus.rom(ds18b20());
        bus.byte(0x44); // Convert T
        bus.reset(true);
        bus.byte(ROM_SKIP);
        bus.byte(0xBE); // Read Scratchpad
        for b in [0x50, 0x05] {
            for i in 0..8 {
                bus.read((b >> i) & 1 == 1);
            }
        }
        let anns = decode(&bus);
        assert_eq!(
            fields(&anns),
            [
                "reset",
                "Match ROM",
                "ROM 28-00000A1B2C3D",
                "0x44",
                "reset",
                "Skip ROM",
                "0xBE",
                "0x50",
                "0x05",
            ]
        );
        // Bits are LEVEL_BIT, one per slot.
        let bits = anns.iter().filter(|a| a.level == LEVEL_BIT).count();
        assert_eq!(bits, 8 + 64 + 8 + 8 + 8 + 16);
        assert_eq!(
            render_onewire(&anns),
            [
                "reset  Match ROM  28-00000A1B2C3D  44",
                "reset  Skip ROM  BE  50  05",
            ]
        );
    }

    #[test]
    fn a_reset_nobody_answers_says_so() {
        let mut bus = Bus::new();
        bus.reset(false);
        bus.reset(true);
        let anns = decode(&bus);
        assert_eq!(fields(&anns), ["reset, no presence", "reset"]);
    }

    #[test]
    fn a_bad_rom_crc_is_flagged() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_READ);
        bus.rom(ds18b20() ^ (1 << 60));
        let anns = decode(&bus);
        assert!(
            fields(&anns).contains(&"ROM 28-00000A1B2C3D CRC error".to_string()),
            "{:?}",
            fields(&anns)
        );
    }

    #[test]
    fn search_rom_spells_out_the_rom_from_the_triplets() {
        let rom = ds18b20();
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_SEARCH);
        for i in 0..64 {
            let b = (rom >> i) & 1 == 1;
            bus.read(b);
            bus.read(!b);
            bus.bit(b);
        }
        let anns = decode(&bus);
        assert_eq!(
            fields(&anns),
            ["reset", "Search ROM", "ROM 28-00000A1B2C3D"]
        );
    }

    #[test]
    fn a_search_nobody_answers_is_an_error_not_a_rom() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_SEARCH);
        bus.read(true);
        bus.read(true);
        bus.bit(true);
        let anns = decode(&bus);
        assert_eq!(
            fields(&anns),
            [
                "reset",
                "Search ROM",
                "search: no device answered at ROM bit 0"
            ]
        );
    }

    #[test]
    fn overdrive_skip_switches_timing_until_a_standard_reset() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_OVERDRIVE_SKIP);
        bus.overdrive = true;
        bus.reset(true);
        bus.byte(ROM_SKIP);
        bus.byte(0xA5);
        bus.standard_reset(true);
        bus.byte(ROM_SKIP);
        bus.byte(0x0F);
        let anns = decode(&bus);
        assert_eq!(
            fields(&anns),
            [
                "reset",
                "Overdrive Skip ROM",
                "reset (overdrive)",
                "Skip ROM",
                "0xA5",
                "reset",
                "Skip ROM",
                "0x0F",
            ]
        );
    }

    #[test]
    fn overdrive_below_its_rate_is_flagged() {
        // 500 kS/s times standard speed (7.5 samples in 15 µs) but not
        // overdrive's 3 µs.
        let mut d = OneWireDecoder::new(OneWireConfig {
            bit: BIT,
            sample_rate_hz: 500_000,
            speed: Speed::Standard,
        })
        .unwrap();
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_OVERDRIVE_SKIP);
        let scaled: Vec<Transition> = bus
            .transitions
            .iter()
            .map(|t| Transition {
                sample_index: t.sample_index / (2 * SCALE),
                value: t.value,
            })
            .collect();
        let end = bus.end() / (2 * SCALE);
        let mut anns = d.feed(&scaled, end);
        anns.extend(d.finish(end));
        assert!(
            anns.iter()
                .any(|a| a.text.starts_with("the bus switches to overdrive")),
            "{:?}",
            fields(&anns)
        );
    }

    #[test]
    fn too_slow_a_sample_rate_is_refused() {
        let err = OneWireDecoder::new(OneWireConfig {
            bit: 0,
            sample_rate_hz: 100_000,
            speed: Speed::Standard,
        })
        .unwrap_err();
        assert!(err.to_string().contains("266667"), "{err}");
        assert!(OneWireDecoder::new(OneWireConfig {
            bit: 0,
            sample_rate_hz: 1_000_000,
            speed: Speed::Overdrive,
        })
        .is_err());
    }

    #[test]
    fn slots_before_the_first_reset_are_counted_not_decoded() {
        let mut bus = Bus::new();
        bus.byte(0x12);
        bus.bit(true);
        bus.reset(true);
        bus.byte(ROM_SKIP);
        let anns = decode(&bus);
        assert_eq!(
            fields(&anns),
            [
                "9 time slots before this reset skipped (byte boundaries unknown)",
                "reset",
                "Skip ROM",
            ]
        );
    }

    #[test]
    fn a_window_opened_inside_a_low_pulse_skips_that_pulse() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_SKIP);
        // Open the window 200 µs into the first (reset) pulse.
        let start = bus.transitions[0].sample_index + 200 * SCALE;
        let rest: Vec<Transition> = bus
            .transitions
            .iter()
            .copied()
            .filter(|t| t.sample_index > start)
            .collect();
        let mut d = make();
        d.seed(0);
        let mut anns = d.feed(&rest, bus.end());
        anns.extend(d.finish(bus.end()));
        // The reset's own rise is no slot; the presence pulse is a 0 slot
        // nobody can place in a byte, then the Skip ROM's 8 slots.
        assert_eq!(
            fields(&anns),
            ["9 time slots and no reset after them (byte boundaries unknown)"]
        );
    }

    #[test]
    fn an_impossible_pulse_is_an_error_and_drops_the_partial_byte() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_SKIP);
        bus.bit(true);
        bus.bit(false);
        bus.pulse(300, 50); // between a slot and a reset
        bus.byte(0x55); // unaligned: skipped
        bus.reset(true);
        let anns = decode(&bus);
        assert_eq!(
            fields(&anns),
            [
                "reset",
                "Skip ROM",
                "incomplete: 2 of 8 bits",
                "low for 300 µs: too long for a standard time slot, too short for a reset",
                "8 time slots before this reset skipped (byte boundaries unknown)",
                "reset",
            ]
        );
    }

    #[test]
    fn finish_flushes_a_partial_byte_and_a_pending_reset() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_SKIP);
        bus.bit(true);
        let anns = decode(&bus);
        assert_eq!(
            fields(&anns),
            ["reset", "Skip ROM", "incomplete: 1 of 8 bits"]
        );

        // Input ending right after a reset: no presence window seen yet.
        let mut bus = Bus::new();
        bus.pulse(480, 5);
        let mut d = make();
        let end = bus.t * SCALE;
        let mut anns = d.feed(&bus.transitions, end);
        anns.extend(d.finish(end));
        assert_eq!(fields(&anns), ["reset, no presence"]);
    }

    #[test]
    fn transactions_one_per_reset_with_presence_and_family() {
        let mut bus = Bus::new();
        bus.reset(true);
        bus.byte(ROM_MATCH);
        bus.rom(ds18b20());
        bus.byte(0x44);
        bus.reset(false);
        let anns = decode(&bus);
        let mut g = TransactionGrouper::new(GroupProtocol::OneWire);
        let mut ts = g.push(&anns);
        ts.extend(g.finish());
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].presence, Some(true));
        assert_eq!(ts[0].address, Some(0x28));
        assert_eq!(ts[0].data, [0x44]);
        assert_eq!(ts[0].notes, ["Match ROM", "ROM 28-00000A1B2C3D"]);
        assert_eq!(ts[1].presence, Some(false));
        assert_eq!(ts[1].nacks, 1);
        assert_eq!(ts[1].notes, ["no presence pulse"]);
    }

    fn sample_bus() -> Bus {
        let mut bus = Bus::new();
        bus.bit(true);
        bus.reset(true);
        bus.byte(ROM_MATCH);
        bus.rom(ds18b20());
        bus.byte(0xBE);
        bus.read(false);
        bus.read(true);
        bus.reset(false);
        bus.reset(true);
        bus.byte(ROM_OVERDRIVE_SKIP);
        bus.overdrive = true;
        bus.reset(true);
        bus.byte(0x33);
        bus
    }

    #[test]
    fn chunking_at_every_split_point_gives_the_same_result() {
        let bus = sample_bus();
        assert_all_split_points(&make, &bus.transitions, bus.end());
    }

    #[test]
    fn chunking_with_lagging_valid_until_gives_the_same_result() {
        let bus = sample_bus();
        let transitions = bus.transitions.clone();
        let end = bus.end();
        proptest!(|(
            cuts in proptest::collection::vec(0usize..transitions.len(), 0..8),
            slacks in proptest::collection::vec(0u64..1200, 0..8)
        )| {
            let plan = build_plan(&transitions, end, &cuts, &slacks);
            assert_same_result(&make, &transitions, end, &plan);
        });
    }
}
