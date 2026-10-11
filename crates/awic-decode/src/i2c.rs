//! Streaming I²C decoder (SPEC §6).
//!
//! Purely edge-driven — no baud/rate to configure — because I²C carries its
//! own clock: a bit is whatever SDA reads on SCL's rising edge, and
//! START/STOP are SDA moving while SCL is high. That single rule is also
//! why SDA changing while SCL is low needs no special case here: such a
//! change is simply not "while SCL is high", so it never matches the
//! START/STOP test.

use crate::annotation::{LEVEL_BIT, LEVEL_FIELD, LEVEL_FRAME};
use crate::{
    Annotation, AnnotationKind, ChannelRole, Decoder, DecoderOption, LevelTracker, Transition,
};

/// I²C construction parameters (SPEC §6): which packed-byte bits carry SCL
/// and SDA. No rate — decoding follows the clock edges themselves.
#[derive(Debug, Clone, Copy)]
pub struct I2cConfig {
    pub scl: u8,
    pub sda: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum I2cConfigError {
    #[error("SCL and SDA must be different bits, both given as bit {0}")]
    SameBit(u8),
}

const OPTIONS: &[DecoderOption] = &[DecoderOption {
    id: "address_width",
    description: "7-bit addressing (default); 10-bit addresses are recognised automatically \
                  from the reserved 0b11110xxx prefix",
}];

const CHANNELS: [ChannelRole; 2] = [ChannelRole::Scl, ChannelRole::Sda];

/// The reserved 7-bit-address-field prefix (top 5 bits) that marks 10-bit
/// addressing (I²C spec UM10204 §3.1.11).
const TEN_BIT_PREFIX: u16 = 0b1_1110;

#[derive(Debug, Clone, Copy)]
enum FieldKind {
    /// The next byte is a 7-bit-address-plus-R/W field, or the first byte of
    /// a 10-bit address.
    Address,
    /// The next byte is the low 8 bits of a 10-bit address, following a
    /// first byte that matched [`TEN_BIT_PREFIX`] with W. `address_high` is
    /// that first byte's top 2 address bits; `field_start` is where the
    /// combined address annotation should start.
    TenBitLow {
        address_high: u16,
        field_start: u64,
    },
    Data,
}

#[derive(Debug, Clone)]
enum Slot {
    Byte {
        kind: FieldKind,
        bits: Vec<bool>,
        start: u64,
    },
    /// Awaiting the ACK/NACK bit that follows every clocked byte.
    Ack,
}

fn bits_to_byte(bits: &[bool]) -> u8 {
    bits.iter().fold(0u8, |acc, &b| (acc << 1) | b as u8)
}

/// Streaming I²C decoder over SCL/SDA (SPEC §6).
#[derive(Debug)]
pub struct I2cDecoder {
    scl_bit: u8,
    sda_bit: u8,
    tracker: LevelTracker,
    /// Set on START/repeated-START, cleared on STOP. A START seen while
    /// this is already true is a repeated start rather than a bus error
    /// (SPEC §6: "a missing STOP before the next START is a repeated
    /// start").
    open_transaction: bool,
    slot: Option<Slot>,
    next_kind: FieldKind,
    /// The full 10-bit address once known, kept for a repeated-start read
    /// continuation (which retransmits only the high byte with R=1).
    ten_bit_addr: Option<u16>,
}

impl I2cDecoder {
    pub fn new(cfg: I2cConfig) -> Result<Self, I2cConfigError> {
        if cfg.scl == cfg.sda {
            return Err(I2cConfigError::SameBit(cfg.scl));
        }
        let idle = (1u8 << cfg.scl) | (1u8 << cfg.sda);
        Ok(I2cDecoder {
            scl_bit: cfg.scl,
            sda_bit: cfg.sda,
            tracker: LevelTracker::new(idle),
            open_transaction: false,
            slot: None,
            next_kind: FieldKind::Address,
            ten_bit_addr: None,
        })
    }

    fn on_start(&mut self, sample: u64, out: &mut Vec<Annotation>) {
        let kind = if self.open_transaction {
            AnnotationKind::RepeatedStart
        } else {
            AnnotationKind::Start
        };
        out.push(Annotation::new(sample, sample, LEVEL_FRAME, kind));
        self.open_transaction = true;
        self.slot = None;
        self.next_kind = FieldKind::Address;
    }

