//! [`Annotation`] and its nesting levels (SPEC §6).

/// A whole protocol unit: a UART character frame, an I²C transaction from
/// START to STOP, an SPI CS-framed transfer, a CAN frame, a 1-Wire reset.
pub const LEVEL_FRAME: u8 = 0;
/// A field within a frame: an I²C address or data byte, an SPI word, a CAN
/// ID, DLC, data byte or CRC, a 1-Wire ROM command, ROM code or byte.
pub const LEVEL_FIELD: u8 = 1;
/// A single bit within a field: an I²C ACK/NACK bit, a CAN stuff bit, a
/// 1-Wire time slot.
pub const LEVEL_BIT: u8 = 2;

/// One decoded unit, positioned by sample index (D-10) so it lines up with
/// the trace regardless of transport. `level` says how annotations nest —
/// frame → field → bit — so a UI can stack them under the waveform; nesting
/// is by sample-range containment, there is no explicit parent link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub start_sample: u64,
    pub end_sample: u64,
    pub level: u8,
    pub kind: AnnotationKind,
    pub text: String,
}

impl Annotation {
    pub(crate) fn new(start: u64, end: u64, level: u8, kind: AnnotationKind) -> Self {
        let text = kind.default_text();
        Annotation {
            start_sample: start,
            end_sample: end,
            level,
            kind,
            text,
        }
    }
}

/// What an [`Annotation`] represents. Shared across the five decoders
/// rather than one enum per protocol: annotations are consumed generically
/// (rendering, export, the graph's stacked lanes) and most variants are
/// meaningful across more than one protocol (`Error`, `Incomplete`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationKind {
    // -- UART --
    /// One decoded character, value already reassembled per bit order.
    /// `u16`, not `u8`: UART frames may carry up to 9 data bits (SPEC §6),
    /// one more than a byte holds.
    Data(u16),
    /// A stop bit sampled low.
    FramingError,
    /// The parity bit didn't match the data bits' parity.
    ParityError,
    /// The line was held at the space level for a whole frame including
    /// where the stop bit should have been — longer than any real frame.
    Break,

    // -- I2C --
    Start,
    /// A START seen without an intervening STOP is a repeated start (SPEC
    /// §6 I²C bullet), not a bus error.
    RepeatedStart,
    Stop,
    /// A 7-bit or combined 10-bit address plus the R/W bit that followed it.
    Address {
        address: u16,
        read: bool,
    },
    Ack,
    Nack,

    // -- SPI --
    /// Chip-select becoming active or inactive, framing a transfer (only
    /// emitted when a CS line is configured).
    ChipSelect {
        active: bool,
    },
    /// One clocked word. Either side is `None` when that line isn't wired
    /// (SPEC §6: MOSI/MISO either optional).
    Word {
        mosi: Option<u32>,
        miso: Option<u32>,
    },

    // -- CAN --
    /// A stuff bit: the opposite-level bit inserted after five equal bits,
    /// carrying no data.
    StuffBit,
    /// The identifier field: 11 bits, or 29 for an extended frame.
    CanId {
        id: u32,
        extended: bool,
    },
    /// The RTR bit: a remote frame requests data and carries none itself.
    CanRtr {
        remote: bool,
    },
    /// The IDE bit: recessive in an extended (29-bit) frame.
    CanIde {
        extended: bool,
    },
    /// The 4-bit data length code, as sent (9..=15 mean 8 bytes in classic CAN).
    CanDlc(u8),
    /// The CRC-15 sequence and whether it matched the one computed over the
    /// received bits.
    CanCrc {
        value: u16,
        ok: bool,
    },
    /// The seven recessive end-of-frame bits.
    CanEof,
    /// A whole CAN frame, SOF through EOF (or through the last bit before an
    /// error flag cut it short). `acked` is whether some node pulled the ACK
    /// slot dominant.
    CanFrame {
        id: u32,
        extended: bool,
        remote: bool,
        dlc: u8,
        data: Vec<u8>,
        crc_ok: bool,
        acked: bool,
    },
    /// An error flag (or overload flag) outside a frame: six or more
    /// dominant bits in a row.
    CanErrorFrame,

    // -- 1-Wire --
    /// A reset pulse, through the presence pulse that answered it if one did.
    /// `overdrive`: the reset was timed at overdrive speed.
    OneWireReset {
        presence: bool,
        overdrive: bool,
    },
    /// One time slot's bit, written by the master or read from a slave (the
    /// line looks the same either way).
    OneWireBit(bool),
    /// The ROM command, the first byte after a reset.
    OneWireRomCommand(u8),
    /// A 64-bit ROM code — family code in the low byte, then the 48-bit
    /// serial, CRC-8 in the high byte — and whether that CRC matched.
    OneWireRom {
        rom: u64,
        crc_ok: bool,
    },

    // -- shared --
    /// `finish()` flushed a frame/transaction that never completed.
    Incomplete,
    /// A configuration or protocol condition a decoder can't make sense of.
    Error(String),
}

