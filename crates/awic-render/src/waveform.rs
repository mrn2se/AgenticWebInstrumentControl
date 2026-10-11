//! The two drawing routines: amplitude-against-time and the folded eye.

use awic_core::minmax_decimate;

use crate::eye::{EyeAnalysis, EyeKind, UiSource};
use crate::svg::{Anchor, Svg};
use crate::units::{si, tick_labels, ticks};
use crate::{theme, Metric, Trace, View, ViewRequest, FOOTER, M_BOTTOM, M_LEFT, M_RIGHT, M_TOP};

/// Amplitude against time, one lane per channel.
///
/// Every column is drawn as the **min/max band** of the samples behind it,
/// never as one sampled point: at 24 MS/s over 900 pixels each column stands
/// for thousands of samples, and picking one of them hides exactly the short
/// glitch a person is looking for.
pub(crate) fn draw(
    request: &ViewRequest,
    width: u32,
    height: u32,
    rate_hz: f64,
    from_sample: usize,
    traces: &[Trace],
) -> View {
    let plot_w = f64::from(width) - M_LEFT - M_RIGHT;
    let plot_h = f64::from(height) - M_TOP - M_BOTTOM - FOOTER;
    let cols = (plot_w.max(1.0) as usize).min(4000);
    let lanes = traces.len().max(1);
    let lane_h = plot_h / lanes as f64;

    let t0 = from_sample as f64 / rate_hz;
    let samples = traces.iter().map(|t| t.values.len()).max().unwrap_or(0);
    let t1 = t0 + samples as f64 / rate_hz;

    let mut svg = Svg::new(width, height);
    svg.rect(0.0, 0.0, f64::from(width), f64::from(height), theme::BG);
    svg.rect(M_LEFT, M_TOP, plot_w, plot_h, theme::PANEL);

    let mut metrics = Vec::new();
    let mut notes = Vec::new();
    let x_of = |t: f64| M_LEFT + (t - t0) / (t1 - t0).max(f64::MIN_POSITIVE) * plot_w;

    // Time grid, shared by every lane. Labelled from the tick spacing, not
    // from each tick on its own: a narrow window late in a capture otherwise
    // spells every label "399 ms" (B-53).
    let time_ticks = ticks(t0, t1, 8);
    for (t, label) in time_ticks.iter().zip(tick_labels(&time_ticks, "s")) {
        let x = x_of(*t);
        svg.line(x, M_TOP, x, M_TOP + plot_h, theme::GRID, 1.0);
        svg.text(
            x,
            M_TOP + plot_h + 14.0,
            &label,
            theme::MUTED,
            10.0,
            Anchor::Middle,
        );
    }

    for (i, trace) in traces.iter().enumerate() {
        let top = M_TOP + lane_h * i as f64;
        let colour = theme::TRACE[i % theme::TRACE.len()];
        let (lo, hi) = padded_span(&trace.values, &trace.unit);
        let y_of = |v: f64| {
            let t = ((v - lo) / (hi - lo).max(f64::MIN_POSITIVE)).clamp(0.0, 1.0);
            top + (1.0 - t) * (lane_h - 6.0) + 3.0
        };

        // Lane separator and label. The name goes just inside the plot, where
        // the live renderer puts it too: in the left margin it landed on top
        // of the lane's own top value label, and "3.17 V" under "CH1" read
        // "3.1CH1" (B-53). The margin is for values only.
        if i > 0 {
            svg.line(M_LEFT, top, M_LEFT + plot_w, top, theme::GRID, 1.0);
        }
        svg.text_bold(
            M_LEFT + 4.0,
            top + 12.0,
            &trace.name,
            colour,
            11.0,
            Anchor::Start,
        );

        if trace.values.is_empty() {
            svg.text(
                M_LEFT + plot_w / 2.0,
                top + lane_h / 2.0,
                "no samples in this window",
                theme::MUTED,
                11.0,
                Anchor::Middle,
            );
            continue;
        }

        // Value labels: the lane's own extremes, which is what a reader wants
        // from a lane that is only a few tens of pixels tall.
        let unit = trace.unit.as_str();
        for (v, anchor_y) in [(hi, top + 10.0), (lo, top + lane_h - 3.0)] {
            svg.text(
                M_LEFT - 8.0,
                anchor_y,
                &level_label(v, unit),
                theme::MUTED,
                9.0,
                Anchor::End,
            );
        }

        let band = minmax_decimate(&trace.values, cols);
        if band.len() < trace.values.len() {
            notes.push(format!(
                "{}: {} samples drawn as {} min/max columns — no pulse is dropped, but a single \
                 column may stand for many edges",
                trace.name,
                trace.values.len(),
                band.len()
            ));
        }
        let drawn = connected_band(&band);
        let mut d = String::new();
        for (col, (min, max)) in drawn.iter().enumerate() {
            let x = M_LEFT + (col as f64 + 0.5) / drawn.len() as f64 * plot_w;
            let y1 = y_of(*max);
            let y2 = y_of(*min);
            // A vertical segment per column: a band where the signal moved
            // within the column, a dot where it did not.
            d.push_str(&format!("M{:.1} {:.1}V{:.1}", x, y1, y2.max(y1 + 0.6)));
        }
        svg.path(&d, colour, "none", 1.2);

        let min = trace.values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = trace
            .values
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        if min.is_finite() && max.is_finite() {
            metrics.push(Metric::new(
                &format!("{}_min", trace.name.to_lowercase()),
                min,
                unit,
            ));
            metrics.push(Metric::new(
                &format!("{}_max", trace.name.to_lowercase()),
                max,
                unit,
            ));
        }
    }

    // Axes over the traces, so the frame stays visible.
    svg.line(M_LEFT, M_TOP, M_LEFT, M_TOP + plot_h, theme::AXIS, 1.0);
    svg.line(
        M_LEFT,
        M_TOP + plot_h,
        M_LEFT + plot_w,
        M_TOP + plot_h,
        theme::AXIS,
        1.0,
    );

    let title = request
        .title
        .clone()
        .unwrap_or_else(|| format!("{} channel(s), {}", traces.len(), si(rate_hz, "S/s")));
    svg.text_bold(M_LEFT, 20.0, &title, theme::TEXT, 13.0, Anchor::Start);
    svg.text(
        f64::from(width) - M_RIGHT,
        20.0,
        &format!("{} window", si(t1 - t0, "s")),
        theme::MUTED,
        11.0,
        Anchor::End,
    );

    metrics.insert(0, Metric::new("window", t1 - t0, "s"));
    metrics.insert(1, Metric::new("sample_rate", rate_hz, "S/s"));
    footer(&mut svg, width, height, &metrics);

    let desc = describe(&title, &metrics, &notes);
    View {
        svg: svg.finish(&title, &desc),
        width,
        height,
        mode: "waveform",
        channels: traces.iter().map(|t| t.channel).collect(),
        metrics,
        // A waveform is a picture of everything, not a search for anything:
        // there is nothing for it to single out. `render` adds one kind of
        // finding afterwards - a channel clipped against a converter rail,
        // which is a defect of the capture rather than something drawn.
        findings: Vec::new(),
        notes,
    }
}