    fn on_stop(&mut self, sample: u64, out: &mut Vec<Annotation>) {
        out.push(Annotation::new(
            sample,
            sample,
            LEVEL_FRAME,
            AnnotationKind::Stop,
        ));
        self.open_transaction = false;
        self.slot = None;
        self.ten_bit_addr = None;
    }

    /// A byte (address or data) just finished clocking in at `sample`
    /// (its 8th bit's SCL rising edge). Emits the field annotation (or
    /// defers it, for a 10-bit address's first byte) and sets `next_kind`
    /// for whatever follows the ack.
    fn finish_byte(
        &mut self,
        kind: FieldKind,
        byte: u8,
        start: u64,
        sample: u64,
        out: &mut Vec<Annotation>,
    ) {
        match kind {
            FieldKind::Address => {
                let addr7 = u16::from(byte >> 1);
                let read = byte & 1 == 1;
                if addr7 >> 2 == TEN_BIT_PREFIX {
                    if read {
                        // Repeated-start read continuation: the low byte
                        // isn't retransmitted, so fall back to the address
                        // we cached from the earlier write phase if we have
                        // it (nothing to combine otherwise).
                        let address = self.ten_bit_addr.unwrap_or(addr7);
                        out.push(Annotation::new(
                            start,
                            sample,
                            LEVEL_FIELD,
                            AnnotationKind::Address {
                                address,
                                read: true,
                            },
                        ));
                        self.next_kind = FieldKind::Data;
                    } else {
                        self.next_kind = FieldKind::TenBitLow {
                            address_high: addr7 & 0x3,
                            field_start: start,
                        };
                    }
                } else {
                    out.push(Annotation::new(
                        start,
                        sample,
                        LEVEL_FIELD,
                        AnnotationKind::Address {
                            address: addr7,
                            read,
                        },
                    ));
                    self.next_kind = FieldKind::Data;
                }
            }
            FieldKind::TenBitLow {
                address_high,
                field_start,
            } => {
                let address = (address_high << 8) | u16::from(byte);
                self.ten_bit_addr = Some(address);
                out.push(Annotation::new(
                    field_start,
                    sample,
                    LEVEL_FIELD,
                    AnnotationKind::Address {
                        address,
                        read: false,
                    },
                ));
                self.next_kind = FieldKind::Data;
            }
            FieldKind::Data => {
                out.push(Annotation::new(
                    start,
                    sample,
                    LEVEL_FIELD,
                    AnnotationKind::Data(u16::from(byte)),
                ));
                self.next_kind = FieldKind::Data;
            }
        }
    }

    fn on_scl_rising(&mut self, sample: u64, sda: bool, out: &mut Vec<Annotation>) {
        let slot = self.slot.take();
        self.slot = match slot {
            None => Some(Slot::Byte {
                kind: self.next_kind,
                bits: vec![sda],
                start: sample,
            }),
            Some(Slot::Byte {
                kind,
                mut bits,
                start,
            }) => {
                bits.push(sda);
                if bits.len() == 8 {
                    let byte = bits_to_byte(&bits);
                    self.finish_byte(kind, byte, start, sample, out);
                    Some(Slot::Ack)
                } else {
                    Some(Slot::Byte { kind, bits, start })
                }
            }
            Some(Slot::Ack) => {
                // SDA low = held down = ACK; high = released = NACK.
                let kind = if sda {
                    AnnotationKind::Nack
                } else {
                    AnnotationKind::Ack
                };
                out.push(Annotation::new(sample, sample, LEVEL_BIT, kind));
                None
            }
        };
    }
}

impl Decoder for I2cDecoder {
    fn name(&self) -> &str {
        "I2C"
    }

    fn required_channels(&self) -> &[ChannelRole] {
        &CHANNELS
    }

    fn options(&self) -> &[DecoderOption] {
        OPTIONS
    }

    fn feed(&mut self, transitions: &[Transition], valid_until: u64) -> Vec<Annotation> {
        self.tracker.push(transitions, valid_until);
        let mask = (1u8 << self.scl_bit) | (1u8 << self.sda_bit);
        let mut out = Vec::new();
        while let Some((sample, before, after)) = self.tracker.next_relevant(mask) {
            let scl_changed = (before >> self.scl_bit) & 1 != (after >> self.scl_bit) & 1;
            let scl_now = (after >> self.scl_bit) & 1 == 1;
            let sda_changed = (before >> self.sda_bit) & 1 != (after >> self.sda_bit) & 1;
            let sda_now = (after >> self.sda_bit) & 1 == 1;
            if scl_changed {
                if scl_now && self.open_transaction {
                    self.on_scl_rising(sample, sda_now, &mut out);
                }
            } else if sda_changed && scl_now {
                if sda_now {
                    self.on_stop(sample, &mut out);
                } else {
                    self.on_start(sample, &mut out);
                }
            }
        }
        out
    }

