//! Wire types for `capture.render` (SPECIFICATION.md §5.6): a drawn view of a
//! capture, plus the numbers it measured.
//!
//! The picture and the numbers travel together on purpose. A caller that can
//! see images gets the shape; a caller that cannot still gets every value the
//! picture is labelled with, in `metrics`. Neither is a summary of the other.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Which view to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum RenderKind {
    /// Amplitude against time, one lane per channel, min/max enveloped.
    Waveform,
    /// Every period folded onto the same axes and accumulated.
    Eye,
    /// The distribution of interval lengths, with the populations named.
    Histogram,
    /// The intervals that do not fit the dominant population, and where.
    Anomalies,
}

impl RenderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RenderKind::Waveform => "waveform",
            RenderKind::Eye => "eye",
            RenderKind::Histogram => "histogram",
            RenderKind::Anomalies => "anomalies",
        }
    }
}

/// Which intervals the histogram and anomaly views measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum IntervalKind {
    /// Time spent above the threshold — "are these bits the right length?"
    High,
    /// Time spent below it.
    Low,
    /// Rising edge to rising edge — "is this clock steady?"
    Period,
}

/// One thing a view found, located in time where it has a place.
///
/// This is how a long capture becomes actionable: not "3 % of periods are
/// wrong" but "the period at 1.483 s is 2.1 ms, +110 % of the reference". A
/// caller reads the list, takes a timestamp and asks for a waveform view of a
/// window around it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RenderFinding {
    /// Seconds from the start of the analysed window; absent for a finding
    /// that describes the whole capture.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub at_s: Option<f64>,
    pub label: String,
    pub detail: String,
}

/// One number the view measured.
///
/// `value` is in `unit`'s base unit — volts, seconds, hertz, percent — so a
/// caller can compute with it directly; `text` is the same number as the
/// picture labels it (`1.00 kHz`). Both are sent because deriving the second
/// from the first at each call site is where unit mistakes come from. `value`
/// is absent for a metric that is a word rather than a quantity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RenderMetric {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<f64>,
    pub unit: String,
    pub text: String,
}

/// A drawn view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RenderResult {
    pub capture: String,
    pub kind: RenderKind,
    /// The document itself. SVG, so the annotations in it are text a reader
    /// can parse rather than pixels it has to recognise, and so it scales in a
    /// browser without blurring.
    pub svg: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// The channels drawn, as hardware channel numbers (D4 is 4, CH1 is 0).
    pub channels: Vec<u8>,
    pub metrics: Vec<RenderMetric>,
    /// What the view found, located in time where it has a place.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub findings: Vec<RenderFinding>,
    /// Anything that stops the picture from being read at face value: a
    /// decimated trace, a recovered rather than given period, a closed eye.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
}
