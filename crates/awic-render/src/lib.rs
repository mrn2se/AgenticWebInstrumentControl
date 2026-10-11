//! Server-side views of a capture, as SVG (SPECIFICATION.md §5.6).
//!
//! # Why the server draws
//!
//! The web UI renders its own live traces in WebGL, and will keep doing so —
//! this is not that. This exists so that a *caller without a screen* can ask
//! "what does this look like?" and get an answer: an agent over MCP, a script
//! over the wire protocol, a person pasting a link into an issue. The same
//! renderer backs the web UI's snapshot button, so what a person shares and
//! what an agent sees are the same picture.
//!
//! # Why SVG, and why the numbers come with it
//!
//! Every view returns a picture *and* a list of [`Metric`]s, and the picture's
//! `<desc>` contains the reading in words. That is deliberate: a model reading
//! `eye_height = 3.19 V` is not guessing from pixels, and a model that cannot
//! see images at all still gets the whole reading. The picture's job is to
//! show *why* the numbers are what they are — where the jitter is, which edge
//! is slow, whether there are two populations — which is exactly what a list
//! of numbers cannot convey.

mod charts;
pub mod eye;
pub mod png;
pub mod svg;
pub mod timing;
pub mod units;
mod waveform;

use awic_core::capture_store::Capture;
use awic_core::series::{hardware_channel_name, hardware_channel_series, hardware_channels};

pub use eye::{EyeAnalysis, UiSource};

/// Which view to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Amplitude against time: one lane per channel, min/max enveloped so no
    /// narrow pulse is dropped by decimation.
    Waveform,
    /// Every unit interval folded onto the same axes and accumulated.
    Eye,
    /// How long the intervals are, as a distribution, with the populations
    /// they fall into named.
    Histogram,
    /// Which intervals do not fit the dominant population, and where they are.
    Anomalies,
}

impl ViewMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ViewMode::Waveform => "waveform",
            ViewMode::Eye => "eye",
            ViewMode::Histogram => "histogram",
            ViewMode::Anomalies => "anomalies",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "waveform" => Some(ViewMode::Waveform),
            "eye" => Some(ViewMode::Eye),
            "histogram" => Some(ViewMode::Histogram),
            "anomalies" => Some(ViewMode::Anomalies),
            _ => None,
        }
    }

    /// Whether the view is about one channel. Folding or clustering two
    /// signals together produces a picture of neither.
    pub fn is_single_channel(self) -> bool {
        !matches!(self, ViewMode::Waveform)
    }
}

/// What to draw and how big.
#[derive(Debug, Clone)]
pub struct ViewRequest {
    pub mode: ViewMode,
    /// Channels to draw, as hardware channel numbers (D4 is 4, CH1 is 0), the
    /// same addressing as every other tool; empty means all of them.
    /// The eye view takes exactly one — folding two signals onto one set of
    /// axes produces a picture of nothing.
    pub channels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Window within the capture, seconds from its start. `None` is the whole
    /// capture.
    pub t0_s: Option<f64>,
    pub t1_s: Option<f64>,
    /// Eye view: the unit interval to fold on. `None` recovers it from the
    /// signal.
    pub period_s: Option<f64>,
    /// Histogram and anomaly views: which intervals to measure. Defaults to
    /// periods, which is what "is this clock steady" asks about; pulse widths
    /// are what "are these bits the right length" asks about.
    pub interval_kind: Option<timing::IntervalKind>,
    /// Anomaly view: how far an interval may stray from the dominant one
    /// before it is called out, as a fraction. Defaults to
    /// `charts::DEFAULT_TOLERANCE`.
    pub tolerance: Option<f64>,
    pub title: Option<String>,
}

impl Default for ViewRequest {
    fn default() -> Self {
        Self {
            mode: ViewMode::Waveform,
            channels: Vec::new(),
            width: 900,
            height: 420,
            t0_s: None,
            t1_s: None,
            period_s: None,
            interval_kind: None,
            tolerance: None,
            title: None,
        }
    }
}

