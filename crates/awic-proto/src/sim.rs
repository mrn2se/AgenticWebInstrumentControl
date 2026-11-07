//! The virtual bench's wire types (`sim.status`, `sim.set`): a simulated
//! board under test with virtual instruments clipped onto its test points,
//! present only when the server runs with `[sim] enabled = true`.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Everything a client shows about the virtual bench. What the bench is
/// hiding — the logic analyzer's wiring when it was shuffled, the faults of
/// a mystery — is left out until it is revealed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimStatus {
    /// Seeds every random choice; the same seed and the same actions give
    /// the same waveforms.
    pub seed: u64,
    /// The virtual hub's bus path: its ports power the board and the
    /// instruments (`usb.power`, `usb.cycle`).
    pub hub: String,
    pub board: SimBoard,
    /// The puzzle chip on the board: keys in, seven segments out.
    pub counter: SimCounter,
    pub instruments: Vec<SimInstrument>,
    pub faults: Vec<SimFault>,
    /// A fault was chosen at random and is kept secret until revealed.
    pub mystery: bool,
    /// The wiring and the faults are shown.
    pub revealed: bool,
    /// The 555's timing resistor R2 — a pot on the board.
    pub osc_r2_ohm: f64,
    /// The 555's frequency for that R2, from its datasheet formula.
    pub osc_nominal_hz: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimBoard {
    pub name: String,
    pub description: String,
    /// Its bus path, on the virtual hub's port `port`.
    pub device: String,
    pub port: u8,
    pub powered: bool,
    /// Seconds since the MCU last started; absent while it is not running.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub uptime_s: Option<f64>,
    /// How many times it has started since the bench came up.
    pub boots: u32,
    pub last_reset_cause: String,
    /// The terminal source of the USB-UART on the board (`term.open`).
    pub serial_source: String,
    /// What the console runs at, e.g. `"115200 8N1"`.
    pub console: String,
    pub test_points: Vec<SimTestPoint>,
}

/// U3: a 16-pin CMOS chip with its markings sanded off, driving a
/// seven-segment display. Which part it is, is the puzzle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimCounter {
    /// `"U3"`.
    pub name: String,
    pub description: String,
    /// The part number — only once guessed right, or revealed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub part: Option<String>,
    pub solved: bool,
    /// How many guesses were wrong.
    pub wrong_guesses: u32,
    pub inputs: Vec<SimInput>,
    /// The display DS1: the segments lit now, as letters `a`..`g` (`"bc"`
    /// shows a 1). Empty while blank.
    pub display: String,
}

/// A key or switch on one of U3's pins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimInput {
    /// `"KEY1"`.
    pub id: String,
    /// The U3 pin it drives.
    pub pin: u8,
    /// A push button (pressed, then released) rather than a toggle switch.
    pub momentary: bool,
    /// The level it drives now (a key: whether it is held down).
    pub state: bool,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimTestPoint {
    /// `"TP1"`.
    pub id: String,
    /// `"UART TX"`.
    pub name: String,
    pub description: String,
    /// `"digital"` or `"analog"`.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimInstrument {
    pub device: String,
    pub label: String,
    pub port: u8,
    pub powered: bool,
    pub probes: Vec<SimProbe>,
    /// The test points its probes are on, in board order. Always known —
    /// even when which channel is on which of them is the secret.
    pub probed_points: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimProbe {
    pub channel: u8,
    /// The test point it is clipped to; absent while that is a secret.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub test_point: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SimFault {
    /// `"baud_drift"`.
    pub id: String,
    pub title: String,
    /// What it looks like from outside.
    pub symptom: String,
    /// Whether it is in force; absent while that is a secret.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active: Option<bool>,
    /// What is really going on; only once revealed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cause: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> SimStatus {
        SimStatus {
            seed: 42,
            hub: "254-1".into(),
            board: SimBoard {
                name: "board".into(),
                description: "d".into(),
                device: "254-1.1".into(),
                port: 1,
                powered: true,
                uptime_s: Some(1.5),
                boots: 1,
                last_reset_cause: "power-on".into(),
                serial_source: "serial:254-1.1:1.0".into(),
                console: "115200 8N1".into(),
                test_points: vec![SimTestPoint {
                    id: "TP1".into(),
                    name: "UART TX".into(),
                    description: "d".into(),
                    kind: "digital".into(),
                }],
            },
            counter: SimCounter {
                name: "U3".into(),
                description: "d".into(),
                part: None,
                solved: false,
                wrong_guesses: 0,
                inputs: vec![SimInput {
                    id: "KEY1".into(),
                    pin: 1,
                    momentary: true,
                    state: false,
                    description: "d".into(),
                }],
                display: "bc".into(),
            },
            instruments: vec![SimInstrument {
                device: "254-1.2".into(),
                label: "logic".into(),
                port: 2,
                powered: true,
                probes: vec![SimProbe {
                    channel: 0,
                    test_point: Some("TP3".into()),
                }],
                probed_points: vec!["TP3".into()],
            }],
            faults: vec![SimFault {
                id: "baud_drift".into(),
                title: "t".into(),
                symptom: "s".into(),
                active: Some(false),
                cause: None,
            }],
            mystery: false,
            revealed: false,
            osc_r2_ohm: 6800.0,
            osc_nominal_hz: 988.0,
        }
    }

    /// `ResponseResult` is untagged: the status must come back as itself,
    /// not as some earlier variant that happens to accept its fields.
    #[test]
    fn a_status_response_round_trips() {
        let resp = crate::Response::ok(1, crate::ResponseResult::SimStatus(Box::new(status())));
        let value = serde_json::to_value(&resp).unwrap();
        let back: crate::Response = serde_json::from_value(value).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn hidden_fields_are_left_out() {
        let probe = SimProbe {
            channel: 3,
            test_point: None,
        };
        assert_eq!(
            serde_json::to_value(&probe).unwrap(),
            serde_json::json!({"channel": 3})
        );
    }
}