/// The folded eye.
pub(crate) fn draw_eye(
    request: &ViewRequest,
    width: u32,
    height: u32,
    rate_hz: f64,
    trace: &Trace,
) -> Result<View, String> {
    let plot_w = f64::from(width) - M_LEFT - M_RIGHT;
    let plot_h = f64::from(height) - M_TOP - M_BOTTOM - FOOTER;
    let cols = (plot_w.max(16.0) as usize).min(1200);
    let rows = (plot_h.max(16.0) as usize).min(800);
    let analysis = crate::eye::analyze(&trace.values, rate_hz, cols, rows, request.period_s)?;
    let unit = trace.unit.as_str();

    let mut svg = Svg::new(width, height);
    svg.rect(0.0, 0.0, f64::from(width), f64::from(height), theme::BG);
    svg.rect(M_LEFT, M_TOP, plot_w, plot_h, theme::PANEL);

    let cw = plot_w / analysis.cols as f64;
    let rh = plot_h / analysis.rows as f64;
    let y_of = |v: f64| {
        let t = ((v - analysis.y_lo) / (analysis.y_hi - analysis.y_lo).max(f64::MIN_POSITIVE))
            .clamp(0.0, 1.0);
        M_TOP + (1.0 - t) * plot_h
    };
    let total_ui = 2.0 * analysis.ui_s;

    // Time grid in unit intervals: the reader needs to see where one bit ends.
    for k in 0..=4 {
        let t = total_ui * f64::from(k) / 4.0;
        let x = M_LEFT + f64::from(k) / 4.0 * plot_w;
        svg.line(x, M_TOP, x, M_TOP + plot_h, theme::GRID, 1.0);
        svg.text(
            x,
            M_TOP + plot_h + 14.0,
            &si(t, "s"),
            theme::MUTED,
            10.0,
            Anchor::Middle,
        );
    }
    for v in ticks(analysis.y_lo, analysis.y_hi, 4) {
        let y = y_of(v);
        svg.line(M_LEFT, y, M_LEFT + plot_w, y, theme::GRID, 1.0);
        svg.text(
            M_LEFT - 8.0,
            y + 3.5,
            &level_label(v, unit),
            theme::MUTED,
            10.0,
            Anchor::End,
        );
    }

    // The density itself, in LEVELS opacity bands. Runs of equal level within
    // a column become one rectangle, which is what keeps the document small:
    // a real eye has a handful of runs per column, not hundreds.
    const LEVELS: u32 = 6;
    let scale = (f64::from(analysis.max_count) + 1.0).ln();
    let mut bands = vec![String::new(); LEVELS as usize];
    for col in 0..analysis.cols {
        let mut row = 0;
        while row < analysis.rows {
            let level = density_level(analysis.density[row * analysis.cols + col], scale, LEVELS);
            if level == 0 {
                row += 1;
                continue;
            }
            let start = row;
            while row + 1 < analysis.rows
                && density_level(
                    analysis.density[(row + 1) * analysis.cols + col],
                    scale,
                    LEVELS,
                ) == level
            {
                row += 1;
            }
            let run = (row - start + 1) as f64;
            bands[(level - 1) as usize].push_str(&format!(
                "M{:.1} {:.1}h{:.1}v{:.1}h-{:.1}z",
                M_LEFT + col as f64 * cw,
                M_TOP + start as f64 * rh,
                cw.max(0.6),
                run * rh,
                cw.max(0.6)
            ));
            row += 1;
        }
    }
    for (i, d) in bands.iter().enumerate() {
        let alpha = (i + 1) as f64 / LEVELS as f64;
        svg.path_alpha(d, theme::DENSITY, alpha);
    }

    // Annotations. Each one is also a metric below, so nothing in the picture
    // is only in the picture.
    // A periodic analog signal has no two levels and so no threshold or
    // opening to draw (B-74): the shading is the whole picture.
    let periodic = analysis.kind == EyeKind::Periodic;
    let threshold_y = y_of(analysis.threshold);
    if !periodic {
        svg.dashed(
            M_LEFT,
            threshold_y,
            M_LEFT + plot_w,
            threshold_y,
            theme::AXIS,
        );
        svg.text(
            M_LEFT + plot_w - 4.0,
            threshold_y - 4.0,
            &format!("threshold {}", level_label(analysis.threshold, unit)),
            theme::MUTED,
            9.0,
            Anchor::End,
        );
    }

    let phase = analysis.eye_phase.unwrap_or(0.5);
    let centre_x = M_LEFT + phase * plot_w;
    if let (Some(h), Some(w)) = (analysis.eye_height, analysis.eye_width_s) {
        let half_h = h / 2.0;
        let mid = (analysis.level_low + analysis.level_high) / 2.0;
        let top = y_of(mid + half_h);
        let bottom = y_of(mid - half_h);
        let half_w = w / 2.0 / total_ui * plot_w;
        // The opening, as a box a reader can compare against the shading.
        svg.line(centre_x, top, centre_x, bottom, theme::MARKER, 1.5);
        svg.line(
            centre_x - half_w,
            (top + bottom) / 2.0,
            centre_x + half_w,
            (top + bottom) / 2.0,
            theme::MARKER,
            1.5,
        );
        svg.text(
            centre_x + 6.0,
            (top + bottom) / 2.0,
            &format!("height {}", si(h, unit)),
            theme::MARKER,
            10.0,
            Anchor::Start,
        );
        svg.text(
            centre_x,
            (top + bottom) / 2.0 - 6.0,
            &format!("width {}", si(w, "s")),
            theme::MARKER,
            10.0,
            Anchor::Middle,
        );
    } else if !periodic {
        svg.text(
            centre_x,
            M_TOP + plot_h / 2.0,
            "eye closed at its centre",
            theme::MARKER,
            12.0,
            Anchor::Middle,
        );
    }

    svg.line(M_LEFT, M_TOP, M_LEFT, M_TOP + plot_h, theme::AXIS, 1.0);
    svg.line(
        M_LEFT,
        M_TOP + plot_h,
        M_LEFT + plot_w,
        M_TOP + plot_h,
        theme::AXIS,
        1.0,
    );

    let title = request.title.clone().unwrap_or_else(|| {
        format!(
            "{} eye, {} periods folded on {}",
            trace.name,
            analysis.traces,
            si(analysis.ui_s, "s")
        )
    });
    svg.text_bold(M_LEFT, 20.0, &title, theme::TEXT, 13.0, Anchor::Start);
    svg.text(
        f64::from(width) - M_RIGHT,
        20.0,
        "2 unit intervals",
        theme::MUTED,
        11.0,
        Anchor::End,
    );

    let metrics = eye_metric_list(&analysis, unit);
    let mut notes = analysis.notes.clone();
    if unit.is_empty() {
        // A digital lane is one bit per sample: its levels are exactly 0 and 1
        // and the vertical opening is therefore always the full range. Left
        // unsaid, `eye_height = 1.00` reads like a measurement of signal
        // quality, which on a logic line it is not — the timing numbers are.
        notes.push(
            "this is a digital channel, so its levels are exactly 0 and 1 and the vertical \
             opening is full by construction — on a logic line the timing numbers (jitter, the \
             settled stretches, the duty cycle) are what this view measures. For signal quality, \
             probe the same line with the oscilloscope."
                .to_string(),
        );
    }
    footer(&mut svg, width, height, &metrics);
    let desc = describe(&title, &metrics, &notes);
    Ok(View {
        svg: svg.finish(&title, &desc),
        width,
        height,
        mode: "eye",
        channels: vec![trace.channel],
        metrics,
        findings: eye_findings(&analysis, unit),
        notes,
    })
}