/// One number the view measured, in the form an agent should read it.
///
/// `value` is the number in `unit`'s own base unit (volts, seconds, hertz) so
/// arithmetic on it needs no unprefixing; `text` is the same number as the
/// picture labels it. Both are present because a caller doing maths and a
/// caller writing a sentence want different things, and computing the second
/// from the first is where unit bugs live.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: String,
    pub value: Option<f64>,
    pub unit: String,
    pub text: String,
}

impl Metric {
    pub fn new(name: &str, value: f64, unit: &str) -> Self {
        Self {
            name: name.to_string(),
            value: Some(value),
            unit: unit.to_string(),
            text: units::si(value, unit),
        }
    }

    /// A metric whose value is a plain count or a word, not a physical
    /// quantity.
    pub fn plain(name: &str, text: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            value: None,
            unit: String::new(),
            text: text.into(),
        }
    }

    pub fn count(name: &str, n: usize) -> Self {
        Self {
            name: name.to_string(),
            value: Some(n as f64),
            unit: String::new(),
            text: n.to_string(),
        }
    }
}

/// One thing the view found, optionally located in time.
///
/// This is what turns a long capture into something an agent can act on: not
/// "3 % of periods are wrong" but "the period at 1.483 s is 2.1 ms, +110 % of
/// the 1.00 ms reference". A caller reads the list, picks a timestamp and asks
/// for a waveform view of a window around it.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Seconds from the start of the analysed window, when the finding has a
    /// place; `None` for one that describes the whole capture.
    pub at_s: Option<f64>,
    pub label: String,
    pub detail: String,
}

/// A drawn view.
#[derive(Debug, Clone)]
pub struct View {
    pub svg: String,
    pub width: u32,
    pub height: u32,
    pub mode: &'static str,
    /// The channels actually drawn, as hardware channel numbers.
    pub channels: Vec<u8>,
    pub metrics: Vec<Metric>,
    /// What the view found, located in time where it has a place.
    pub findings: Vec<Finding>,
    /// Anything the reader needs in order not to over-trust the picture: a
    /// recovered rather than given period, a closed eye, a decimated trace.
    pub notes: Vec<String>,
}

/// The palette. Light background on purpose: it is what reproduces in a
/// document, a terminal screenshot and a model's image input alike, and thin
/// dark traces on white survive rescaling better than the reverse.
mod theme {
    pub const BG: &str = "#ffffff";
    pub const PANEL: &str = "#fafafa";
    pub const GRID: &str = "#e4e4e7";
    pub const AXIS: &str = "#71717a";
    pub const TEXT: &str = "#18181b";
    pub const MUTED: &str = "#71717a";
    pub const TRACE: [&str; 8] = [
        "#2563eb", "#dc2626", "#16a34a", "#d97706", "#7c3aed", "#0891b2", "#db2777", "#4d7c0f",
    ];
    pub const DENSITY: &str = "#1d4ed8";
    pub const MARKER: &str = "#dc2626";
}

/// Plot margins. Left is wide enough for an SI-prefixed voltage label.
const M_LEFT: f64 = 62.0;
const M_RIGHT: f64 = 14.0;
const M_TOP: f64 = 34.0;
const M_BOTTOM: f64 = 30.0;
/// Room under the plot for the measured numbers.
const FOOTER: f64 = 44.0;

