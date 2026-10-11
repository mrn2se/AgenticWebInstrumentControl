//! Grouping a decoder's annotation stream into transactions (live test
//! F-02): one entry per I²C START..STOP, per UART burst, per SPI CS window —
//! the unit an agent reasons about. One SSD1306 framebuffer write is ~11 000
//! annotations (a field, eight bits and an ACK per byte) but 66 transactions
//! here.
//!
//! Streaming, like the decoders: [`TransactionGrouper::push`] takes each batch
//! of annotations as it is produced and returns the transactions it
//! completed; [`TransactionGrouper::finish`] flushes the one still open.

use crate::{Annotation, AnnotationKind, LEVEL_FRAME};

/// Which grouping rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupProtocol {
    /// Bursts of characters: a new burst starts after an idle gap longer
    /// than [`UART_GAP_FRAMES`] character times.
    Uart,
    /// START (or repeated START) to STOP.
    I2c,
    /// Chip-select windows; without CS, bursts of words split by idle gaps
    /// like UART.
    Spi,
    /// One transaction per CAN frame; an error flag is folded into the
    /// frame it cut short.
    Can,
    /// One transaction per 1-Wire reset, through the last byte before the
    /// next one.
    OneWire,
}

/// Idle time, in character (or word) times, that ends a UART burst or a
/// CS-less SPI burst.
pub const UART_GAP_FRAMES: f64 = 2.0;
/// A transaction holding this many data items is closed and continued in the
/// next one, so a line that never goes idle cannot grow one without bound.
pub const MAX_TRANSACTION_ITEMS: usize = 4096;

/// A pause in the clock this many words long, inside one CS window, is
/// flagged as a possible missed CS release.
pub const SPI_PAUSE_WORDS: f64 = 1.5;
/// ... and only when it is also this many times the window's typical gap
/// between words, so firmware that always pauses between bytes is not
/// flagged on every one.
pub const SPI_PAUSE_OVER_TYPICAL: f64 = 3.0;

/// A clock pause inside one CS window that looks like two transfers whose CS
/// release was too short to sample (issue #32: at 16 MS/s, a LOAD TX and the
/// RTS after it decoded as one transaction, CS high for under 62.5 ns
/// between them). Needs three words or more, so there is a typical gap to
/// compare against.
fn spi_clock_pause(words: &[(u64, u64)]) -> Option<String> {
    if words.len() < 3 {
        return None;
    }
    let mut lens: Vec<u64> = words.iter().map(|(s, e)| e.saturating_sub(*s)).collect();
    lens.sort_unstable();
    let word = lens[lens.len() / 2].max(1) as f64;
    let gaps: Vec<(u64, usize)> = words
        .windows(2)
        .enumerate()
        .map(|(k, w)| (w[1].0.saturating_sub(w[0].1), k + 1))
        .collect();
    let (gap, after) = *gaps.iter().max_by_key(|(g, _)| *g)?;
    let mut sorted: Vec<u64> = gaps.iter().map(|(g, _)| *g).collect();
    sorted.sort_unstable();
    let typical = sorted[sorted.len() / 2] as f64;
    let gap_f = gap as f64;
    (gap_f > SPI_PAUSE_WORDS * word && gap_f > SPI_PAUSE_OVER_TYPICAL * typical + 2.0).then(|| {
        format!(
            "clock paused {gap} samples ({:.1} words) before word {after} (sample {}) inside \
             one CS window: possibly two transfers with a CS release too short to sample",
            gap_f / word,
            words[after].0
        )
    })
}

/// One transaction's content.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Transaction {
    pub start_sample: u64,
    pub end_sample: u64,
    /// I²C: the address and direction this transaction was addressed with.
    /// CAN: the frame's identifier (11 or 29 bits). 1-Wire: the family code
    /// of the ROM code it carried.
    pub address: Option<u32>,
    pub read: Option<bool>,
    /// I²C: begun by a repeated START (continues the previous transaction
    /// without a STOP in between).
    pub repeated_start: bool,
    /// UART characters, I²C data bytes (after the address), SPI MOSI words,
    /// CAN data bytes, 1-Wire bytes after the ROM command and ROM code.
    pub data: Vec<u32>,
    /// SPI MISO words.
    pub miso: Vec<u32>,
    /// I²C: 1 when the address or a written byte was NACKed — counted, not
    /// inferred from the notes (B-43), and at most once: every byte after a
    /// NACK reads as NACKed too (B-82). The master's NACK that ends a read is
    /// not one. CAN: 1 for a frame nobody acknowledged. 1-Wire: 1 for a
    /// reset no device answered.
    pub nacks: u32,
    /// 1-Wire: whether a device answered the reset with a presence pulse;
    /// `None` for an entry that began without a reset.
    pub presence: Option<bool>,
    /// Framing/parity errors, breaks, decoder errors and incomplete ends.
    pub errors: u32,
    /// Everything that was not a clean byte: NACKs, framing/parity errors,
    /// breaks, an incomplete end.
    pub notes: Vec<String>,
}