/// The eye's own findings: the two facts a reader most often needs to act on,
/// stated rather than left to be derived from the metric list.
fn eye_findings(a: &EyeAnalysis, unit: &str) -> Vec<crate::Finding> {
    let mut out = Vec::new();
    if let (Some(height), Some(phase)) = (a.eye_height, a.eye_phase) {
        out.push(crate::Finding {
            at_s: Some(phase * 2.0 * a.ui_s),
            label: "widest opening".to_string(),
            detail: format!(
                "{} of vertical room, {:.0} % of the way through the two-interval window",
                si(height, unit),
                phase * 100.0
            ),
        });
    }
    if let Some(tie) = a.tie_pp_s {
        out.push(crate::Finding {
            at_s: None,
            label: "timing spread".to_string(),
            detail: format!(
                "crossings stray {} peak to peak, {:.2} % of the {} unit interval",
                si(tie, "s"),
                tie / a.ui_s.max(f64::MIN_POSITIVE) * 100.0,
                si(a.ui_s, "s")
            ),
        });
    }
    out
}

/// The eye's numbers, in the order a reader should meet them.
pub(crate) fn eye_metric_list(a: &EyeAnalysis, unit: &str) -> Vec<Metric> {
    let mut m = vec![
        Metric::new("unit_interval", a.ui_s, "s"),
        Metric::new("frequency", 1.0 / a.ui_s.max(f64::MIN_POSITIVE), "Hz"),
        Metric::plain(
            "unit_interval_source",
            match a.ui_source {
                UiSource::Given => "given by the caller",
                UiSource::Recovered => "recovered from the signal",
                UiSource::RecoveredFromRuns => {
                    "recovered from the shortest run between crossings (NRZ data)"
                }
            },
        ),
        Metric::count("periods_folded", a.traces),
        Metric::plain(
            "kind",
            match a.kind {
                EyeKind::Persistence => {
                    "persistence: every folded period agrees, so eye_width is the shorter settled stretch"
                }
                EyeKind::Data => {
                    "data: phases carry both levels, so eye_width is the opening at the threshold"
                }
                EyeKind::Periodic => {
                    "periodic: a signal with no two levels (a sine), folded as persistence; \
                     it has no eye opening, only a period and jitter"
                }
            },
        ),
    ];
    if a.kind == EyeKind::Periodic {
        // Levels, threshold and the opening are NRZ quantities (B-74).
        m.push(Metric::new("min", a.y_lo, unit));
        m.push(Metric::new("max", a.y_hi, unit));
    } else {
        m.push(Metric::new("level_low", a.level_low, unit));
        m.push(Metric::new("level_high", a.level_high, unit));
        m.push(Metric::new("threshold", a.threshold, unit));
    }
    match a.eye_height {
        _ if a.kind == EyeKind::Periodic => {}
        Some(h) => m.push(Metric::new("eye_height", h, unit)),
        None => m.push(Metric::plain("eye_height", "unavailable")),
    }
    match a.eye_width_s {
        _ if a.kind == EyeKind::Periodic => {}
        Some(w) => {
            m.push(Metric::new("eye_width", w, "s"));
            m.push(Metric::new(
                "eye_width_percent_of_ui",
                w / a.ui_s.max(f64::MIN_POSITIVE) * 100.0,
                "%",
            ));
        }
        None => m.push(Metric::plain("eye_width", "closed")),
    }
    if let Some(h) = a.settled_high_s {
        m.push(Metric::new("settled_high", h, "s"));
    }
    if let Some(l) = a.settled_low_s {
        m.push(Metric::new("settled_low", l, "s"));
    }
    if let (Some(h), Some(l)) = (a.settled_high_s, a.settled_low_s) {
        let total = h + l;
        if total > 0.0 {
            m.push(Metric::new("duty", h / total * 100.0, "%"));
        }
    }
    match a.tie_pp_s {
        Some(j) => m.push(Metric::new("jitter_tie_pp", j, "s")),
        None => m.push(Metric::plain("jitter_tie_pp", "unavailable")),
    }
    // Which grid that number was measured against (B-70): one per burst, so a
    // reader knows it is the worst stretch and not an average over idle time.
    if let Some(bursts) = a.tie_bursts {
        m.push(Metric::count("jitter_bursts", bursts));
    }
    if let Some(c) = a.cycle_to_cycle_pp_s {
        m.push(Metric::new("jitter_cycle_to_cycle_pp", c, "s"));
    }
    if let Some(p) = a.fitted_period_s {
        m.push(Metric::new("fitted_period", p, "s"));
    }
    m
}

