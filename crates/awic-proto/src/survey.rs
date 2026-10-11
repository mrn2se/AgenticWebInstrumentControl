//! `survey` wire types (live test F-04): "which pin does what" for a digital
//! capture or recording range in one call — per channel activity, idle
//! level, pulse widths and rhythm, plus protocol guesses. Times are seconds
//! (from the capture's or recording's start), not samples: the consumer is an
//! agent reasoning about a bench, not a renderer lining up pixels.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum SurveyActivity {
    /// No edge at all.
    Constant,
    /// Only spikes of a sample or two away from the resting level: crosstalk
    /// or a floating input.
    GlitchesOnly,
    /// Pulses of a sample or two as well, but back to back and far more than
    /// any other line has edges: a signal too fast for the sample rate.
    Undersampled,
    /// Activity in groups separated by idle gaps longer than 1 ms — or one
    /// group with that much quiet before or after it in the window.
    Bursty,
    /// Activity throughout.
    Continuous,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct ChannelSurveyInfo {
    pub channel: u8,
    /// `D0`..`D7`.
    pub name: String,
    /// One line for a human or an agent: `"I2C SDA (with SCL D6)"`,
    /// `"UART TX/RX @ 115200 baud"`, `"constant low"`, ...
    pub summary: String,
    pub activity: SurveyActivity,
    /// The level the line rests at (its longest steady stretch).
    pub idle_high: bool,
    pub edges: u64,
    /// Spikes of ≤ 2 samples.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub glitches: u64,
    /// Share of the surveyed time spent high, 0-1.
    pub high_fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub min_high_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub min_low_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub median_pulse_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_edge_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_edge_s: Option<f64>,
    #[serde(skip_serializing_if = "is_zero", default)]
    pub bursts: u64,
    /// Time between burst starts, when they come at a steady rhythm.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub burst_period_s: Option<f64>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// A protocol the survey recognized, with what `decode` needs to decode it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "protocol", rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum ProtocolGuess {
    Uart {
        channel: u8,
        /// The nearest standard rate within 3 %, else the measured one.
        baud: u32,
        measured_baud: f64,
        /// Share of pulses that fit a whole number of bits, 0-1.
        fit: f64,
        /// The line idles low: decode with `invert`.
        inverted: bool,
    },
    I2c {
        scl: u8,
        sda: u8,
        /// START conditions seen — roughly the number of transactions.
        starts: u64,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        clock_hz: Option<f64>,
    },
    Clock {
        channel: u8,
        frequency_hz: f64,
        /// Share of the period spent high, 0-1.
        duty: f64,
    },
    /// Decode with `spi`: `clk` as `clk`, the `data` lines as `mosi`/`miso`
    /// (timing cannot tell which is which), `cs` as `cs`, and `mode`.
    Spi {
        clk: u8,
        data: Vec<u8>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        cs: Option<u8>,
        /// The chip select is asserted high: decode with `cs_active_high`.
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        cs_active_high: bool,
        /// SPI mode 0-3: CPOL from the clock's resting level, CPHA from the
        /// edge the data is steady at.
        mode: u8,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        clock_hz: Option<f64>,
        /// The clock is too fast to time at this sample rate (no `clock_hz`):
        /// the roles hold, but capture faster if the device can — a decode
        /// at a few samples per bit may lose bits.
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        undersampled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SurveyResult {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub capture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rec: Option<u16>,
    pub rate_hz: u64,
    /// The range surveyed, seconds.
    pub t0_s: f64,
    pub t1_s: f64,
    pub channels: Vec<ChannelSurveyInfo>,
    pub guesses: Vec<ProtocolGuess>,
    /// Set when the recording is no longer live: when it ended and why, so
    /// a result that stops short of "now" says so (live test B-24).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}