    fn seed(&mut self, levels: u8) {
        self.tracker.seed(levels);
    }

    fn finish(&mut self, end_sample: u64) -> Vec<Annotation> {
        let mut out = Vec::new();
        if let Some(slot) = self.slot.take() {
            let (start, text) = match slot {
                Slot::Byte { bits, start, .. } => {
                    (start, format!("{} of 8 bits clocked in", bits.len()))
                }
                Slot::Ack => (end_sample, "waiting for ACK/NACK".to_string()),
            };
            out.push(Annotation {
                start_sample: start,
                end_sample: end_sample.max(start),
                level: LEVEL_FIELD,
                kind: AnnotationKind::Incomplete,
                text,
            });
        } else if self.open_transaction {
            out.push(Annotation::new(
                end_sample,
                end_sample,
                LEVEL_FRAME,
                AnnotationKind::Incomplete,
            ));
        }
        self.open_transaction = false;
        self.next_kind = FieldKind::Address;
        self.ten_bit_addr = None;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::render_i2c;
    use crate::test_util::{assert_all_split_points, assert_same_result, build_plan};
    use proptest::proptest;

    /// Bit-bangs SCL/SDA the way a real I²C master would, so the decoder is
    /// exercised against realistic edge sequences (setup-before-rising,
    /// hold-through-high, release-before-falling) rather than idealized
    /// hand-picked transitions.
    struct Waveform {
        scl_bit: u8,
        sda_bit: u8,
        half: u64,
        cursor: u64,
        scl: bool,
        sda: bool,
        transitions: Vec<Transition>,
    }

    impl Waveform {
        fn new(scl_bit: u8, sda_bit: u8, half: u64) -> Self {
            Waveform {
                scl_bit,
                sda_bit,
                half,
                cursor: 0,
                scl: true,
                sda: true,
                transitions: Vec::new(),
            }
        }

        fn set(&mut self, scl: bool, sda: bool) {
            if scl != self.scl || sda != self.sda {
                self.scl = scl;
                self.sda = sda;
                let value = (u8::from(scl) << self.scl_bit) | (u8::from(sda) << self.sda_bit);
                self.transitions.push(Transition {
                    sample_index: self.cursor,
                    value,
                });
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

        /// A START condition. From bus-idle (SCL/SDA both high) it's just
        /// SDA falling while SCL is high; a repeated start (SCL currently
        /// low, right after a previous byte's ack) first has to bring SCL
        /// high with SDA released before it can fall.
        fn do_start(&mut self) {
            if !self.scl {
                self.set(false, true);
                self.advance();
                self.set(true, true);
                self.advance();
            }
            self.set(true, false);
            self.advance();
            self.set(false, false);
        }

        fn do_bit(&mut self, bit: bool) {
            self.set(false, bit);
            self.advance();
            self.set(true, bit);
            self.advance();
            self.set(false, bit);
        }

        fn do_byte(&mut self, byte: u8) {
            for i in (0..8).rev() {
                self.do_bit((byte >> i) & 1 == 1);
            }
        }

        fn do_ack(&mut self, ack: bool) {
            self.do_bit(!ack);
        }

        /// SCL must currently be low (right after a byte's ack).
        fn do_stop(&mut self) {
            self.set(false, false);
            self.advance();
            self.set(true, false);
            self.advance();
            self.set(true, true);
            self.advance();
        }

        fn finish(self) -> (Vec<Transition>, u64) {
            (self.transitions, self.cursor)
        }
    }

    fn decode_all(cfg: I2cConfig, transitions: &[Transition], end: u64) -> Vec<Annotation> {
        let mut d = I2cDecoder::new(cfg).unwrap();
        let mut out = d.feed(transitions, end);
        out.extend(d.finish(end));
        out
    }

    #[test]
    fn write_transaction_at_100khz() {
        // 100 kHz half-period at a 1 MHz sample rate = 5 samples.
        let mut w = Waveform::new(0, 1, 5);
        w.idle(4);
        w.do_start();
        w.do_byte(0x50 << 1);
        w.do_ack(true);
        w.do_byte(0x01);
        w.do_ack(true);
        w.do_byte(0x02);
        w.do_ack(true);
        w.do_stop();
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(I2cConfig { scl: 0, sda: 1 }, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Start,
                AnnotationKind::Address {
                    address: 0x50,
                    read: false
                },
                AnnotationKind::Ack,
                AnnotationKind::Data(1),
                AnnotationKind::Ack,
                AnnotationKind::Data(2),
                AnnotationKind::Ack,
                AnnotationKind::Stop,
            ]
        );
        assert_eq!(
            render_i2c(&anns),
            vec!["S 0x50 W A 0x01 A 0x02 A P".to_string()]
        );
    }

    /// Live-test regression (2026-09-15): a decode window opened while SCL
    /// was high and SDA low — mid-byte on an SSD1306 framebuffer write —
    /// read as a START at the window's first sample, and the bytes after it
    /// came out shifted. Seeded with the real levels, the decoder waits for
    /// the next genuine START instead.
    #[test]
    fn a_window_opened_mid_transaction_waits_for_a_real_start() {
        let mut w = Waveform::new(0, 1, 5);
        w.idle(4);
        w.do_start();
        w.do_byte(0x3C << 1);
        w.do_ack(true);
        w.do_byte(0x40);
        w.do_ack(true);
        w.do_byte(0x00);
        w.do_ack(true);
        w.do_stop();
        w.idle(4);
        w.do_start();
        w.do_byte(0x3C << 1);
        w.do_ack(true);
        w.do_byte(0x55);
        w.do_ack(true);
        w.do_stop();
        let (transitions, end) = w.finish();

        // A window start inside byte 0x00's clocking, SCL high and SDA low.
        let (scl_high_sda_low, idx) = (0b01, 30);
        let cut = transitions[idx..]
            .iter()
            .position(|t| t.value == scl_high_sda_low)
            .map(|p| idx + p)
            .expect("a data-0 bit inside the first transaction");
        let window = &transitions[cut..];

        let unseeded = decode_all(I2cConfig { scl: 0, sda: 1 }, window, end);
        assert_eq!(
            unseeded[0].kind,
            AnnotationKind::Start,
            "the bug: a phantom START"
        );

        let mut d = I2cDecoder::new(I2cConfig { scl: 0, sda: 1 }).unwrap();
        d.seed(window[0].value);
        let mut anns = d.feed(window, end);
        anns.extend(d.finish(end));
        // The first transaction's tail yields only its (real) STOP.
        assert_eq!(anns[0].kind, AnnotationKind::Stop);
        assert_eq!(
            render_i2c(&anns[1..]),
            vec!["S 0x3C W A 0x55 A P".to_string()]
        );
    }

    /// SPEC §6's own example line, at 400 kHz: a write then a repeated
    /// start into a read, the last byte NACKed.
    #[test]
    fn repeated_start_write_then_read_at_400khz() {
        // 400 kHz half-period at a 24 MHz sample rate = 30 samples.
        let mut w = Waveform::new(0, 1, 30);
        w.idle(4);
        w.do_start();
        w.do_byte(0x48 << 1);
        w.do_ack(true);
        w.do_byte(0x01);
        w.do_ack(true);
        w.do_start(); // no do_stop first: this is a repeated start
        w.do_byte((0x48 << 1) | 1);
        w.do_ack(true);
        w.do_byte(0x9C);
        w.do_ack(false); // master NACKs the last byte it reads
        w.do_stop();
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(I2cConfig { scl: 0, sda: 1 }, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Start,
                AnnotationKind::Address {
                    address: 0x48,
                    read: false
                },
                AnnotationKind::Ack,
                AnnotationKind::Data(1),
                AnnotationKind::Ack,
                AnnotationKind::RepeatedStart,
                AnnotationKind::Address {
                    address: 0x48,
                    read: true
                },
                AnnotationKind::Ack,
                AnnotationKind::Data(0x9C),
                AnnotationKind::Nack,
                AnnotationKind::Stop,
            ]
        );
        assert_eq!(
            render_i2c(&anns),
            vec!["S 0x48 W A 0x01 A Sr 0x48 R A 0x9C N P".to_string()]
        );
    }

    #[test]
    fn address_nack_aborts_gracefully() {
        let mut w = Waveform::new(0, 1, 5);
        w.idle(4);
        w.do_start();
        w.do_byte(0x10 << 1);
        w.do_ack(false);
        w.do_stop();
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(I2cConfig { scl: 0, sda: 1 }, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Start,
                AnnotationKind::Address {
                    address: 0x10,
                    read: false
                },
                AnnotationKind::Nack,
                AnnotationKind::Stop,
            ]
        );
    }