/// The measured numbers as a line of text under the plot. The picture is not
/// the only place they appear — they are in the metrics and in `<desc>` too —
/// but a shared screenshot should not need the JSON beside it.
pub(crate) fn footer(svg: &mut Svg, width: u32, height: u32, metrics: &[Metric]) {
    let y = f64::from(height) - FOOTER + 14.0;
    let mut line = String::new();
    let mut second = String::new();
    for m in metrics {
        let piece = format!("{}={}  ", m.name, m.text);
        if line.len() + piece.len() < 108 {
            line.push_str(&piece);
        } else if second.len() + piece.len() < 108 {
            second.push_str(&piece);
        }
    }
    svg.text(M_LEFT, y, line.trim_end(), theme::TEXT, 10.0, Anchor::Start);
    if !second.is_empty() {
        svg.text(
            M_LEFT,
            y + 13.0,
            second.trim_end(),
            theme::TEXT,
            10.0,
            Anchor::Start,
        );
    }
    let _ = width;
}

/// `<desc>`: the whole reading in words, for a reader that cannot see the
/// picture — including a model with images switched off.
pub(crate) fn describe(title: &str, metrics: &[Metric], notes: &[String]) -> String {
    let mut out = title.to_string();
    out.push_str(". ");
    for m in metrics {
        out.push_str(&format!("{}: {}. ", m.name, m.text));
    }
    for n in notes {
        out.push_str(n);
        out.push_str(". ");
    }
    out.trim_end().to_string()
}

