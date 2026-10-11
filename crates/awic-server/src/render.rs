//! `capture.render`: the wire and MCP face of [`awic_render`]
//! (SPECIFICATION.md §5.6).
//!
//! The server's part is small on purpose — look up the capture, translate the
//! request, translate the result — because the drawing and the measuring live
//! in `awic-render`, where they are testable without a server.

use awic_proto::{IntervalKind, RenderFinding, RenderKind, RenderMetric, RenderResult};
use awic_render::timing::IntervalKind as CoreIntervalKind;
use awic_render::{render as draw, Finding, Metric, ViewMode, ViewRequest};

use awic_core::capture_store::Capture;

/// Everything optional about a render request.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub channels: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub t0_s: Option<f64>,
    pub t1_s: Option<f64>,
    pub period_s: Option<f64>,
    pub interval_kind: Option<IntervalKind>,
    pub tolerance: Option<f64>,
    pub title: Option<String>,
}

/// Default canvas. Wide enough that a second of a 1 kHz signal is legible and
/// small enough that the document stays a few tens of kilobytes.
const DEFAULT_WIDTH: u32 = 900;
const DEFAULT_HEIGHT: u32 = 420;

pub fn render(
    capture: &Capture,
    handle: &str,
    kind: RenderKind,
    options: Options,
) -> Result<RenderResult, String> {
    let request = ViewRequest {
        mode: match kind {
            RenderKind::Waveform => ViewMode::Waveform,
            RenderKind::Eye => ViewMode::Eye,
            RenderKind::Histogram => ViewMode::Histogram,
            RenderKind::Anomalies => ViewMode::Anomalies,
        },
        channels: options.channels,
        width: options.width.unwrap_or(DEFAULT_WIDTH),
        height: options.height.unwrap_or(DEFAULT_HEIGHT),
        t0_s: options.t0_s,
        t1_s: options.t1_s,
        period_s: options.period_s,
        interval_kind: options.interval_kind.map(|k| match k {
            IntervalKind::High => CoreIntervalKind::High,
            IntervalKind::Low => CoreIntervalKind::Low,
            IntervalKind::Period => CoreIntervalKind::Period,
        }),
        tolerance: options.tolerance,
        title: options.title,
    };
    let view = draw(capture, &request)?;
    Ok(RenderResult {
        capture: handle.to_string(),
        kind,
        svg: view.svg,
        mime_type: "image/svg+xml".to_string(),
        width: view.width,
        height: view.height,
        channels: view.channels,
        metrics: view.metrics.iter().map(metric).collect(),
        findings: view.findings.iter().map(finding).collect(),
        notes: view.notes,
    })
}

fn finding(f: &Finding) -> RenderFinding {
    RenderFinding {
        at_s: f.at_s,
        label: f.label.clone(),
        detail: f.detail.clone(),
    }
}

fn metric(m: &Metric) -> RenderMetric {
    RenderMetric {
        name: m.name.clone(),
        value: m.value,
        unit: m.unit.clone(),
        text: m.text.clone(),
    }
}
