//! Protocol decoder wire types (SPECIFICATION.md §6, §8.2, §9): the JSON
//! shapes for `capture.decode`, `rec.decode` and `decode.list`, plus
//! [`DecodeOpenSpec`] which `Request::TermOpen` carries for the `decode`
//! terminal source (§11.2).
//!
//! Kept separate from `awic-decode` (which knows nothing of JSON or ts-rs —
//! DEVELOPMENT.md §2's "no dependencies beyond `std`") and from
//! `awic-decode`'s own [`awic_decode::ChannelRole`]/`DecoderOption` (metadata
//! for a UI, not wire types): the server (`awic-server/src/decode.rs`)
//! converts between the two. `options` avoids `serde_json::Value` (not a
//! `ts-rs` type here) in favour of a small typed [`OptionValue`], so a
//! decoder option is exactly as strongly typed on the wire as everything
//! else in this crate.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// `max_annotations` default for `capture.decode`/`rec.decode`/the `decode`
/// terminal source, when the caller doesn't ask for a specific bound (SPEC
/// T21: "a `max_annotations` default and a hard cap"). Generous enough for a
/// typical UI view (a few hundred UART bytes, or I²C/SPI transactions) while
/// staying well inside the context-budget spirit of §8.3.
pub const DEFAULT_MAX_ANNOTATIONS: u32 = 500;
/// Hard ceiling regardless of what a caller asks for.
pub const HARD_MAX_ANNOTATIONS: u32 = 5_000;
/// `max_bytes` default: a UART burst (or a CS-less SPI burst) holding more
/// data bytes is continued in the next transaction, so one entry stays a few
/// KB however long the line talks without a pause (B-97: a 1999-byte burst
/// came back as one ~10 KB entry, its bytes once as hex and once as text).
pub const DEFAULT_MAX_TRANSACTION_BYTES: u32 = 256;

/// Which protocol decoder (SPEC §6: "v1 scope: I²C, SPI, UART", plus CAN and
/// 1-Wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum DecoderId {
    Uart,
    I2c,
    Spi,
    Can,
    OneWire,
}

/// The wire form of `awic_decode::ChannelRole`: one logical signal a decoder
/// needs bound to a hardware channel (SPEC §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum ChannelRole {
    Rx,
    Scl,
    Sda,
    Clk,
    Mosi,
    Miso,
    Cs,
    /// CAN's one bus line.
    Can,
    /// 1-Wire's one open-drain line.
    Owr,
}

/// One entry of a `channels` mapping: which hardware channel plays a given
/// role. A `Vec` rather than a map keyed by role, so the wire shape stays a
/// plain JSON array a form can build incrementally and ts-rs can type
/// exactly (no `Record<string, T>` ambiguity over enum keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct ChannelMapping {
    pub role: ChannelRole,
    pub channel: u8,
    /// Differential line (analog captures only): the role's line is
    /// `channel - minus`, e.g. CANH as `channel` and CANL as `minus`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[ts(optional)]
    pub minus: Option<u8>,
}

/// One decoder construction option's value, as supplied by a caller. Untagged
/// so a client sends exactly the JSON value that matches the option's
/// declared type (SPEC's "types, defaults, allowed values"): `115200`,
/// `true`, or `"even"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(untagged)]
#[ts(export, export_to = "types.ts")]
pub enum OptionValue {
    Int(i64),
    Bool(bool),
    Text(String),
}

/// One option setting in a `capture.decode`/`rec.decode`/decode-source
/// request. Options not supplied take the default `decode.list` reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct OptionSetting {
    pub id: String,
    pub value: OptionValue,
}

/// One decoder construction option's metadata (SPEC §21/T21: "types,
/// defaults, allowed values, so a UI or agent can build a form without
/// hardcoding"). Tagged on `type` so a client can render the right control
/// (a number field, a checkbox, a select) with no per-decoder special case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum DecoderOptionSpec {
    Int {
        id: String,
        description: String,
        default: i64,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        min: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        max: Option<i64>,
    },
    Bool {
        id: String,
        description: String,
        default: bool,
    },
    Enum {
        id: String,
        description: String,
        default: String,
        values: Vec<String>,
    },
}

