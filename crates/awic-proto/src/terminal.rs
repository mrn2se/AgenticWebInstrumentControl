//! Terminal module wire types (SPECIFICATION.md §11.2/§11.2.1): the JSON
//! shapes carried by the `term.*` ops, referenced from [`crate::control`]'s
//! `Request`/`ResponseResult`/`Event` enums. The binary output frame (magic
//! `0xA7`) lives in [`crate::term_frame`], mirroring how sample frames
//! (`0xA5`) sit in [`crate::frame`] beside the JSON control types.
//!
//! A terminal *source* is anything the session manager can open a
//! full-duplex (or read-only, for `log`) byte stream against — a serial
//! port, the server's own log, and later `scpi`/`decode` sources that plug
//! into the same session manager without changing these types.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What kind of thing a [`TerminalSourceInfo`] is. New kinds (`scpi`,
/// `decode`) are additive — tagged so old clients ignore what they don't
/// understand rather than choking on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum TerminalKind {
    /// A USB-serial interface (SPEC §11.2.1): `source: "serial:<intf>"`.
    Serial,
    /// The server's own structured log (SPEC §12): one fixed source, `id: "log"`.
    Log,
    /// An interactive console on AWIC's own SCPI engine (SPEC §9, §11.2):
    /// one fixed source, `id: "scpi"`, always available.
    Scpi,
    /// Live protocol-decoder output on a recording (SPEC §6, §11.2): one
    /// fixed picker entry, `id: "decode"`, opened with a
    /// [`crate::DecodeOpenSpec`]. Never shared/joined like `serial` — each
    /// open mints its own session, since two opens name different decoder
    /// configurations even when both watch the same recording.
    Decode,
}

/// One terminal source as reported by `term.sources` (SPEC §11.2's example):
/// discovered fresh each call (serial ports come and go with the bus),
/// never cached client-side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct TerminalSourceInfo {
    /// Stable id across appearances of the same interface, e.g.
    /// `"serial:3-3.4.4:1.1"` or `"log"` — what `term.open` takes.
    pub source: String,
    pub kind: TerminalKind,
    /// Human-readable, e.g. `"ttyACM0 — Raspberry Pi Debug Probe"`.
    pub label: String,
    /// The USB device's own bus path (e.g. `"3-3.4.4"`, without the
    /// interface suffix `source` carries) for `serial` sources — `None` for
    /// `log`. This is the join key the USB tree module uses to put a
    /// terminal icon on the right port (SPEC §11.2.1) without guessing at
    /// `source`'s structure.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub device: Option<String>,
    /// Whether `term.open` can succeed right now. `false` while the tty node
    /// exists but permissions haven't been granted yet, or while a udev
    /// race is still settling (SPEC §11.2.1) — the UI disables the source
    /// with [`Self::unavailable_reason`] rather than hiding it.
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unavailable_reason: Option<String>,
}

/// UART parity (SPEC §11.2.1's baud/data-bits/parity/stop-bits settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum Parity {
    None,
    Even,
    Odd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum StopBits {
    One,
    Two,
}

/// The frame format a `serial` source is opened with. Ignored (and refused,
/// see `term_open_settings_required_for_serial` server-side) for non-serial
/// sources such as `log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SerialSettings {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
}

/// Lifecycle of one open terminal session (SPEC §11.2.1): `waiting` is a
/// serial session whose device left the bus — scrollback is kept, and the
/// session returns to `open` on its own if the same interface reappears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum TerminalState {
    Open,
    Waiting,
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terminal_source_info_omits_unavailable_reason_when_available() {
        let info = TerminalSourceInfo {
            source: "log".into(),
            kind: TerminalKind::Log,
            label: "Server log".into(),
            device: None,
            available: true,
            unavailable_reason: None,
        };
        let value = serde_json::to_value(&info).unwrap();
        assert!(value.get("unavailable_reason").is_none());
        assert!(value.get("device").is_none());
        assert_eq!(value["kind"], "log");
    }

    #[test]
    fn serial_settings_round_trips() {
        let json = json!({ "baud": 115200, "data_bits": 8, "parity": "none", "stop_bits": "one" });
        let settings: SerialSettings = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            settings,
            SerialSettings {
                baud: 115_200,
                data_bits: 8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            }
        );
        assert_eq!(serde_json::to_value(settings).unwrap(), json);
    }

    #[test]
    fn terminal_state_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(TerminalState::Waiting).unwrap(),
            "waiting"
        );
    }
}
