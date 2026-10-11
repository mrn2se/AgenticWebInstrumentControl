//! The histogram and the anomaly digest (SPECIFICATION.md §5.6.3, §5.6.4).

use crate::svg::{Anchor, Svg};
use crate::timing::{self, IntervalKind, Population, TimingAnalysis};
use crate::units::{log_ticks, si, tick_labels, ticks};
use crate::{
    theme, Finding, Metric, Trace, View, ViewRequest, FOOTER, M_BOTTOM, M_LEFT, M_RIGHT, M_TOP,
};

/// How many outliers the digest names. Beyond a handful, the useful answer is
/// the *count* plus the worst few — a list of four thousand timestamps is the
/// samples again, in a different shape.
pub const MAX_NAMED_OUTLIERS: usize = 12;

/// Default tolerance for the digest: a period more than this fraction away
/// from the dominant one is called out. Five percent is loose enough to
/// ignore sampling quantisation on a period of a few hundred samples and tight
/// enough to catch a stutter.
pub const DEFAULT_TOLERANCE: f64 = 0.05;
/// Intervals the anomalies plot draws one marker each for; more are drawn as a
/// per-column band (B-42).
const MAX_MARKERS: usize = 3000;
/// Outliers that keep their own marker once the plot is banded.
const MAX_OUTLIER_MARKERS: usize = 400;
/// Width in pixels of one column of the banded plot.
const BAND_PX: f64 = 2.0;