    #[test]
    fn ten_bit_address_write_then_data() {
        let address: u16 = 0x1A2;
        let addr_high = ((address >> 8) & 0x3) as u8;
        let addr_low = (address & 0xFF) as u8;
        let first_byte = ((0b1_1110u8 << 2) | addr_high) << 1; // W = 0

        let mut w = Waveform::new(0, 1, 5);
        w.idle(4);
        w.do_start();
        w.do_byte(first_byte);
        w.do_ack(true);
        w.do_byte(addr_low);
        w.do_ack(true);
        w.do_byte(0x55);
        w.do_ack(true);
        w.do_stop();
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(I2cConfig { scl: 0, sda: 1 }, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Start,
                AnnotationKind::Ack,
                AnnotationKind::Address {
                    address,
                    read: false
                },
                AnnotationKind::Ack,
                AnnotationKind::Data(0x55),
                AnnotationKind::Ack,
                AnnotationKind::Stop,
            ]
        );
    }

    #[test]
    fn ten_bit_address_repeated_start_read() {
        let address: u16 = 0x1A2;
        let addr_high = ((address >> 8) & 0x3) as u8;
        let addr_low = (address & 0xFF) as u8;
        let first_byte = ((0b1_1110u8 << 2) | addr_high) << 1;

        let mut w = Waveform::new(0, 1, 5);
        w.idle(4);
        w.do_start();
        w.do_byte(first_byte);
        w.do_ack(true);
        w.do_byte(addr_low);
        w.do_ack(true);
        w.do_start();
        w.do_byte(first_byte | 1); // R = 1, no second address byte
        w.do_ack(true);
        w.do_byte(0xAA);
        w.do_ack(false);
        w.do_stop();
        w.idle(4);
        let (transitions, end) = w.finish();
        let anns = decode_all(I2cConfig { scl: 0, sda: 1 }, &transitions, end);
        let kinds: Vec<_> = anns.iter().map(|a| a.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotationKind::Start,
                AnnotationKind::Ack,
                AnnotationKind::Address {
                    address,
                    read: false
                },
                AnnotationKind::Ack,
                AnnotationKind::RepeatedStart,
                AnnotationKind::Address {
                    address,
                    read: true
                },
                AnnotationKind::Ack,
                AnnotationKind::Data(0xAA),
                AnnotationKind::Nack,
                AnnotationKind::Stop,
            ]
        );
    }

    #[test]
    fn rejects_same_bit() {
        let err = I2cDecoder::new(I2cConfig { scl: 2, sda: 2 }).unwrap_err();
        assert_eq!(err, I2cConfigError::SameBit(2));
    }

    #[test]
    fn finish_flushes_open_transaction() {
        let mut w = Waveform::new(0, 1, 5);
        w.idle(4);
        w.do_start();
        w.do_byte(0x50 << 1);
        // No ack, no stop: the address byte completes (so it's already
        // reported) but the transaction is left open awaiting its ack.
        let (transitions, end) = w.finish();
        let mut d = I2cDecoder::new(I2cConfig { scl: 0, sda: 1 }).unwrap();
        let mid = d.feed(&transitions, end);
        assert_eq!(
            mid.iter().map(|a| a.kind.clone()).collect::<Vec<_>>(),
            vec![
                AnnotationKind::Start,
                AnnotationKind::Address {
                    address: 0x50,
                    read: false
                }
            ]
        );
        let flushed = d.finish(end);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].kind, AnnotationKind::Incomplete);
    }

    #[test]
    fn chunking_invariant() {
        let mut w = Waveform::new(0, 1, 5);
        w.idle(3);
        w.do_start();
        w.do_byte(0x48 << 1);
        w.do_ack(true);
        w.do_byte(0x01);
        w.do_ack(true);
        w.do_start();
        w.do_byte((0x48 << 1) | 1);
        w.do_ack(true);
        w.do_byte(0x9C);
        w.do_ack(false);
        w.do_stop();
        w.idle(3);
        let (transitions, end) = w.finish();
        let make = || I2cDecoder::new(I2cConfig { scl: 0, sda: 1 }).unwrap();
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