/// A digital lane's levels read as `0`/`1`, not `0 ` and `1 ` with a stray
/// unit — and an analog one keeps its volts.
fn level_label(v: f64, unit: &str) -> String {
    if unit.is_empty() {
        if v >= 0.5 {
            "1".to_string()
        } else {
            "0".to_string()
        }
    } else {
        si(v, unit)
    }
}

/// Each column's min/max band stretched to meet the previous column's.
///
/// A column is the min/max of the samples behind it, and with few samples per
/// column an edge can fall exactly on a column boundary: column *k* is all
/// high, column *k+1* all low. Drawing each column's own range then strokes
/// the two levels and never the transition between them, so the trace comes
/// out as disconnected ticks at both levels with no vertical edges at all
/// (B-52, visible on a 320 us window at 4 MS/s - 1280 samples over 824
/// columns). Reaching back to the neighbour's range closes exactly those
/// gaps and leaves overlapping columns untouched.
fn connected_band(band: &[(f64, f64)]) -> Vec<(f64, f64)> {
    band.iter()
        .enumerate()
        .map(|(col, &(min, max))| {
            let (mut lo, mut hi) = (min, max);
            if let Some(&(pmin, pmax)) = col.checked_sub(1).and_then(|i| band.get(i)) {
                lo = lo.min(pmax);
                hi = hi.max(pmin);
            }
            (lo, hi)
        })
        .collect()
}