pub struct TransactionGrouper {
    protocol: GroupProtocol,
    current: Option<Transaction>,
    /// Length of the last UART frame / SPI word, the unit of the idle gap.
    unit: u64,
    /// SPI: a ChipSelect annotation has been seen, so CS frames transfers.
    spi_has_cs: bool,
    /// I²C: NACKs after the first in the open transaction.
    i2c_more_nacks: u32,
    /// I²C: address-less STARTs folded into the pending error entry.
    i2c_bare_starts: u32,
    /// SPI with CS: the open window's words as `(start, end)` samples, to
    /// find a clock pause inside it at CS release.
    spi_words: Vec<(u64, u64)>,
    /// Data items a burst holds before it is continued in the next entry.
    max_items: usize,
}

impl TransactionGrouper {
    pub fn new(protocol: GroupProtocol) -> Self {
        Self {
            protocol,
            current: None,
            unit: 0,
            spi_has_cs: false,
            i2c_more_nacks: 0,
            i2c_bare_starts: 0,
            spi_words: Vec::new(),
            max_items: MAX_TRANSACTION_ITEMS,
        }
    }

    /// Bounds a burst — a UART burst, a CS-less SPI burst — at `max` data
    /// items (at most [`MAX_TRANSACTION_ITEMS`]); a longer one is continued
    /// in the next entry (B-97). Framed transactions (an I²C START..STOP, a
    /// CS window, a CAN frame) are never cut.
    pub fn with_max_items(mut self, max: usize) -> Self {
        self.max_items = max.clamp(1, MAX_TRANSACTION_ITEMS);
        self
    }

    pub fn push(&mut self, annotations: &[Annotation]) -> Vec<Transaction> {
        let mut done = Vec::new();
        for a in annotations {
            match self.protocol {
                GroupProtocol::Uart => self.push_uart(a, &mut done),
                GroupProtocol::I2c => self.push_i2c(a, &mut done),
                GroupProtocol::Spi => self.push_spi(a, &mut done),
                GroupProtocol::Can => self.push_can(a, &mut done),
                GroupProtocol::OneWire => self.push_onewire(a, &mut done),
            }
        }
        done
    }

    pub fn finish(&mut self) -> Vec<Transaction> {
        let mut done = Vec::new();
        self.close(&mut done);
        done
    }

    fn close(&mut self, done: &mut Vec<Transaction>) {
        let Some(mut t) = self.current.take() else {
            return;
        };
        if self.protocol == GroupProtocol::I2c {
            if self.i2c_more_nacks > 0 {
                t.notes.push(format!(
                    "{} more bytes NACKed after it",
                    std::mem::take(&mut self.i2c_more_nacks)
                ));
            }
            // A START that never got an address byte is no transaction — on
            // a line that only looks like I²C there are many (B-83). Runs of
            // them fold into one error entry, in place of empty
            // "acknowledged" transactions with no address.
            if t.address.is_none() {
                let note = |n: u32| {
                    format!(
                        "{n} START{} without an address byte",
                        if n == 1 { "" } else { "s" }
                    )
                };
                match done
                    .last_mut()
                    .filter(|p| p.address.is_none() && self.i2c_bare_starts > 0)
                {
                    Some(p) => {
                        self.i2c_bare_starts += 1;
                        p.end_sample = p.end_sample.max(t.end_sample);
                        p.notes = vec![note(self.i2c_bare_starts)];
                    }
                    None => {
                        self.i2c_bare_starts = 1;
                        t.repeated_start = false;
                        t.data.clear();
                        t.nacks = 0;
                        t.errors = 1;
                        t.notes = vec![note(1)];
                        done.push(t);
                    }
                }
                return;
            }
            self.i2c_bare_starts = 0;
        }
        done.push(t);
    }

    fn open(&mut self, start: u64) -> &mut Transaction {
        self.current.insert(Transaction {
            start_sample: start,
            end_sample: start,
            ..Default::default()
        })
    }

    /// Opens a new burst unless `a` follows the current one closely enough.
    fn burst_for(&mut self, a: &Annotation, done: &mut Vec<Transaction>) -> &mut Transaction {
        let gap_limit = (self.unit as f64 * UART_GAP_FRAMES).max(1.0) as u64;
        // Frames on the wire, a bad one included; the "continues" note is
        // not a frame, so it does not take a byte's place (B-104).
        let frames = |t: &Transaction| t.data.len() + t.errors as usize;
        let continues = self.current.as_ref().is_some_and(|t| {
            a.start_sample.saturating_sub(t.end_sample) <= gap_limit && frames(t) < self.max_items
        });
        if !continues {
            let split = self
                .current
                .as_ref()
                .is_some_and(|t| frames(t) >= self.max_items);
            self.close(done);
            let t = self.open(a.start_sample);
            if split {
                t.notes.push("continues the previous transaction".into());
            }
        }
        self.current.as_mut().expect("opened above")
    }

