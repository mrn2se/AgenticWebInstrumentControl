//! Rendering a decoder's [`Annotation`] stream as terminal text — the
//! `decode` terminal source (SPECIFICATION.md §11.2): "UART bytes as text,
//! I²C and SPI transactions as one line each."
//!
//! These are pure functions over an annotation slice, not part of the
//! [`Decoder`](crate::Decoder) trait: the terminal is one consumer among
//! several (graph annotations, export), and rendering has no state of its
//! own beyond the annotations already produced.

use crate::{Annotation, AnnotationKind, LEVEL_FRAME};

/// UART as the raw data bytes, with bracketed markers for the conditions
/// that aren't a byte: framing/parity errors and breaks. A 9-bit data word
/// (SPEC §6 allows 5-9) is truncated to its low 8 bits — a terminal has no
/// way to show a wider byte anyway.
pub fn render_uart(annotations: &[Annotation]) -> Vec<u8> {
    let mut out = Vec::new();
    for ann in annotations {
        match &ann.kind {
            AnnotationKind::Data(b) => out.push((*b & 0xFF) as u8),
            AnnotationKind::FramingError => out.extend_from_slice(b"<FE>"),
            AnnotationKind::ParityError => out.extend_from_slice(b"<PE>"),
            AnnotationKind::Break => out.extend_from_slice(b"<BREAK>"),
            AnnotationKind::Error(_) => out.extend_from_slice(b"<ERR>"),
            _ => {}
        }
    }
    out
}

/// I²C as one line per transaction, e.g.
/// `S 0x48 W A 0x01 A Sr 0x48 R A 0x9C N P`. A transaction runs from a
/// `Start` to the matching `Stop`; a `RepeatedStart` continues the same
/// line rather than starting a new one. Every annotation's own
/// [`Annotation::text`] is already the right token (`S`, `Sr`, `P`, `A`,
/// `N`, `"0x48 W"`, `"0x01"`, ...), so rendering is just grouping them.
pub fn render_i2c(annotations: &[Annotation]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for ann in annotations {
        match &ann.kind {
            AnnotationKind::Start => {
                if !current.is_empty() {
                    lines.push(current.join(" "));
                    current.clear();
                }
                current.push(ann.text.clone());
            }
            AnnotationKind::Stop => {
                current.push(ann.text.clone());
                lines.push(current.join(" "));
                current.clear();
            }
            AnnotationKind::Incomplete => current.push(format!("[{}]", ann.text)),
            AnnotationKind::Error(msg) => current.push(format!("[{msg}]")),
            _ => current.push(ann.text.clone()),
        }
    }
    if !current.is_empty() {
        lines.push(current.join(" "));
    }
    lines
}

/// SPI as one line per CS-framed transfer, or one line per word when no CS
/// is configured (no `ChipSelect` annotations appear in that case).
pub fn render_spi(annotations: &[Annotation]) -> Vec<String> {
    let has_cs = annotations
        .iter()
        .any(|a| matches!(a.kind, AnnotationKind::ChipSelect { .. }));
    let mut lines = Vec::new();
    let mut mosi: Vec<Option<u32>> = Vec::new();
    let mut miso: Vec<Option<u32>> = Vec::new();
    let mut open = false;
    for ann in annotations {
        match &ann.kind {
            AnnotationKind::ChipSelect { active: true } => {
                open = true;
                mosi.clear();
                miso.clear();
            }
            AnnotationKind::ChipSelect { active: false } => {
                if open && !(mosi.is_empty() && miso.is_empty()) {
                    lines.push(format_words(&mosi, &miso));
                }
                open = false;
            }
            AnnotationKind::Word { mosi: m, miso: s } => {
                if has_cs {
                    mosi.push(*m);
                    miso.push(*s);
                } else {
                    lines.push(format_words(&[*m], &[*s]));
                }
            }
            _ => {}
        }
    }
    // A transfer still open when the stream ends (e.g. `finish()` was
    // called mid-transfer): show what was clocked in rather than drop it.
    if open && !(mosi.is_empty() && miso.is_empty()) {
        lines.push(format_words(&mosi, &miso));
    }
    lines
}

/// CAN as one candump-style line per frame, e.g.
/// `  can  123   [8]  DE AD BE EF 01 02 03 04` (identifiers padded to three
/// hex digits, eight for an extended frame), with `  NO-ACK` and `  CRC-ERR`
/// flags after it, `remote` in place of the data of a remote frame, and
/// `ERROR FRAME`, `STUFF ERROR` and the like for what was not a frame.
pub fn render_can(annotations: &[Annotation]) -> Vec<String> {
    let mut lines = Vec::new();
    for ann in annotations.iter().filter(|a| a.level == LEVEL_FRAME) {
        match &ann.kind {
            AnnotationKind::CanFrame {
                id,
                extended,
                remote,
                dlc,
                data,
                crc_ok,
                acked,
            } => {
                let id = if *extended {
                    format!("{id:08X}")
                } else {
                    format!("{id:03X}")
                };
                let mut line = format!("  can  {id}   [{dlc}]");
                if *remote {
                    line.push_str("  remote");
                } else {
                    let bytes: Vec<String> = data.iter().map(|b| format!("{b:02X}")).collect();
                    line.push_str(&format!("  {}", bytes.join(" ")));
                }
                if !acked {
                    line.push_str("  NO-ACK");
                }
                if !crc_ok {
                    line.push_str("  CRC-ERR");
                }
                lines.push(line);
            }
            AnnotationKind::CanErrorFrame => lines.push("  can  ERROR FRAME".to_string()),
            AnnotationKind::Error(msg) => lines.push(format!("  can  {}", msg.to_uppercase())),
            AnnotationKind::Incomplete => lines.push("  can  INCOMPLETE".to_string()),
            _ => {}
        }
    }
    lines
}