/// The span to draw, with a little headroom so a flat trace is not a line
/// welded to the lane's edge. Digital lanes are always drawn 0..1.
fn padded_span(values: &[f64], unit: &str) -> (f64, f64) {
    if unit.is_empty() {
        return (-0.15, 1.15);
    }
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &v in values {
        if v.is_finite() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0);
    }
    let pad = ((hi - lo) * 0.08).max(1e-6);
    (lo - pad, hi + pad)
}

/// Counts to one of `levels` opacity bands, logarithmically: a linear ramp
/// hides everything but the two settled levels, which are hundreds of times
/// denser than the edges that the view exists to show.
fn density_level(count: u32, log_max: f64, levels: u32) -> u32 {
    if count == 0 || log_max <= 0.0 {
        return 0;
    }
    let t = (f64::from(count) + 1.0).ln() / log_max;
    ((t * f64::from(levels)).ceil() as u32).clamp(1, levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digital_lane_is_drawn_zero_to_one_whatever_the_samples_say() {
        assert_eq!(padded_span(&[0.0, 1.0], ""), (-0.15, 1.15));
        assert_eq!(level_label(1.0, ""), "1");
        assert_eq!(level_label(0.0, ""), "0");
        assert_eq!(level_label(1.5, "V"), "1.50 V");
    }

    #[test]
    fn a_flat_analog_trace_still_gets_a_span_to_draw_in() {
        let (lo, hi) = padded_span(&[2.5, 2.5], "V");
        assert!(lo < 2.5 && hi > 2.5, "{lo}..{hi}");
        // No samples at all must not produce NaN bounds.
        let (lo, hi) = padded_span(&[], "V");
        assert!(lo.is_finite() && hi.is_finite() && lo < hi);
    }

    #[test]
    fn an_edge_on_a_column_boundary_is_still_drawn() {
        // Column 0 all high, column 1 all low: each column's own range strokes
        // one level only, and the falling edge between them went undrawn.
        let band = [(1.0, 1.0), (0.0, 0.0), (0.0, 0.0), (1.0, 1.0)];
        let got = connected_band(&band);
        assert_eq!(got[0], (1.0, 1.0));
        assert_eq!(got[1], (0.0, 1.0), "the falling edge spans both levels");
        assert_eq!(got[2], (0.0, 0.0));
        assert_eq!(got[3], (0.0, 1.0), "and the rising edge too");
        // Every consecutive pair of drawn bands now touches: no gap is left
        // anywhere in the trace, which is the property the view needs.
        for pair in got.windows(2) {
            let ((alo, ahi), (blo, bhi)) = (pair[0], pair[1]);
            assert!(ahi >= blo && bhi >= alo, "gap between {pair:?}");
        }
    }

    #[test]
    fn columns_that_already_overlap_are_left_alone() {
        let band = [(0.0, 1.0), (0.2, 0.8), (0.4, 0.9)];
        assert_eq!(connected_band(&band), band.to_vec());
        assert!(connected_band(&[]).is_empty());
        assert_eq!(connected_band(&[(2.5, 2.5)]), vec![(2.5, 2.5)]);
    }

    #[test]
    fn density_levels_are_logarithmic_and_bounded() {
        let log_max = (1000.0f64 + 1.0).ln();
        assert_eq!(density_level(0, log_max, 6), 0);
        assert_eq!(density_level(1000, log_max, 6), 6);
        assert!(density_level(1, log_max, 6) >= 1, "any hit is visible");
        // A sparse cell must not vanish into the same band as a dense one.
        assert!(density_level(2, log_max, 6) < density_level(900, log_max, 6));
        assert_eq!(density_level(5, 0.0, 6), 0, "no data, no bands");
    }
}