    fn push_uart(&mut self, a: &Annotation, done: &mut Vec<Transaction>) {
        if a.level != LEVEL_FRAME {
            return;
        }
        if matches!(a.kind, AnnotationKind::Break) {
            // A break is its own event, never part of a burst.
            self.close(done);
            let t = self.open(a.start_sample);
            t.end_sample = a.end_sample;
            t.notes.push("break".into());
            t.errors += 1;
            self.close(done);
            return;
        }
        self.unit = a.end_sample.saturating_sub(a.start_sample).max(1);
        let t = self.burst_for(a, done);
        // The frame's place in the burst, counting the frames an error
        // dropped from `data`: numbered by `data` alone, errors next to each
        // other shared one index (B-85). The sample finds it on the line.
        let frame = t.data.len() + t.errors as usize;
        let at = a.start_sample;
        match &a.kind {
            AnnotationKind::Data(v) => t.data.push(u32::from(*v)),
            AnnotationKind::FramingError => {
                t.errors += 1;
                t.notes.push(format!(
                    "framing error in frame {frame} (sample {at}), not in data"
                ));
            }
            AnnotationKind::ParityError => {
                t.errors += 1;
                t.notes.push(format!(
                    "parity error in frame {frame} (sample {at}), not in data"
                ));
            }
            AnnotationKind::Incomplete => {
                t.errors += 1;
                t.notes.push("incomplete last frame".into());
            }
            AnnotationKind::Error(msg) => {
                t.errors += 1;
                t.notes.push(msg.clone());
            }
            _ => {}
        }
        t.end_sample = t.end_sample.max(a.end_sample);
    }

    fn push_i2c(&mut self, a: &Annotation, done: &mut Vec<Transaction>) {
        match &a.kind {
            AnnotationKind::Start => {
                self.close(done);
                self.open(a.start_sample);
            }
            AnnotationKind::RepeatedStart => {
                self.close(done);
                self.open(a.start_sample).repeated_start = true;
            }
            AnnotationKind::Stop => {
                if let Some(t) = self.current.as_mut() {
                    t.end_sample = a.end_sample;
                }
                self.close(done);
            }
            // Anything else only means something inside a transaction: a
            // window opened mid-transaction yields nothing until a START.
            kind => {
                let Some(t) = self.current.as_mut() else {
                    return;
                };
                t.end_sample = t.end_sample.max(a.end_sample);
                match kind {
                    AnnotationKind::Address { address, read } => {
                        t.address = Some(u32::from(*address));
                        t.read = Some(*read);
                    }
                    AnnotationKind::Data(v) => t.data.push(u32::from(*v)),
                    // The master NACKs the last byte it reads: the end of a
                    // read, not a failure (B-82).
                    AnnotationKind::Nack if t.read == Some(true) && !t.data.is_empty() => {}
                    AnnotationKind::Nack if t.nacks > 0 => self.i2c_more_nacks += 1,
                    AnnotationKind::Nack => {
                        t.nacks = 1;
                        if t.data.is_empty() {
                            t.notes.push("address NACK".into());
                        } else {
                            t.notes
                                .push(format!("NACK after byte {}", t.data.len() - 1));
                        }
                    }
                    AnnotationKind::Incomplete => {
                        t.errors += 1;
                        // A frame-level one carries no detail of its own.
                        t.notes.push(if a.level == LEVEL_FRAME {
                            "incomplete: the range ends inside this transaction".into()
                        } else {
                            format!("incomplete: {}", a.text)
                        });
                    }
                    AnnotationKind::Error(msg) => {
                        t.errors += 1;
                        t.notes.push(msg.clone());
                    }
                    _ => {}
                }
                if t.data.len() >= MAX_TRANSACTION_ITEMS {
                    let (address, read) = (t.address, t.read);
                    self.close(done);
                    let next = self.open(a.end_sample);
                    next.address = address;
                    next.read = read;
                    next.notes.push("continues the previous transaction".into());
                }
            }
        }
    }

    fn push_spi(&mut self, a: &Annotation, done: &mut Vec<Transaction>) {
        match &a.kind {
            AnnotationKind::ChipSelect { active: true } => {
                self.spi_has_cs = true;
                self.close(done);
                self.spi_words.clear();
                self.open(a.start_sample);
            }
            AnnotationKind::ChipSelect { active: false } => {
                self.spi_has_cs = true;
                let pause = spi_clock_pause(&std::mem::take(&mut self.spi_words));
                if let Some(t) = self.current.as_mut() {
                    t.end_sample = a.end_sample;
                    if let Some(note) = pause {
                        t.notes.push(note);
                    }
                }
                self.close(done);
            }
            AnnotationKind::Word { mosi, miso } => {
                let t = if self.spi_has_cs {
                    // Seeded mid-window, words before the next CS assert
                    // belong to no transfer.
                    let Some(t) = self.current.as_mut() else {
                        return;
                    };
                    self.spi_words.push((a.start_sample, a.end_sample));
                    t
                } else {
                    self.unit = a.end_sample.saturating_sub(a.start_sample).max(1);
                    self.burst_for(a, done)
                };
                if let Some(m) = mosi {
                    t.data.push(*m);
                }
                if let Some(s) = miso {
                    t.miso.push(*s);
                }
                t.end_sample = t.end_sample.max(a.end_sample);
            }
            AnnotationKind::Incomplete => {
                if let Some(t) = self.current.as_mut() {
                    t.notes.push("incomplete last word".into());
                }
            }
            _ => {}
        }
    }