impl DecoderOptionSpec {
    /// The option's id, whatever its type.
    pub fn id(&self) -> &str {
        match self {
            Self::Int { id, .. } | Self::Bool { id, .. } | Self::Enum { id, .. } => id,
        }
    }
}

/// One decoder `decode.list` reports: its id, the channel roles it needs
/// bound, and its options — everything a UI needs to build a decode form
/// with no hardcoded knowledge of any particular protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct DecoderInfo {
    pub id: DecoderId,
    pub name: String,
    pub channels: Vec<ChannelRole>,
    pub options: Vec<DecoderOptionSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct DecodeListResult {
    pub decoders: Vec<DecoderInfo>,
}

/// The wire form of `awic_decode::AnnotationKind` (SPEC §6). Tagged on `kind`
/// so the UI can switch on it directly instead of re-parsing
/// [`DecodedAnnotation::text`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum AnnotationKindWire {
    Data {
        value: u16,
    },
    FramingError,
    ParityError,
    Break,
    Start,
    RepeatedStart,
    Stop,
    Address {
        address: u16,
        read: bool,
    },
    Ack,
    Nack,
    ChipSelect {
        active: bool,
    },
    Word {
        mosi: Option<u32>,
        miso: Option<u32>,
    },
    StuffBit,
    CanId {
        id: u32,
        extended: bool,
    },
    CanRtr {
        remote: bool,
    },
    CanIde {
        extended: bool,
    },
    CanDlc {
        dlc: u8,
    },
    CanCrc {
        value: u16,
        ok: bool,
    },
    CanEof,
    CanFrame {
        id: u32,
        extended: bool,
        remote: bool,
        dlc: u8,
        data: Vec<u8>,
        crc_ok: bool,
        acked: bool,
    },
    CanErrorFrame,
    OneWireReset {
        presence: bool,
        overdrive: bool,
    },
    OneWireBit {
        value: bool,
    },
    OneWireRomCommand {
        code: u8,
    },
    /// `rom` named as Linux's w1 names a device: family code and the 48-bit
    /// serial (`"28-00000A1B2C3D"`); `crc` is the code's last byte. A string
    /// because the 64-bit code does not fit a JSON number exactly.
    OneWireRom {
        rom: String,
        crc: u8,
        crc_ok: bool,
    },
    Incomplete,
    Error {
        message: String,
    },
}

/// One decoded annotation (SPEC §6): `level` nests frame (0) -> field (1) ->
/// bit (2) by sample-range containment, exactly as `awic_decode::Annotation`
/// documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct DecodedAnnotation {
    pub start_sample: u64,
    pub end_sample: u64,
    pub level: u8,
    pub kind: AnnotationKindWire,
    pub text: String,
}

/// The shape of a `capture.decode`/`rec.decode` result (live test F-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum DecodeFormat {
    /// Every annotation (frames, fields, bits) — what the graph draws.
    #[default]
    Annotations,
    /// One entry per I²C START..STOP, UART burst or SPI CS window, its bytes
    /// as hex — what an agent reads. `max_annotations` then bounds the
    /// number of transactions.
    Transactions,
}

/// One transaction (live test F-02): the unit an agent reasons about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct DecodedTransaction {
    pub start_sample: u64,
    pub end_sample: u64,
    /// Seconds from the capture's or recording's start.
    pub t_s: f64,
    /// I²C: the address and direction it was addressed with. CAN: the frame's
    /// identifier (`u32`: an extended one has 29 bits). 1-Wire: the family
    /// code of the ROM code the transaction carried (the full code is in
    /// `notes`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub address: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub read: Option<bool>,
    /// I²C: begun by a repeated START, continuing the previous transaction.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub repeated_start: bool,
    /// UART bytes, I²C data bytes after the address, SPI MOSI words, CAN data
    /// bytes, 1-Wire bytes after the ROM command and ROM code — as
    /// space-separated hex (`"40 00 1F"`).
    pub data: String,
    /// Number of items in `data`.
    pub count: u32,
    /// SPI MISO words, as hex.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub miso: Option<String>,
    /// UART: the bytes as text, non-printables escaped (`"OK\r\n"`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
    /// I²C and CAN only: `true` when every byte of the transaction (address
    /// and data) was ACKed, `false` when any was NACKed — for CAN, when no
    /// node acknowledged the frame. Explicit so "all ACKed" is read, not
    /// inferred from a missing `notes` entry (B-43). Absent for a CAN entry
    /// that is not a frame (a stuff error, a lone error flag). 1-Wire: whether
    /// a device answered the reset with a presence pulse; absent for slots
    /// seen before any reset.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub acked: Option<bool>,
    /// NACKs, framing/parity errors, breaks, an incomplete end.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
    /// F-14: this many identical transactions in a row (same address,
    /// direction, data, acknowledgement and notes) folded into this entry;
    /// absent for one. `start_sample`/`t_s` are the first's, `end_sample`
    /// the last's.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[ts(optional)]
    pub repeat: Option<u32>,
}