/// 1-Wire as one line per reset, e.g.
/// `reset  Match ROM  28-00000A1B2C3D  44` — the reset (with `NO-PRESENCE`
/// when no device answered, `overdrive` when it was timed at overdrive
/// speed), the ROM command, the ROM code it carried (`CRC-ERR` after a bad
/// one), then the data bytes as hex. Errors and incomplete ends appear in
/// brackets where they happened.
pub fn render_onewire(annotations: &[Annotation]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for ann in annotations {
        match &ann.kind {
            AnnotationKind::OneWireReset {
                presence,
                overdrive,
            } => {
                if !current.is_empty() {
                    lines.push(current.join("  "));
                    current.clear();
                }
                let mut reset = "reset".to_string();
                if *overdrive {
                    reset.push_str(" overdrive");
                }
                if !presence {
                    reset.push_str(" NO-PRESENCE");
                }
                current.push(reset);
            }
            AnnotationKind::OneWireRomCommand(_) => current.push(ann.text.clone()),
            AnnotationKind::OneWireRom { rom, crc_ok } => {
                let mut text = crate::onewire::format_rom(*rom);
                if !crc_ok {
                    text.push_str(" CRC-ERR");
                }
                current.push(text);
            }
            AnnotationKind::Data(b) => current.push(format!("{b:02X}")),
            AnnotationKind::Incomplete | AnnotationKind::Error(_) => {
                current.push(format!("[{}]", ann.text))
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        lines.push(current.join("  "));
    }
    lines
}

fn format_words(mosi: &[Option<u32>], miso: &[Option<u32>]) -> String {
    let fmt = |vals: &[Option<u32>]| {
        vals.iter()
            .map(|v| v.map_or("--".to_string(), |x| format!("{x:02X}")))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut parts = Vec::new();
    if mosi.iter().any(Option::is_some) {
        parts.push(format!("MOSI: {}", fmt(mosi)));
    }
    if miso.iter().any(Option::is_some) {
        parts.push(format!("MISO: {}", fmt(miso)));
    }
    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u32, extended: bool, remote: bool, data: &[u8], acked: bool) -> Annotation {
        Annotation::new(
            0,
            100,
            LEVEL_FRAME,
            AnnotationKind::CanFrame {
                id,
                extended,
                remote,
                dlc: data.len() as u8,
                data: data.to_vec(),
                crc_ok: true,
                acked,
            },
        )
    }

    #[test]
    fn can_lines_look_like_candump() {
        let anns = vec![
            frame(
                0x123,
                false,
                false,
                &[0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4],
                false,
            ),
            frame(0x1ABC_DEF0, true, false, &[1], true),
            frame(0x7FF, false, true, &[0; 2], true),
            Annotation::new(0, 10, LEVEL_FRAME, AnnotationKind::CanErrorFrame),
            Annotation::new(
                0,
                10,
                LEVEL_FRAME,
                AnnotationKind::Error("stuff error".into()),
            ),
            Annotation::new(0, 1, 1, AnnotationKind::Data(7)),
        ];
        assert_eq!(
            render_can(&anns),
            [
                "  can  123   [8]  DE AD BE EF 01 02 03 04  NO-ACK",
                "  can  1ABCDEF0   [1]  01",
                "  can  7FF   [2]  remote",
                "  can  ERROR FRAME",
                "  can  STUFF ERROR",
            ]
        );
    }

    #[test]
    fn onewire_lines_start_at_each_reset() {
        use crate::{LEVEL_BIT, LEVEL_FIELD};
        let reset = |presence| {
            Annotation::new(
                0,
                1,
                LEVEL_FRAME,
                AnnotationKind::OneWireReset {
                    presence,
                    overdrive: false,
                },
            )
        };
        let anns = vec![
            reset(true),
            Annotation::new(0, 1, LEVEL_BIT, AnnotationKind::OneWireBit(true)),
            Annotation::new(0, 1, LEVEL_FIELD, AnnotationKind::OneWireRomCommand(0x55)),
            Annotation::new(
                0,
                1,
                LEVEL_FIELD,
                AnnotationKind::OneWireRom {
                    rom: 0xA200_000A_1B2C_3D28,
                    crc_ok: false,
                },
            ),
            Annotation::new(0, 1, LEVEL_FIELD, AnnotationKind::Data(0x44)),
            reset(false),
        ];
        assert_eq!(
            render_onewire(&anns),
            [
                "reset  Match ROM  28-00000A1B2C3D CRC-ERR  44",
                "reset NO-PRESENCE",
            ]
        );
    }
}