    /// Whether an error annotation at `a` belongs to the frame still open:
    /// it follows within a frame's own length, so one long after is not
    /// charged to a frame that ended cleanly.
    fn follows_open_frame(&self, a: &Annotation) -> bool {
        self.current.as_ref().is_some_and(|t| {
            a.start_sample.saturating_sub(t.end_sample) <= (t.end_sample - t.start_sample).max(1)
        })
    }

    fn push_can(&mut self, a: &Annotation, done: &mut Vec<Transaction>) {
        match &a.kind {
            AnnotationKind::CanFrame {
                id,
                extended,
                remote,
                dlc,
                data,
                crc_ok,
                acked,
            } => {
                self.close(done);
                let t = self.open(a.start_sample);
                t.end_sample = a.end_sample;
                t.address = Some(*id);
                t.data = data.iter().map(|&b| u32::from(b)).collect();
                if *extended {
                    t.notes.push("extended".into());
                }
                if *remote {
                    t.notes.push(format!("remote, DLC {dlc}"));
                }
                if !acked {
                    t.nacks += 1;
                    t.notes.push("no ACK".into());
                }
                if !crc_ok {
                    t.errors += 1;
                    t.notes.push("CRC error".into());
                }
            }
            // A frame the decoder had to abandon (stuff error, form error in
            // the CRC delimiter) is a transaction of its own, with no ID. It
            // stays open so the error flag that follows joins it.
            AnnotationKind::Error(msg) if a.level == LEVEL_FRAME => {
                self.close(done);
                let t = self.open(a.start_sample);
                t.end_sample = a.end_sample;
                t.errors += 1;
                t.notes.push(msg.clone());
            }
            // A form error inside a frame already reported (EOF) or an error
            // flag: both are notes on that frame; with none to join, the
            // error flag is a transaction itself.
            AnnotationKind::Error(_) | AnnotationKind::CanErrorFrame => {
                let note = match &a.kind {
                    AnnotationKind::Error(msg) => msg.clone(),
                    _ => "error frame".to_string(),
                };
                if !self.follows_open_frame(a) {
                    self.close(done);
                    self.open(a.start_sample);
                }
                let t = self.current.as_mut().expect("open above or joined");
                t.end_sample = t.end_sample.max(a.end_sample);
                t.errors += 1;
                t.notes.push(note);
            }
            AnnotationKind::Incomplete => {
                self.close(done);
                let t = self.open(a.start_sample);
                t.end_sample = a.end_sample;
                t.errors += 1;
                t.notes.push("incomplete frame".into());
                self.close(done);
            }
            _ => {}
        }
    }

    fn push_onewire(&mut self, a: &Annotation, done: &mut Vec<Transaction>) {
        match &a.kind {
            AnnotationKind::OneWireReset {
                presence,
                overdrive,
            } => {
                self.close(done);
                let t = self.open(a.start_sample);
                t.end_sample = a.end_sample;
                t.presence = Some(*presence);
                if *overdrive {
                    t.notes.push("overdrive".into());
                }
                if !presence {
                    t.nacks = 1;
                    t.notes.push("no presence pulse".into());
                }
            }
            // Slots skipped before a reset: an entry of its own.
            AnnotationKind::Incomplete if a.level == LEVEL_FRAME => {
                self.close(done);
                let t = self.open(a.start_sample);
                t.end_sample = a.end_sample;
                t.errors = 1;
                t.notes.push(a.text.clone());
                self.close(done);
            }
            AnnotationKind::OneWireBit(_) => {
                if let Some(t) = self.current.as_mut() {
                    t.end_sample = t.end_sample.max(a.end_sample);
                }
            }
            kind => {
                if self.current.is_none() {
                    // An error with no reset before it.
                    self.open(a.start_sample);
                }
                let t = self.current.as_mut().expect("opened above");
                t.end_sample = t.end_sample.max(a.end_sample);
                match kind {
                    AnnotationKind::OneWireRomCommand(_) => t.notes.push(a.text.clone()),
                    AnnotationKind::OneWireRom { rom, crc_ok } => {
                        t.address = Some((*rom & 0xFF) as u32);
                        t.notes
                            .push(format!("ROM {}", crate::onewire::format_rom(*rom)));
                        if !crc_ok {
                            t.errors += 1;
                            t.notes.push("ROM CRC error".into());
                        }
                    }
                    AnnotationKind::Data(v) => t.data.push(u32::from(*v)),
                    AnnotationKind::Incomplete | AnnotationKind::Error(_) => {
                        t.errors += 1;
                        t.notes.push(a.text.clone());
                    }
                    _ => {}
                }
                if t.data.len() >= MAX_TRANSACTION_ITEMS {
                    let address = t.address;
                    self.close(done);
                    let next = self.open(a.end_sample);
                    next.address = address;
                    next.notes.push("continues the previous transaction".into());
                }
            }
        }
    }
}