/// A histogram of interval lengths, with the populations marked.
pub(crate) fn draw_histogram(
    request: &ViewRequest,
    width: u32,
    height: u32,
    rate_hz: f64,
    trace: &Trace,
) -> Result<View, String> {
    let analysis = timing::analyze(&trace.values, rate_hz)?;
    let kind = request.interval_kind.unwrap_or(IntervalKind::Period);
    let (intervals, populations) = match kind {
        IntervalKind::High => (&analysis.high_intervals, &analysis.high),
        IntervalKind::Low => (&analysis.low_intervals, &analysis.low),
        IntervalKind::Period => (&analysis.period_intervals, &analysis.periods),
    };
    if intervals.is_empty() {
        return Err(format!(
            "no complete {} intervals in this window",
            kind.as_str()
        ));
    }

    let plot_w = f64::from(width) - M_LEFT - M_RIGHT;
    let plot_h = f64::from(height) - M_TOP - M_BOTTOM - FOOTER;
    let mut svg = Svg::new(width, height);
    svg.rect(0.0, 0.0, f64::from(width), f64::from(height), theme::BG);
    svg.rect(M_LEFT, M_TOP, plot_w, plot_h, theme::PANEL);

    // A log axis, because a capture can hold 80 ns glitches next to 1 ms
    // pulses and a linear axis puts every one of them in the leftmost pixel.
    let shortest = intervals
        .iter()
        .map(|&(_, l)| l)
        .fold(f64::INFINITY, f64::min);
    let longest = intervals
        .iter()
        .map(|&(_, l)| l)
        .fold(f64::NEG_INFINITY, f64::max);
    let (axis_lo, axis_hi) = (shortest * 0.7, longest * 1.4);
    let x_of = |length: f64| {
        let t = (length.max(f64::MIN_POSITIVE).log10() - axis_lo.log10())
            / (axis_hi.log10() - axis_lo.log10()).max(f64::MIN_POSITIVE);
        M_LEFT + t.clamp(0.0, 1.0) * plot_w
    };

    // Bars: one per pixel column, counting the intervals that land there.
    let columns = (plot_w.max(1.0) as usize).min(2000);
    let mut counts = vec![0usize; columns];
    for &(_, length) in intervals {
        let t = (length.max(f64::MIN_POSITIVE).log10() - axis_lo.log10())
            / (axis_hi.log10() - axis_lo.log10()).max(f64::MIN_POSITIVE);
        let column = ((t.clamp(0.0, 1.0) * (columns - 1) as f64).round() as usize).min(columns - 1);
        counts[column] += 1;
    }
    let tallest = counts.iter().copied().max().unwrap_or(1).max(1);
    let mut bars = String::new();
    let bar_w = plot_w / columns as f64;
    for (column, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let h = count as f64 / tallest as f64 * (plot_h - 4.0);
        bars.push_str(&format!(
            "M{:.1} {:.1}h{:.2}v{:.1}h-{:.2}z",
            M_LEFT + column as f64 * bar_w,
            M_TOP + plot_h - h,
            bar_w.max(0.8),
            h,
            bar_w.max(0.8)
        ));
    }
    svg.path_alpha(&bars, theme::TRACE[0], 0.85);

    for t in log_ticks(axis_lo, axis_hi) {
        let x = x_of(t);
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
    for count in ticks(0.0, tallest as f64, 4) {
        let y = M_TOP + plot_h - count / tallest as f64 * (plot_h - 4.0);
        svg.text(
            M_LEFT - 8.0,
            y + 3.5,
            &format!("{count:.0}"),
            theme::MUTED,
            9.0,
            Anchor::End,
        );
    }

    // Each population marked where it sits, biggest share first.
    for (i, p) in populations.iter().enumerate().take(4) {
        let x = x_of(p.mean_s);
        svg.dashed(x, M_TOP, x, M_TOP + plot_h, theme::MARKER);
        svg.text(
            x + 4.0,
            M_TOP + 12.0 + i as f64 * 13.0,
            &format!("{} ({:.0} %)", si(p.mean_s, "s"), p.share * 100.0),
            theme::MARKER,
            10.0,
            Anchor::Start,
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
            "{} {} intervals, {} population(s)",
            trace.name,
            kind.as_str(),
            populations.len()
        )
    });
    svg.text_bold(M_LEFT, 20.0, &title, theme::TEXT, 13.0, Anchor::Start);
    svg.text(
        f64::from(width) - M_RIGHT,
        20.0,
        &format!("{} intervals, log axis", intervals.len()),
        theme::MUTED,
        11.0,
        Anchor::End,
    );

    let metrics = histogram_metrics(&analysis, kind, intervals.len(), populations);
    let findings = populations
        .iter()
        .enumerate()
        .map(|(i, p)| Finding {
            at_s: None,
            label: format!("population_{}", i + 1),
            detail: format!(
                "{} of {} intervals ({:.1} %) at {}, spread {} to {}",
                p.count,
                intervals.len(),
                p.share * 100.0,
                si(p.mean_s, "s"),
                si(p.min_s, "s"),
                si(p.max_s, "s")
            ),
        })
        .collect();
    crate::waveform::footer(&mut svg, width, height, &metrics);
    let desc = crate::waveform::describe(&title, &metrics, &analysis.notes);
    Ok(View {
        svg: svg.finish(&title, &desc),
        width,
        height,
        mode: "histogram",
        channels: vec![trace.channel],
        metrics,
        findings,
        notes: analysis.notes.clone(),
    })
}

fn histogram_metrics(
    analysis: &TimingAnalysis,
    kind: IntervalKind,
    total: usize,
    populations: &[Population],
) -> Vec<Metric> {
    let mut m = vec![
        Metric::plain("intervals_measured", kind.as_str()),
        Metric::count("intervals", total),
        Metric::count("populations", populations.len()),
        Metric::count("glitches", analysis.glitches),
    ];
    for (i, p) in populations.iter().enumerate() {
        let n = i + 1;
        m.push(Metric::new(
            &format!("population_{n}_length"),
            p.mean_s,
            "s",
        ));
        m.push(Metric::new(
            &format!("population_{n}_share"),
            p.share * 100.0,
            "%",
        ));
        m.push(Metric::count(&format!("population_{n}_count"), p.count));
        // The spread inside a population is how much jitter there is *within*
        // one kind of pulse, which a mean alone hides.
        m.push(Metric::new(
            &format!("population_{n}_spread"),
            p.max_s - p.min_s,
            "s",
        ));
    }
    if let Some(dominant) = populations.first() {
        m.push(Metric::new(
            "dominant_rate",
            1.0 / dominant.mean_s.max(f64::MIN_POSITIVE),
            "Hz",
        ));
    }
    m
}