/// Draws `request` from `capture`.
pub fn render(capture: &Capture, request: &ViewRequest) -> Result<View, String> {
    let width = request.width.clamp(240, 4000);
    let height = request.height.clamp(160, 4000);
    let rate_hz = capture.meta.rate_hz as f64;
    if rate_hz <= 0.0 {
        return Err("the capture has no sample rate".to_string());
    }

    // Hardware channel numbers (D4, CH1 = 0), as every other tool takes them
    // (B-33). `hardware_channel_series` refuses one the capture lacks.
    let wanted = if request.channels.is_empty() {
        hardware_channels(capture)
    } else {
        request.channels.clone()
    };
    if wanted.is_empty() {
        return Err("the capture holds no channels".to_string());
    }

    // The window, as sample indices.
    let total = series_len(capture);
    if total == 0 {
        return Err("the capture holds no samples".to_string());
    }
    let from = request
        .t0_s
        .map(|t| (t.max(0.0) * rate_hz).floor() as usize)
        .unwrap_or(0)
        .min(total.saturating_sub(1));
    let to = request
        .t1_s
        .map(|t| (t.max(0.0) * rate_hz).ceil() as usize)
        .unwrap_or(total)
        .clamp(from + 1, total);

    let mut traces = Vec::new();
    for &ch in &wanted {
        let series = hardware_channel_series(capture, ch).map_err(|e| e.to_string())?;
        let name = hardware_channel_name(capture, ch);
        let unit = series.unit().to_string();
        let values = series.to_f64();
        traces.push(Trace {
            channel: ch,
            name,
            unit,
            values: values[from.min(values.len())..to.min(values.len())].to_vec(),
        });
    }

    if request.mode.is_single_channel() && traces.len() != 1 {
        return Err(format!(
            "the {} view folds one channel; {} were asked for — name one with `channels`",
            request.mode.as_str(),
            traces.len()
        ));
    }
    let mut view = match request.mode {
        ViewMode::Waveform => Ok(waveform::draw(
            request, width, height, rate_hz, from, &traces,
        )),
        ViewMode::Eye => {
            noise_floor_refusal(capture, traces[0].channel, &traces[0].values)?;
            waveform::draw_eye(request, width, height, rate_hz, &traces[0])
        }
        ViewMode::Histogram => charts::draw_histogram(request, width, height, rate_hz, &traces[0]),
        ViewMode::Anomalies => charts::draw_anomalies(request, width, height, rate_hz, &traces[0]),
    }?;
    // A clipped capture is drawn as a line welded to the rail, and nothing in
    // the picture says why (B-54). The codes know, so the view says so.
    for &ch in &wanted {
        if let Some(clipping) = awic_core::series::hardware_channel_clipping(capture, ch) {
            let name = hardware_channel_name(capture, ch);
            view.notes.push(format!("{name}: {}", clipping.describe()));
            view.findings.push(Finding {
                at_s: None,
                label: format!("{name} clipped"),
                detail: clipping.describe(),
            });
        }
    }
    // A start with no end is easy to mistake for a client that dropped the
    // end: say what the window is (B-36).
    if request.t0_s.is_some() && request.t1_s.is_none() {
        view.notes.push(format!(
            "window starts at {:.1} ms and runs to the end of the capture ({:.1} ms long); pass t1_s (or span_s) to zoom",
            from as f64 / rate_hz * 1e3,
            (to - from) as f64 / rate_hz * 1e3,
        ));
    }
    Ok(view)
}

/// Eye metrics with no picture, for a caller that only wants the numbers —
/// an agent checking whether a link is still clean does not need the shading.
pub fn eye_metrics(
    capture: &Capture,
    channel: u8,
    period_s: Option<f64>,
) -> Result<(EyeAnalysis, Vec<Metric>), String> {
    let series = hardware_channel_series(capture, channel).map_err(|e| e.to_string())?;
    let unit = series.unit().to_string();
    let values = series.to_f64();
    noise_floor_refusal(capture, channel, &values)?;
    let analysis = eye::analyze(&values, capture.meta.rate_hz as f64, 256, 128, period_s)?;
    let metrics = waveform::eye_metric_list(&analysis, &unit);
    Ok((analysis, metrics))
}

/// An eye on a line that spans a few ADC codes folds the converter's noise:
/// it "recovered" an 889 kHz clock from a probe on no signal (B-105).
fn noise_floor_refusal(capture: &Capture, channel: u8, values: &[f64]) -> Result<(), String> {
    match awic_core::measure::below_timing_floor(capture, channel, values) {
        Some(why) => Err(format!(
            "{} cannot be folded into an eye: {why}",
            hardware_channel_name(capture, channel)
        )),
        None => Ok(()),
    }
}

/// Internal: one channel's window, already in physical units.
pub(crate) struct Trace {
    pub channel: u8,
    pub name: String,
    pub unit: String,
    pub values: Vec<f64>,
}

fn series_len(capture: &Capture) -> usize {
    match &capture.data {
        awic_core::capture_store::CaptureData::Digital { packed, .. } => packed.len(),
        awic_core::capture_store::CaptureData::Analog {
            raw, sample_bits, ..
        } => {
            let per = if *sample_bits <= 8 { 1 } else { 2 };
            let n = capture.meta.channel_count.max(1) as usize;
            raw.len() / (per * n)
        }
    }
}