/// Space-separated hex, two digits per byte (more for wider SPI words).
pub fn hex_string(values: &[u32]) -> String {
    let width = values
        .iter()
        .map(|&v| {
            if v > 0xFFFF {
                8
            } else if v > 0xFF {
                4
            } else {
                2
            }
        })
        .max()
        .unwrap_or(2);
    values
        .iter()
        .map(|v| format!("{v:0width$X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The bytes as text for a UART burst: printable ASCII as is, `\r` `\n` `\t`
/// escaped, a well-formed UTF-8 sequence for a printable character as that
/// character, anything else as `\xNN`. A device printing "Grüße" used to read
/// `Gr\xC3\xBC\xC3\x9Fe` (B-88); a lone, invalid or overlong byte, a C1
/// control character, or a 9-bit value is still escaped byte by byte, so
/// nothing that is not text can pass for text.
pub fn escaped_text(values: &[u32]) -> String {
    let mut s = String::new();
    let mut i = 0;
    while i < values.len() {
        let v = values[i];
        match v {
            0x0D => s.push_str("\\r"),
            0x0A => s.push_str("\\n"),
            0x09 => s.push_str("\\t"),
            0x5C => s.push_str("\\\\"),
            0x20..=0x7E => s.push(v as u8 as char),
            0xC2..=0xF4 => {
                if let Some((c, len)) = utf8_char(&values[i..]) {
                    s.push(c);
                    i += len;
                    continue;
                }
                s.push_str(&format!("\\x{:02X}", v & 0xFF));
            }
            _ => s.push_str(&format!("\\x{:02X}", v & 0xFF)),
        }
        i += 1;
    }
    s
}

/// The printable character a UTF-8 sequence at the start of `values` encodes,
/// and its length in bytes — `None` if it is cut short, malformed, or a
/// control character.
fn utf8_char(values: &[u32]) -> Option<(char, usize)> {
    let len = match values.first()? {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return None,
    };
    let bytes: Vec<u8> = values
        .get(..len)?
        .iter()
        .map(|&b| u8::try_from(b).ok())
        .collect::<Option<_>>()?;
    let c = std::str::from_utf8(&bytes).ok()?.chars().next()?;
    (!c.is_control()).then_some((c, len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{LEVEL_BIT, LEVEL_FIELD};

    fn ann(start: u64, end: u64, level: u8, kind: AnnotationKind) -> Annotation {
        Annotation {
            start_sample: start,
            end_sample: end,
            level,
            text: String::new(),
            kind,
        }
    }

    #[test]
    fn i2c_groups_start_to_stop_and_splits_repeated_starts() {
        use AnnotationKind::*;
        let anns = vec![
            ann(5, 5, LEVEL_FIELD, Data(0x99)), // before any START: ignored
            ann(10, 10, LEVEL_FRAME, Start),
            ann(
                11,
                19,
                LEVEL_FIELD,
                Address {
                    address: 0x3C,
                    read: false,
                },
            ),
            ann(19, 19, LEVEL_BIT, Ack),
            ann(20, 28, LEVEL_FIELD, Data(0x40)),
            ann(29, 37, LEVEL_FIELD, Data(0x01)),
            ann(38, 38, LEVEL_FRAME, RepeatedStart),
            ann(
                39,
                47,
                LEVEL_FIELD,
                Address {
                    address: 0x3C,
                    read: true,
                },
            ),
            ann(48, 56, LEVEL_FIELD, Data(0x7F)),
            ann(56, 56, LEVEL_BIT, Nack),
            ann(58, 58, LEVEL_FRAME, Stop),
        ];
        let mut g = TransactionGrouper::new(GroupProtocol::I2c);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(txs.len(), 2);
        assert_eq!((txs[0].address, txs[0].read), (Some(0x3C), Some(false)));
        assert_eq!(txs[0].data, vec![0x40, 0x01]);
        assert_eq!((txs[0].start_sample, txs[0].end_sample), (10, 37));
        assert!(txs[1].repeated_start);
        assert_eq!(txs[1].read, Some(true));
        assert_eq!(txs[1].data, vec![0x7F]);
        assert_eq!(txs[1].end_sample, 58);
        // B-43: NACKs are counted, so "none" is a number, not an absence. The
        // NACK ending the read is the master's, as the protocol requires, and
        // not one (B-82).
        assert_eq!((txs[0].nacks, txs[1].nacks), (0, 0));
        assert!(txs[1].notes.is_empty(), "{:?}", txs[1].notes);
    }

    /// B-82: a write whose address is NACKed and whose bytes are clocked out
    /// anyway (mirrored traffic on the ESP32 bench) counts one NACK, not one
    /// per byte; the rest is one note.
    #[test]
    fn a_nacked_write_counts_one_nack() {
        use AnnotationKind::*;
        let mut anns = vec![
            ann(0, 0, LEVEL_FRAME, Start),
            ann(
                1,
                9,
                LEVEL_FIELD,
                Address {
                    address: 0x3C,
                    read: false,
                },
            ),
            ann(9, 9, LEVEL_BIT, Nack),
        ];
        for k in 0..3u64 {
            let s = 10 + 10 * k;
            anns.push(ann(s, s + 8, LEVEL_FIELD, Data(k as u16)));
            anns.push(ann(s + 8, s + 8, LEVEL_BIT, Nack));
        }
        anns.push(ann(50, 50, LEVEL_FRAME, Stop));
        let mut g = TransactionGrouper::new(GroupProtocol::I2c);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].nacks, 1);
        assert_eq!(
            txs[0].notes,
            vec![
                "address NACK".to_string(),
                "3 more bytes NACKed after it".to_string()
            ]
        );
    }

    /// B-83: STARTs that never get an address byte (an I²C driver's bus
    /// recovery, read as I²C) are one error entry, not a chain of empty
    /// transactions; a real one after them is untouched.
    #[test]
    fn starts_without_an_address_fold_into_one_error() {
        use AnnotationKind::*;
        let mut anns = vec![ann(0, 0, LEVEL_FRAME, Start)];
        for k in 1..8u64 {
            anns.push(ann(k * 100, k * 100, LEVEL_FRAME, RepeatedStart));
        }
        anns.extend([
            ann(800, 800, LEVEL_FRAME, Stop),
            ann(900, 900, LEVEL_FRAME, Start),
            ann(
                901,
                909,
                LEVEL_FIELD,
                Address {
                    address: 0x68,
                    read: false,
                },
            ),
            ann(909, 909, LEVEL_BIT, Ack),
            ann(920, 920, LEVEL_FRAME, Stop),
        ]);
        let mut g = TransactionGrouper::new(GroupProtocol::I2c);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(txs.len(), 2, "{txs:?}");
        assert_eq!(txs[0].address, None);
        assert_eq!(txs[0].errors, 1);
        assert_eq!(
            txs[0].notes,
            vec!["8 STARTs without an address byte".to_string()]
        );
        assert_eq!((txs[0].start_sample, txs[0].end_sample), (0, 800));
        assert_eq!(txs[1].address, Some(0x68));
        assert_eq!((txs[1].nacks, txs[1].errors), (0, 0));
    }

    /// B-83: a frame-level `Incomplete` has no detail of its own; its note
    /// said "incomplete: incomplete".
    #[test]
    fn an_incomplete_end_is_said_once() {
        use AnnotationKind::*;
        let anns = vec![
            ann(0, 0, LEVEL_FRAME, Start),
            ann(
                1,
                9,
                LEVEL_FIELD,
                Address {
                    address: 0x7F,
                    read: true,
                },
            ),
            Annotation::new(40, 40, LEVEL_FRAME, Incomplete),
        ];
        let mut g = TransactionGrouper::new(GroupProtocol::I2c);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(
            txs[0].notes,
            vec!["incomplete: the range ends inside this transaction".to_string()]
        );
    }

    /// B-85: errors next to each other used to share the index of the next
    /// good byte ("byte 0", "byte 0").
    #[test]
    fn uart_errors_are_numbered_by_frame() {
        use AnnotationKind::*;
        let anns = vec![
            ann(0, 100, LEVEL_FRAME, FramingError),
            ann(100, 200, LEVEL_FRAME, FramingError),
            ann(200, 300, LEVEL_FRAME, Data(u16::from(b'A'))),
            ann(300, 400, LEVEL_FRAME, ParityError),
        ];
        let mut g = TransactionGrouper::new(GroupProtocol::Uart);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].data, vec![u32::from(b'A')]);
        assert_eq!(
            txs[0].notes,
            vec![
                "framing error in frame 0 (sample 0), not in data".to_string(),
                "framing error in frame 1 (sample 100), not in data".to_string(),
                "parity error in frame 3 (sample 300), not in data".to_string(),
            ]
        );
    }

    #[test]
    fn uart_bursts_split_on_idle_gaps_and_breaks() {
        use AnnotationKind::*;
        let frame = 100;
        let mut anns = Vec::new();
        for (i, b) in b"Hi".iter().enumerate() {
            let s = i as u64 * frame;
            anns.push(ann(s, s + frame, LEVEL_FRAME, Data(u16::from(*b))));
        }
        // 5 frames of idle, then another burst.
        for (i, b) in b"OK\r\n".iter().enumerate() {
            let s = 700 + i as u64 * frame;
            anns.push(ann(s, s + frame, LEVEL_FRAME, Data(u16::from(*b))));
        }
        anns.push(ann(1200, 1300, LEVEL_FRAME, FramingError));
        anns.push(ann(2000, 3500, LEVEL_FRAME, Break));
        let mut g = TransactionGrouper::new(GroupProtocol::Uart);
        let mut txs = g.push(&anns[..3]);
        txs.extend(g.push(&anns[3..]));
        txs.extend(g.finish());
        assert_eq!(txs.len(), 3, "{txs:?}");
        assert_eq!(escaped_text(&txs[0].data), "Hi");
        assert_eq!(escaped_text(&txs[1].data), "OK\\r\\n");
        assert_eq!(
            txs[1].notes,
            vec!["framing error in frame 4 (sample 1200), not in data".to_string()]
        );
        assert_eq!(txs[2].notes, vec!["break".to_string()]);
    }

    #[test]
    fn spi_uses_cs_windows_when_present() {
        use AnnotationKind::*;
        let anns = vec![
            ann(
                0,
                8,
                LEVEL_FIELD,
                Word {
                    mosi: Some(1),
                    miso: Some(2),
                },
            ), // before CS
            ann(10, 10, LEVEL_FRAME, ChipSelect { active: true }),
            ann(
                11,
                19,
                LEVEL_FIELD,
                Word {
                    mosi: Some(0xA5),
                    miso: Some(0x5A),
                },
            ),
            ann(
                20,
                28,
                LEVEL_FIELD,
                Word {
                    mosi: Some(0x01),
                    miso: None,
                },
            ),
            ann(30, 30, LEVEL_FRAME, ChipSelect { active: false }),
        ];
        let mut g = TransactionGrouper::new(GroupProtocol::Spi);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        // The word before the first CS edge is dropped only once CS is known
        // to frame transfers; here it came first, so it forms its own burst.
        assert_eq!(txs.last().unwrap().data, vec![0xA5, 0x01]);
        assert_eq!(txs.last().unwrap().miso, vec![0x5A]);
        assert_eq!(hex_string(&txs.last().unwrap().data), "A5 01");
    }

    fn spi_word(start: u64, mosi: u32) -> Annotation {
        ann(
            start,
            start + 23,
            LEVEL_FIELD,
            AnnotationKind::Word {
                mosi: Some(mosi),
                miso: None,
            },
        )
    }

    /// Issue #32: a 14-byte LOAD TX and its 1-byte RTS under one CS window,
    /// 3.25 us (52 samples at 16 MS/s) apart, 23-sample words 3 apart.
    #[test]
    fn a_clock_pause_inside_one_cs_window_is_flagged() {
        use AnnotationKind::*;
        let mut anns = vec![ann(0, 0, LEVEL_FRAME, ChipSelect { active: true })];
        let mut at = 5;
        for k in 0..14 {
            anns.push(spi_word(at, k));
            at += 26;
        }
        at += 52 - 3;
        anns.push(spi_word(at, 0x81));
        anns.push(ann(
            at + 30,
            at + 30,
            LEVEL_FRAME,
            ChipSelect { active: false },
        ));
        let mut g = TransactionGrouper::new(GroupProtocol::Spi);
        let txs = g.push(&anns);
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].data.len(), 15);
        assert_eq!(txs[0].errors, 0, "a hint, not an error");
        let note = &txs[0].notes[0];
        assert!(note.contains("before word 14"), "{note}");
        assert!(note.contains("CS release"), "{note}");

        // Evenly spaced words, and a two-word window, are not flagged.
        let mut anns = vec![ann(0, 0, LEVEL_FRAME, ChipSelect { active: true })];
        for k in 0..6 {
            anns.push(spi_word(5 + k * 40, k as u32));
        }
        anns.push(ann(300, 300, LEVEL_FRAME, ChipSelect { active: false }));
        anns.push(ann(400, 400, LEVEL_FRAME, ChipSelect { active: true }));
        anns.push(spi_word(405, 1));
        anns.push(spi_word(600, 2));
        anns.push(ann(700, 700, LEVEL_FRAME, ChipSelect { active: false }));
        let txs = g.push(&anns);
        assert_eq!(txs.len(), 2);
        assert!(txs.iter().all(|t| t.notes.is_empty()), "{txs:?}");
    }

    #[test]
    fn a_burst_is_bounded_at_the_callers_max_items() {
        // B-97: a 1999-byte UART burst came back as one ~10 KB entry.
        let anns: Vec<Annotation> = (0..1999u64)
            .map(|i| ann(i * 10, i * 10 + 10, LEVEL_FRAME, AnnotationKind::Data(0x41)))
            .collect();
        let mut g = TransactionGrouper::new(GroupProtocol::Uart).with_max_items(256);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(txs.len(), 8);
        // Every full entry holds max_items bytes, continuations too (B-104:
        // the "continues" note took one byte's place).
        let sizes: Vec<usize> = txs.iter().map(|t| t.data.len()).collect();
        assert_eq!(sizes, [256, 256, 256, 256, 256, 256, 256, 207]);
        assert!(txs[0].notes.is_empty());
        assert_eq!(
            txs[1].notes,
            vec!["continues the previous transaction".to_string()]
        );
        // Framed transactions are not cut.
        assert_eq!(
            TransactionGrouper::new(GroupProtocol::Uart)
                .with_max_items(usize::MAX)
                .max_items,
            MAX_TRANSACTION_ITEMS
        );
    }

    #[test]
    fn a_burst_of_bad_frames_is_bounded_too() {
        // Frames that fail carry no data but are frames on the wire: a line
        // of framing errors is cut at the bound like one of good bytes.
        let anns: Vec<Annotation> = (0..300u64)
            .map(|i| {
                ann(
                    i * 10,
                    i * 10 + 10,
                    LEVEL_FRAME,
                    AnnotationKind::FramingError,
                )
            })
            .collect();
        let mut g = TransactionGrouper::new(GroupProtocol::Uart).with_max_items(100);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        let errors: Vec<u32> = txs.iter().map(|t| t.errors).collect();
        assert_eq!(errors, [100, 100, 100]);
    }

    #[test]
    fn a_never_idle_line_is_split_into_bounded_transactions() {
        let anns: Vec<Annotation> = (0..(MAX_TRANSACTION_ITEMS as u64 + 10))
            .map(|i| ann(i * 10, i * 10 + 10, LEVEL_FRAME, AnnotationKind::Data(0x55)))
            .collect();
        let mut g = TransactionGrouper::new(GroupProtocol::Uart);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(txs.len(), 2);
        assert_eq!(txs[0].data.len(), MAX_TRANSACTION_ITEMS);
        assert_eq!(
            txs[1].notes,
            vec!["continues the previous transaction".to_string()]
        );
    }

    #[test]
    fn can_groups_one_transaction_per_frame_and_folds_error_flags_in() {
        use AnnotationKind::*;
        let frame = |at: u64, id: u32, acked: bool, crc_ok: bool| {
            ann(
                at,
                at + 100,
                LEVEL_FRAME,
                CanFrame {
                    id,
                    extended: id > 0x7FF,
                    remote: false,
                    dlc: 2,
                    data: vec![0xAB, 0xCD],
                    crc_ok,
                    acked,
                },
            )
        };
        let anns = vec![
            ann(5, 6, LEVEL_FIELD, Data(0x99)), // fields alone make nothing
            frame(0, 0x123, true, true),
            // No ACK, then the transmitter's error flag.
            frame(200, 0x123, false, true),
            ann(300, 360, LEVEL_FRAME, CanErrorFrame),
            // A frame abandoned on a stuff error, and the flag that follows.
            ann(500, 540, LEVEL_FRAME, Error("stuff error".into())),
            ann(540, 600, LEVEL_FRAME, CanErrorFrame),
            frame(800, 0x1ABC_DEF0, true, false),
            // An error flag far from anything stands alone.
            ann(5000, 5060, LEVEL_FRAME, CanErrorFrame),
        ];
        let mut g = TransactionGrouper::new(GroupProtocol::Can);
        let mut txs = g.push(&anns);
        txs.extend(g.finish());
        assert_eq!(txs.len(), 5, "{txs:#?}");
        assert_eq!(txs[0].address, Some(0x123));
        assert_eq!(txs[0].data, vec![0xAB, 0xCD]);
        assert_eq!((txs[0].nacks, txs[0].errors), (0, 0));
        assert_eq!((txs[1].nacks, txs[1].errors), (1, 1));
        assert_eq!(txs[1].notes, vec!["no ACK", "error frame"]);
        assert_eq!(txs[1].end_sample, 360);
        assert_eq!(txs[2].address, None);
        assert_eq!(txs[2].notes, vec!["stuff error", "error frame"]);
        assert_eq!(txs[3].address, Some(0x1ABC_DEF0));
        assert_eq!(txs[3].notes, vec!["extended", "CRC error"]);
        assert_eq!(txs[4].notes, vec!["error frame"]);
        assert_eq!(txs[4].errors, 1);
    }

    #[test]
    fn text_and_hex_rendering() {
        assert_eq!(escaped_text(&[0x41, 0x00, 0x5C, 0xFF]), "A\\x00\\\\\\xFF");
        assert_eq!(hex_string(&[0x1, 0x1234]), "0001 1234");
        assert_eq!(hex_string(&[]), "");
    }

    /// B-88: a device that prints UTF-8 reads as text.
    #[test]
    fn utf8_shows_as_characters_and_anything_else_stays_escaped() {
        let bytes = |s: &str| s.bytes().map(u32::from).collect::<Vec<_>>();
        assert_eq!(escaped_text(&bytes("Grüße äöü €\r\n")), "Grüße äöü €\\r\\n");
        assert_eq!(escaped_text(&bytes("😀")), "😀");
        // A lead byte cut short, a stray continuation byte, an overlong
        // encoding, and a C1 control character (U+0085) stay escaped.
        assert_eq!(escaped_text(&[0x41, 0xC3]), "A\\xC3");
        assert_eq!(escaped_text(&[0xC3, 0x41]), "\\xC3A");
        assert_eq!(escaped_text(&[0xBC]), "\\xBC");
        assert_eq!(escaped_text(&[0xC0, 0x80]), "\\xC0\\x80");
        assert_eq!(escaped_text(&[0xC2, 0x85]), "\\xC2\\x85");
        // A 9-bit value is not a byte of a UTF-8 sequence.
        assert_eq!(escaped_text(&[0xC3, 0x1BC]), "\\xC3\\xBC");
    }
}