/// The anomaly digest: how many intervals do not fit, and where the worst are.
pub(crate) fn draw_anomalies(
    request: &ViewRequest,
    width: u32,
    height: u32,
    rate_hz: f64,
    trace: &Trace,
) -> Result<View, String> {
    let analysis = timing::analyze(&trace.values, rate_hz)?;
    let kind = request.interval_kind.unwrap_or(IntervalKind::Period);
    let (intervals, populations) = match kind {
        IntervalKind::High => (&analysis.high_intervals, &analysis.high),
        IntervalKind::Low => (&analysis.low_intervals, &analysis.low),
        IntervalKind::Period => (&analysis.period_intervals, &analysis.periods),
    };
    if intervals.is_empty() {
        return Err(format!(
            "no complete {} intervals in this window",
            kind.as_str()
        ));
    }
    let tolerance = request.tolerance.unwrap_or(DEFAULT_TOLERANCE).abs();
    let (reference, worst) = timing::outliers(
        intervals,
        populations,
        tolerance,
        MAX_NAMED_OUTLIERS,
        (timing::GLITCH_SAMPLES as f64 + 0.5) / rate_hz,
    );
    let Some(reference_kind) = reference else {
        return Err("no dominant interval to compare against".to_string());
    };
    let reference = reference_kind.length_s();
    // Signed deviation per interval; `None` for an idle gap on NRZ data.
    let deviation = |l: f64| reference_kind.deviation(l).map(|(d, _)| d);
    let judged: Vec<(f64, f64)> = intervals
        .iter()
        .filter_map(|&(at, l)| deviation(l).map(|d| (at, d)))
        .collect();
    let idle_gaps = intervals.len() - judged.len();
    let total_outliers = judged.iter().filter(|&&(_, d)| d.abs() > tolerance).count();

    let plot_w = f64::from(width) - M_LEFT - M_RIGHT;
    let plot_h = f64::from(height) - M_TOP - M_BOTTOM - FOOTER;
    let mut svg = Svg::new(width, height);
    svg.rect(0.0, 0.0, f64::from(width), f64::from(height), theme::BG);
    svg.rect(M_LEFT, M_TOP, plot_w, plot_h, theme::PANEL);

    // Deviation against time: the shape of the problem. A single spike is a
    // stutter, a drift is a clock warming up, a band is jitter — and none of
    // those is distinguishable from the count alone.
    let t0 = intervals.first().map(|&(t, _)| t).unwrap_or(0.0);
    let t1 = intervals.last().map(|&(t, _)| t).unwrap_or(t0 + 1.0);
    let worst_deviation = judged.iter().map(|&(_, d)| d.abs()).fold(0.0f64, f64::max);
    let scale = worst_deviation.max(tolerance * 2.0) * 1.15;
    let x_of =
        |t: f64| M_LEFT + ((t - t0) / (t1 - t0).max(f64::MIN_POSITIVE)).clamp(0.0, 1.0) * plot_w;
    let y_of = |d: f64| M_TOP + plot_h / 2.0 - (d / scale).clamp(-1.0, 1.0) * (plot_h / 2.0 - 4.0);

    // The tolerance band, so "inside" and "outside" are visible at a glance.
    svg.rect(
        M_LEFT,
        y_of(tolerance),
        plot_w,
        y_of(-tolerance) - y_of(tolerance),
        theme::GRID,
    );
    svg.dashed(M_LEFT, y_of(0.0), M_LEFT + plot_w, y_of(0.0), theme::AXIS);
    svg.text(
        M_LEFT + plot_w - 4.0,
        y_of(tolerance) - 4.0,
        &format!("±{:.0} % tolerance", tolerance * 100.0),
        theme::MUTED,
        9.0,
        Anchor::End,
    );

    // A few thousand markers are individually readable; past that they are a
    // smear that cost 542 KB on 21 000 periods (B-42). So the points of a long
    // capture are folded into one band per two pixels of width — the range of
    // deviations that column holds — and only the worst outliers keep a marker
    // of their own. Every number reported comes from the data, not the plot.
    let mut inside = String::new();
    let mut outside = String::new();
    if judged.len() <= MAX_MARKERS {
        for &(at, d) in &judged {
            let (x, y) = (x_of(at), y_of(d));
            let target = if d.abs() > tolerance {
                &mut outside
            } else {
                &mut inside
            };
            target.push_str(&format!("M{:.1} {:.1}h1.4v1.4h-1.4z", x, y));
        }
    } else {
        let columns = ((plot_w / BAND_PX).ceil() as usize).max(1);
        // (lowest, highest) deviation per column, for each population.
        let mut inside_band = vec![(f64::INFINITY, f64::NEG_INFINITY); columns];
        let mut outside_band = vec![(f64::INFINITY, f64::NEG_INFINITY); columns];
        let mut outside_points: Vec<(f64, f64)> = Vec::new();
        for &(at, d) in &judged {
            let column = (((x_of(at) - M_LEFT) / BAND_PX) as usize).min(columns - 1);
            let (band, is_out) = if d.abs() > tolerance {
                (&mut outside_band[column], true)
            } else {
                (&mut inside_band[column], false)
            };
            band.0 = band.0.min(d);
            band.1 = band.1.max(d);
            if is_out {
                outside_points.push((at, d));
            }
        }
        // The worst markers stay individually visible, over the band.
        outside_points.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));
        for &(at, d) in outside_points.iter().take(MAX_OUTLIER_MARKERS) {
            outside.push_str(&format!("M{:.1} {:.1}h1.4v1.4h-1.4z", x_of(at), y_of(d)));
        }
        let draw = |bands: &[(f64, f64)]| {
            let mut path = String::new();
            for (column, &(lo, hi)) in bands.iter().enumerate() {
                if lo > hi {
                    continue;
                }
                let x = M_LEFT + column as f64 * BAND_PX;
                let (top, bottom) = (y_of(hi), y_of(lo));
                path.push_str(&format!(
                    "M{:.0} {:.0}h{}v{:.0}h-{}z",
                    x,
                    top,
                    BAND_PX,
                    (bottom - top).max(1.4),
                    BAND_PX
                ));
            }
            path
        };
        svg.path_alpha(&draw(&inside_band), theme::TRACE[0], 0.35);
        svg.path_alpha(&draw(&outside_band), theme::MARKER, 0.35);
    }
    svg.path_alpha(&inside, theme::TRACE[0], 0.55);
    svg.path_alpha(&outside, theme::MARKER, 1.0);

    // Labelled from the spacing, as the waveform's axis is (B-53).
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
    for d in [-scale, -scale / 2.0, 0.0, scale / 2.0, scale] {
        svg.text(
            M_LEFT - 8.0,
            y_of(d) + 3.5,
            &format!("{:+.0} %", d * 100.0),
            theme::MUTED,
            9.0,
            Anchor::End,
        );
    }

    // The worst few get a mark, so the picture and the list agree.
    for outlier in worst.iter().take(4) {
        let (x, y) = (x_of(outlier.at_s), y_of(outlier.deviation));
        svg.line(x, y - 6.0, x, y + 6.0, theme::MARKER, 1.0);
        svg.text(
            x,
            y - 9.0,
            &si(outlier.at_s, "s"),
            theme::MARKER,
            9.0,
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
            "{}: {} of {} {}s outside ±{:.0} %",
            trace.name,
            total_outliers,
            intervals.len(),
            kind.as_str(),
            tolerance * 100.0
        )
    });
    svg.text_bold(M_LEFT, 20.0, &title, theme::TEXT, 13.0, Anchor::Start);
    svg.text(
        f64::from(width) - M_RIGHT,
        20.0,
        &match reference_kind {
            timing::Reference::Single(_) => format!("reference {}", si(reference, "s")),
            timing::Reference::Nrz { .. } => {
                format!("NRZ data, unit interval {}", si(reference, "s"))
            }
        },
        theme::MUTED,
        11.0,
        Anchor::End,
    );

    let mut notes = analysis.notes.clone();
    if let timing::Reference::Nrz { .. } = reference_kind {
        let longest = judged
            .iter()
            .zip(intervals.iter().filter(|&&(_, l)| deviation(l).is_some()))
            .map(|(_, &(_, l))| (l / reference).round() as u32)
            .max()
            .unwrap_or(1);
        notes.push(format!(
            "NRZ data: the {}s are whole numbers of a {} unit interval (1 to {longest} seen), so \
             each is held against the nearest whole number of units, and its deviation is a \
             fraction of one unit — a run of several bits is not an anomaly (B-102)",
            kind.as_str(),
            si(reference, "s")
        ));
        if idle_gaps > 0 {
            notes.push(format!(
                "{idle_gaps} interval(s) longer than {} units are idle gaps between frames or \
                 bytes and are not judged",
                timing::NRZ_MAX_RUN
            ));
        }
    }
    // A tolerance finer than one sample flags the sampler, not the signal:
    // at 4 MS/s on a 2 us bit, an edge one sample late is 12.5 % off.
    let sample_share = 1.0 / rate_hz / reference;
    if total_outliers > 0 && tolerance < sample_share {
        notes.push(format!(
            "the ±{:.0} % tolerance is {} — finer than one sample ({}, {:.1} % of the {}): an \
             edge one sample early or late is already an outlier. Use a tolerance of at least \
             {:.2} or capture faster",
            tolerance * 100.0,
            si(tolerance * reference, "s"),
            si(1.0 / rate_hz, "s"),
            sample_share * 100.0,
            si(reference, "s"),
            (sample_share * 1.05 * 100.0).ceil() / 100.0
        ));
    }
    if total_outliers > worst.len() {
        notes.push(format!(
            "{total_outliers} intervals are outside the tolerance; the {} worst are named. \
             Narrow the window with t0_s/t1_s to see the rest.",
            worst.len()
        ));
    }
    if total_outliers == 0 {
        notes.push(format!(
            "nothing outside ±{:.0} % of {}: every {} in this window is where it should be",
            tolerance * 100.0,
            si(reference, "s"),
            kind.as_str()
        ));
    }

    let mut metrics = vec![
        Metric::plain("intervals_measured", kind.as_str()),
        Metric::count("intervals", intervals.len()),
        Metric::new("reference", reference, "s"),
        Metric::plain(
            "reference_kind",
            match reference_kind {
                timing::Reference::Single(_) => "dominant interval",
                timing::Reference::Nrz { .. } => "unit interval (NRZ data)",
            },
        ),
        Metric::new("tolerance", tolerance * 100.0, "%"),
        Metric::count("outliers", total_outliers),
        Metric::new(
            "outlier_share",
            total_outliers as f64 / intervals.len() as f64 * 100.0,
            "%",
        ),
        Metric::new("worst_deviation", worst_deviation * 100.0, "%"),
    ];
    if idle_gaps > 0 {
        metrics.push(Metric::count("idle_gaps", idle_gaps));
    }
    let findings = worst
        .iter()
        .map(|o| Finding {
            at_s: Some(o.at_s),
            label: format!("{} at {}", kind.as_str(), si(o.at_s, "s")),
            detail: match o.units {
                None => format!(
                    "{} — {:+.1} % of the {} reference",
                    si(o.length_s, "s"),
                    o.deviation * 100.0,
                    si(reference, "s")
                ),
                Some(n) => format!(
                    "{} — {:+.1} % of a unit interval off {n} × {}",
                    si(o.length_s, "s"),
                    o.deviation * 100.0,
                    si(reference, "s")
                ),
            },
        })
        .collect();
    crate::waveform::footer(&mut svg, width, height, &metrics);
    let desc = crate::waveform::describe(&title, &metrics, &notes);
    Ok(View {
        svg: svg.finish(&title, &desc),
        width,
        height,
        mode: "anomalies",
        channels: vec![trace.channel],
        metrics,
        findings,
        notes,
    })
}