/// F-14: how the transactions fall into bursts — runs of transactions
/// separated by idle gaps far longer than the gaps inside them. Present when
/// there are at least two bursts; medians, so one odd burst does not move
/// them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct BurstSummary {
    pub count: u32,
    /// Transactions per burst (repeats counted), fewest and most.
    pub transactions_min: u32,
    pub transactions_max: u32,
    /// Median time from one burst's start to the next's, seconds.
    pub period_s: f64,
    /// Median burst duration, seconds.
    pub duration_s: f64,
    /// The idle gap that separates bursts: anything longer ends one,
    /// seconds.
    pub gap_s: f64,
}

/// Per-address share of a [`TransactionSummary`] (I²C addresses, CAN IDs,
/// 1-Wire family codes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct AddressSummary {
    /// 7-bit address as the decoder reports it, or the CAN identifier (the
    /// `address` of each transaction).
    pub address: u32,
    pub transactions: u32,
    pub nacks: u32,
}

/// Totals over the `transactions` returned (B-43): an agent reads "0 NACKs"
/// here instead of inferring it from the absence of notes. When `truncated`
/// is true they cover only what was returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct TransactionSummary {
    /// Every transaction, repeats folded into an entry counted one by one.
    pub transactions: u32,
    /// How many of them were folded into an earlier entry's `repeat` (F-14):
    /// `transactions - folded` entries were returned.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub folded: u32,
    /// I²C NACKs (address or data byte), CAN frames nobody acknowledged,
    /// 1-Wire resets no device answered; always 0 for UART and SPI.
    pub nacks: u32,
    /// Framing/parity errors, breaks, decoder errors and incomplete ends.
    pub errors: u32,
    /// I²C, CAN and 1-Wire: transactions and NACKs per address, ID or family
    /// code, ascending.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub by_address: Vec<AddressSummary>,
    /// F-14: the burst structure, when the transactions come in bursts.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[ts(optional)]
    pub bursts: Option<BurstSummary>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct CaptureDecodeResult {
    pub capture: String,
    pub decoder: DecoderId,
    /// `format: "annotations"` (the default) only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub annotations: Option<Vec<DecodedAnnotation>>,
    /// `format: "transactions"` only — present, if empty, when nothing was
    /// decoded (B-84).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub transactions: Option<Vec<DecodedTransaction>>,
    /// `format: "transactions"` only: totals over `transactions`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<TransactionSummary>,
    /// `true` if `max_annotations` was reached before the capture was fully
    /// decoded (SPEC T21: "say how many were omitted").
    pub truncated: bool,
    pub omitted: u64,
    /// UART with `baud` (or CAN with `bitrate`) set to `"auto"`: the rate
    /// detected and used.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detected_baud: Option<u32>,
    /// Analog captures only: the thresholds the waveform was cut with, e.g.
    /// `"analog: CH1 threshold 0.420 V, hysteresis 0.080 V"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecDecodeResult {
    pub rec: u16,
    pub decoder: DecoderId,
    /// `format: "annotations"` (the default) only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub annotations: Option<Vec<DecodedAnnotation>>,
    /// `format: "transactions"` only — present, if empty, when nothing was
    /// decoded (B-84).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub transactions: Option<Vec<DecodedTransaction>>,
    /// `format: "transactions"` only: totals over `transactions`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<TransactionSummary>,
    pub truncated: bool,
    pub omitted: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detected_baud: Option<u32>,
    /// The range actually decoded, resolved against the retained span when
    /// either bound was omitted — same convention as `RecSearchResult::searched`.
    pub decoded: crate::SearchedRange,
    /// Set when the recording is no longer live: when it ended and why, so
    /// a result that stops short of "now" says so (live test B-24).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}

/// What the `decode` terminal source (SPEC §11.2) is opened with: a
/// recording plus a decoder configuration, exactly as `capture.decode`/
/// `rec.decode` take one, so the three surfaces (graph, terminal, MCP) share
/// one notion of "a decoder instance".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct DecodeOpenSpec {
    pub rec: u16,
    pub decoder: DecoderId,
    pub channels: Vec<ChannelMapping>,
    #[serde(default)]
    pub options: Vec<OptionSetting>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn option_value_serializes_untagged() {
        assert_eq!(
            serde_json::to_value(OptionValue::Int(115_200)).unwrap(),
            json!(115200)
        );
        assert_eq!(
            serde_json::to_value(OptionValue::Bool(true)).unwrap(),
            json!(true)
        );
        assert_eq!(
            serde_json::to_value(OptionValue::Text("even".into())).unwrap(),
            json!("even")
        );
    }

    #[test]
    fn decoder_option_spec_tags_on_type() {
        let spec = DecoderOptionSpec::Enum {
            id: "parity".into(),
            description: "none | even | odd".into(),
            default: "none".into(),
            values: vec!["none".into(), "even".into(), "odd".into()],
        };
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(value["type"], "enum");
        assert_eq!(value["default"], "none");
    }

    #[test]
    fn annotation_kind_round_trips() {
        for kind in [
            AnnotationKindWire::Data { value: 0x41 },
            AnnotationKindWire::Address {
                address: 0x50,
                read: false,
            },
            AnnotationKindWire::Word {
                mosi: Some(0xA5),
                miso: None,
            },
            AnnotationKindWire::Error {
                message: "false start bit".into(),
            },
            AnnotationKindWire::CanFrame {
                id: 0x1ABC_DEF0,
                extended: true,
                remote: false,
                dlc: 2,
                data: vec![0xAB, 0xCD],
                crc_ok: true,
                acked: false,
            },
            AnnotationKindWire::CanErrorFrame,
        ] {
            let value = serde_json::to_value(&kind).unwrap();
            let back: AnnotationKindWire = serde_json::from_value(value).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn onewire_wire_names() {
        assert_eq!(
            serde_json::to_value(DecoderId::OneWire).unwrap(),
            json!("one_wire")
        );
        assert_eq!(
            serde_json::to_value(ChannelRole::Owr).unwrap(),
            json!("owr")
        );
        let value = serde_json::to_value(AnnotationKindWire::OneWireRom {
            rom: "28-00000A1B2C3D".into(),
            crc: 0xA2,
            crc_ok: true,
        })
        .unwrap();
        assert_eq!(
            value,
            json!({ "kind": "one_wire_rom", "rom": "28-00000A1B2C3D", "crc": 0xA2, "crc_ok": true })
        );
    }

    #[test]
    fn can_wire_names() {
        assert_eq!(serde_json::to_value(DecoderId::Can).unwrap(), json!("can"));
        assert_eq!(
            serde_json::to_value(ChannelRole::Can).unwrap(),
            json!("can")
        );
        let value = serde_json::to_value(AnnotationKindWire::CanId {
            id: 0x123,
            extended: false,
        })
        .unwrap();
        assert_eq!(
            value,
            json!({ "kind": "can_id", "id": 0x123, "extended": false })
        );
    }

    #[test]
    fn channel_mapping_round_trips() {
        let json = json!({ "role": "scl", "channel": 0 });
        let mapping: ChannelMapping = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            mapping,
            ChannelMapping {
                role: ChannelRole::Scl,
                channel: 0,
                minus: None,
            }
        );
        assert_eq!(serde_json::to_value(mapping).unwrap(), json);
        let diff = json!({ "role": "rx", "channel": 0, "minus": 1 });
        let mapping: ChannelMapping = serde_json::from_value(diff.clone()).unwrap();
        assert_eq!(mapping.minus, Some(1));
        assert_eq!(serde_json::to_value(mapping).unwrap(), diff);
    }
}