impl AnnotationKind {
    fn default_text(&self) -> String {
        match self {
            AnnotationKind::Data(b) => format!("0x{b:02X}"),
            AnnotationKind::FramingError => "framing error".to_string(),
            AnnotationKind::ParityError => "parity error".to_string(),
            AnnotationKind::Break => "break".to_string(),
            AnnotationKind::Start => "S".to_string(),
            AnnotationKind::RepeatedStart => "Sr".to_string(),
            AnnotationKind::Stop => "P".to_string(),
            AnnotationKind::Address { address, read } => {
                let rw = if *read { "R" } else { "W" };
                format!("0x{address:02X} {rw}")
            }
            AnnotationKind::Ack => "A".to_string(),
            AnnotationKind::Nack => "N".to_string(),
            AnnotationKind::ChipSelect { active } => {
                if *active {
                    "CS active".to_string()
                } else {
                    "CS idle".to_string()
                }
            }
            AnnotationKind::Word { mosi, miso } => {
                let m = mosi.map_or("--".to_string(), |v| format!("0x{v:X}"));
                let s = miso.map_or("--".to_string(), |v| format!("0x{v:X}"));
                format!("MOSI={m} MISO={s}")
            }
            AnnotationKind::StuffBit => "stuff".to_string(),
            AnnotationKind::CanId { id, extended } => {
                if *extended {
                    format!("ID 0x{id:08X} ext")
                } else {
                    format!("ID 0x{id:03X}")
                }
            }
            AnnotationKind::CanRtr { remote } => {
                if *remote {
                    "RTR".to_string()
                } else {
                    "data".to_string()
                }
            }
            AnnotationKind::CanIde { extended } => {
                if *extended {
                    "IDE ext".to_string()
                } else {
                    "IDE base".to_string()
                }
            }
            AnnotationKind::CanDlc(dlc) => format!("DLC {dlc}"),
            AnnotationKind::CanCrc { value, ok } => {
                if *ok {
                    format!("CRC 0x{value:04X}")
                } else {
                    format!("CRC 0x{value:04X} bad")
                }
            }
            AnnotationKind::CanEof => "EOF".to_string(),
            AnnotationKind::CanFrame {
                id,
                extended,
                remote,
                dlc,
                data,
                crc_ok,
                acked,
            } => {
                let mut text = if *extended {
                    format!("0x{id:08X} ext")
                } else {
                    format!("0x{id:03X}")
                };
                if *remote {
                    text.push_str(" RTR");
                }
                text.push_str(&format!(" [{dlc}]"));
                for byte in data {
                    text.push_str(&format!(" {byte:02X}"));
                }
                if !acked {
                    text.push_str(" no ACK");
                }
                if !crc_ok {
                    text.push_str(" CRC error");
                }
                text
            }
            AnnotationKind::CanErrorFrame => "error frame".to_string(),
            AnnotationKind::OneWireReset {
                presence,
                overdrive,
            } => {
                let mut text = "reset".to_string();
                if *overdrive {
                    text.push_str(" (overdrive)");
                }
                if !presence {
                    text.push_str(", no presence");
                }
                text
            }
            AnnotationKind::OneWireBit(bit) => u8::from(*bit).to_string(),
            AnnotationKind::OneWireRomCommand(code) => {
                match crate::onewire::rom_command_name(*code) {
                    Some(name) => name.to_string(),
                    None => format!("ROM command 0x{code:02X}?"),
                }
            }
            AnnotationKind::OneWireRom { rom, crc_ok } => {
                let mut text = format!("ROM {}", crate::onewire::format_rom(*rom));
                if !crc_ok {
                    text.push_str(" CRC error");
                }
                text
            }
            AnnotationKind::Incomplete => "incomplete".to_string(),
            AnnotationKind::Error(msg) => msg.clone(),
        }
    }
}
