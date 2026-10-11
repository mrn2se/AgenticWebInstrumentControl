//! Eye diagram: every unit interval of a signal folded onto the same axes and
//! accumulated, so what is normally a wall of edges becomes one picture of
//! how repeatable the signal is (SPECIFICATION.md §5.6.2).
//!
//! The analysis is kept separate from the drawing on purpose. The numbers —
//! eye height and width, the two logic levels, crossing jitter — are the part
//! an agent actually reasons with; the shading is how a person (or a vision
//! model) sees *why* those numbers came out as they did. So
//! [`analyze`] can be called on its own, with no SVG anywhere.

/// Which of two genuinely different pictures the fold produced, and therefore
/// which definition the openings were measured with.
///
/// This is not a detail to hide. A *clock* folded on its own period lands
/// every trace on top of every other, so no phase has samples both above and
/// below the threshold and a textbook eye measurement measures nothing — the
/// meaningful numbers are how long the signal stays settled and how much room
/// the two levels leave a threshold. *Random data* is the opposite: every
/// phase carries both symbols, and the opening between them is the whole
/// point. Reporting one number without saying which case it came from would
/// be reporting a different quantity depending on the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeKind {
    /// Every folded trace agrees at each phase: a clock, a CAL output, a
    /// repeating frame. `eye_width_s` is the shorter of the two settled
    /// stretches — how long a reader may sample and still be sure of the
    /// level.
    Persistence,
    /// Phases carry both levels: data. `eye_width_s` is the classic opening
    /// at the decision threshold.
    Data,
    /// A periodic signal that never sits at two levels — a sine, a triangle,
    /// a heavily filtered square (B-74). It folds like a persistence view, but
    /// with no flat tops there is no opening, no settled stretch and no
    /// decision threshold to measure: those fields are `None`, and only the
    /// period and the crossing jitter mean anything.
    Periodic,
}

/// How the unit interval was arrived at, which changes how much the rest of
/// the reading is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiSource {
    /// The caller gave it.
    Given,
    /// Recovered from the median interval between like-going mid-level
    /// crossings: the period of a clock.
    Recovered,
    /// Recovered from the shortest run between any two crossings, because the
    /// runs on this line are whole multiples of it in three or more lengths —
    /// NRZ data, where a run of equal bits carries no crossing at all and
    /// like-going crossings are therefore multiples of the bit time (B-57).
    RecoveredFromRuns,
}

/// The folded density plus everything measured while folding.
#[derive(Debug, Clone)]
pub struct EyeAnalysis {
    /// `cols * rows` counts, row 0 = the top of the voltage range.
    pub density: Vec<u32>,
    pub cols: usize,
    pub rows: usize,
    pub max_count: u32,
    /// The unit interval used, seconds, and where it came from.
    pub ui_s: f64,
    pub ui_source: UiSource,
    /// Which definition the openings below were measured with.
    pub kind: EyeKind,
    /// How many unit intervals were overlaid.
    pub traces: usize,
    /// Vertical extent of the plot, in the series' own unit.
    pub y_lo: f64,
    pub y_hi: f64,
    /// The two level populations, as means of the samples in the middle fifth
    /// of each unit interval (where a signal is settled).
    pub level_low: f64,
    pub level_high: f64,
    /// Vertical opening: the largest gap anywhere in the window between the
    /// lowest "high" sample and the highest "low" sample at the same phase.
    /// Found rather than assumed, because where the eye is widest depends on
    /// the duty cycle — a 20 %-duty clock opens nowhere near the middle.
    pub eye_height: Option<f64>,
    /// Where that opening is, as a fraction of the two-interval window.
    pub eye_phase: Option<f64>,
    /// Horizontal opening, seconds — see [`EyeKind`] for which of the two
    /// things it is.
    pub eye_width_s: Option<f64>,
    /// [`EyeKind::Persistence`] only: how long the signal stays settled high
    /// and settled low within one period. The shorter of the two is what
    /// limits when a reader may sample, and the pair is the duty cycle.
    pub settled_high_s: Option<f64>,
    pub settled_low_s: Option<f64>,
    /// Threshold the openings are measured against — midway between the
    /// levels.
    pub threshold: f64,
    /// Time interval error, peak to peak, seconds: how far the crossings stray
    /// from a best-fit periodic grid. This is the jitter that closes an eye
    /// horizontally.
    pub tie_pp_s: Option<f64>,
    /// Cycle-to-cycle jitter, peak to peak, seconds: the largest change from
    /// one period to the next. A signal can have small cycle-to-cycle jitter
    /// and large TIE (slow wander) or the reverse, so both are reported —
    /// quoting one as "the jitter" is how the two get confused.
    pub cycle_to_cycle_pp_s: Option<f64>,
    /// How many bursts the crossings fell into, when TIE could be measured.
    /// More than one means `tie_pp_s` is the worst burst's (B-70).
    pub tie_bursts: Option<usize>,
    /// The period the TIE grid was fitted with, seconds. It is the *mean*
    /// interval, not the median used to fold, because fitting against a
    /// median that is off by a percent turns steady drift into apparent
    /// jitter that grows with capture length.
    pub fitted_period_s: Option<f64>,
    /// Why a field is `None`, when something could not be measured.
    pub notes: Vec<String>,
}

/// Fewer than this many unit intervals and there is no eye to speak of —
/// better to say so than to draw two traces and call it a diagram.
pub const MIN_TRACES: usize = 3;

/// A gap of more than this many unit intervals between *like-going* crossings
/// ends a burst (B-70).
///
/// Generous on purpose. The grid is fitted through rising crossings, and
/// between two of them a stuffed stream can hold a six-bit low run followed by
/// a six-bit high run - a dozen intervals with no rising edge, inside one
/// frame. An idle gap between frames is orders of magnitude longer (10 ms is
/// 5000 intervals on a 500 kbit/s bus), so anything between the two separates
/// bursts cleanly, and each frame starts on its own phase that one grid across
/// all of them would report as jitter.
pub const BURST_GAP_UIS: f64 = 20.0;

/// Below this many samples per unit interval the fold says nothing about the
/// shape of an edge: every column would be one or two samples wide, and the
/// picture would be an artefact of the sample grid rather than of the signal.
pub const MIN_SAMPLES_PER_UI: f64 = 8.0;

/// Folds `samples` (physical units, evenly spaced at `rate_hz`) into a
/// `cols × rows` density grid **two unit intervals wide**, aligned so that
/// every rising mid-level crossing sits a quarter of the way in. One whole
/// interval is then centred with half an interval of context on each side, so
/// both edges of a cycle are in view and the jitter on each is visible as
/// smear rather than being cropped off.
pub fn analyze(
    samples: &[f64],
    rate_hz: f64,
    cols: usize,
    rows: usize,
    ui_s: Option<f64>,
) -> Result<EyeAnalysis, String> {
    if rate_hz <= 0.0 {
        return Err("sample rate must be positive".to_string());
    }
    if samples.len() < 8 {
        return Err(format!(
            "only {} samples: an eye diagram needs a signal to fold",
            samples.len()
        ));
    }
    let cols = cols.clamp(16, 4096);
    let rows = rows.clamp(16, 4096);
    let mut notes = Vec::new();

    let (y_lo, y_hi) = span(samples);
    if y_hi <= y_lo {
        return Err(format!(
            "the signal is constant at {y_lo}: there are no edges to fold on"
        ));
    }
    let threshold = (y_lo + y_hi) / 2.0;
    // A band around the threshold, from the signal's own span. Without one, a
    // real edge that dithers across the mid level produces a cluster of
    // crossings a sample or two apart, and every way of recovering a unit
    // interval from them - the median between like-going crossings, or the runs
    // between any of them - reads that cluster instead of the signal. A 101 kHz
    // square on the bench came back as a 1.0-sample unit interval and the fold
    // was refused (B-72). `measure` has had hysteresis all along; the eye did
    // not.
    let hysteresis = (y_hi - y_lo) * HYSTERESIS_FRACTION;
    let crossings = crossings(samples, threshold, hysteresis);
    if crossings.rising.len() < 2 {
        return Err(format!(
            "{} rising crossing(s) of the mid level: not enough to find a period",
            crossings.rising.len()
        ));
    }

    let (ui_samples, ui_source) = match ui_s {
        Some(ui) if ui > 0.0 => (ui * rate_hz, UiSource::Given),
        _ => recover_ui(&crossings, &mut notes)?,
    };
    if ui_samples < MIN_SAMPLES_PER_UI {
        return Err(format!(
            "the period is only {ui_samples:.1} samples long, under the {MIN_SAMPLES_PER_UI:.0} a \
             fold needs; capture at a higher rate"
        ));
    }

    // Two UI wide, the rising crossing a quarter of the way in.
    let window = 2.0 * ui_samples;
    let lead = 0.5 * ui_samples;
    let mut density = vec![0u32; cols * rows];
    let mut max_count = 0u32;
    let mut traces = 0usize;
    // Settled-level and opening statistics, gathered over the same fold.
    let mut low_sum = (0.0, 0usize);
    let mut high_sum = (0.0, 0usize);
    // Per column: the lowest sample above the threshold and the highest below
    // it. That pair is what the eye's opening is made of.
    let mut col_min_high = vec![f64::INFINITY; cols];
    let mut col_max_low = vec![f64::NEG_INFINITY; cols];

    for &start in &crossings.rising {
        let from = start - lead;
        if from < 0.0 || from + window >= (samples.len() - 1) as f64 {
            continue;
        }
        traces += 1;
        for col in 0..cols {
            let at = from + window * (col as f64 + 0.5) / cols as f64;
            let v = interpolate(samples, at);
            let row = value_row(v, y_lo, y_hi, rows);
            let cell = row * cols + col;
            let count = &mut density[cell];
            *count += 1;
            max_count = max_count.max(*count);
            if v >= threshold {
                col_min_high[col] = col_min_high[col].min(v);
            } else {
                col_max_low[col] = col_max_low[col].max(v);
            }
            // Every sample contributes to its own population's mean: the two
            // levels are what the signal spends its time at, and restricting
            // this to some assumed "settled" phase window would bake a duty
            // cycle into the answer.
            if v >= threshold {
                high_sum.0 += v;
                high_sum.1 += 1;
            } else {
                low_sum.0 += v;
                low_sum.1 += 1;
            }
        }
    }

    if traces < MIN_TRACES {
        // The count is of two-period fold windows, each reaching half a period
        // before its edge and a period and a half after it, so a capture of N
        // periods yields about N - 2 of them — and the first edge can sit up
        // to a period in (plus a sample of slack at either end). Say so in the units a person captures in, and name
        // the length that works (B-45).
        let period_s = ui_samples / rate_hz;
        let span_s = samples.len() as f64 / rate_hz;
        let needed_periods = MIN_TRACES + 3;
        return Err(format!(
            "{} holds {:.1} periods of {}; an eye folds two-period windows (half a period of \
             lead, a period and a half after the edge) and needs at least {MIN_TRACES} of them, \
             found {traces} — capture at least {}",
            crate::units::si(span_s, "s"),
            span_s / period_s,
            crate::units::si(period_s, "s"),
            crate::units::si(period_s * needed_periods as f64, "s"),
        ));
    }

    let level_low = mean(low_sum).unwrap_or(y_lo);
    let level_high = mean(high_sum).unwrap_or(y_hi);

    let per_column_s = window / cols as f64 / rate_hz;
    let near = two_level_fraction(samples, threshold);
    let opening = if near < TWO_LEVEL_FRACTION {
        // A sine's crossings jitter by a sample, so the columns at its
        // zero crossings carry both "levels" with a sliver between them, and
        // the fold used to call it data with an eye height of millivolts
        // (B-74). With no flat tops there is nothing to open.
        notes.push(format!(
            "only {:.0} % of the samples sit near the signal's two extremes (a square wave or \
             data holds most of them there): this is a periodic analog signal, not two logic \
             levels, so no eye opening, settled stretch or threshold is measured — read the \
             period and the jitter",
            near * 100.0
        ));
        Opening {
            kind: EyeKind::Periodic,
            height: None,
            phase: None,
            width_s: None,
            settled_high_s: None,
            settled_low_s: None,
        }
    } else {
        measure_opening(&col_min_high, &col_max_low, per_column_s, &mut notes)
    };

    // On NRZ data a crossing is not one unit interval after the last one but
    // a whole number of them, so the grid TIE is measured against has to be
    // indexed by that number. Fitting consecutive indices to CAN crossings is
    // what turned a clean bus into `jitter_tie_pp` 16.2 ms (B-57).
    let indices = match ui_source {
        UiSource::Recovered | UiSource::Given => (0..crossings.rising.len())
            .map(|k| k as f64)
            .collect::<Vec<f64>>(),
        UiSource::RecoveredFromRuns => unit_indices(&crossings.rising, ui_samples),
    };
    let jitter = jitter(
        &crossings.rising,
        &indices,
        rate_hz,
        ui_samples,
        ui_source == UiSource::RecoveredFromRuns,
    );
    // TIE is measured against a grid of consecutive periods. A signal that
    // *stops* — an I2C clock between transactions, a burst — breaks that
    // assumption, and the gap shows up as jitter many times the unit interval.
    // Saying so is the difference between a useful number and a misleading one.
    if let Some(bursts) = jitter.tie_bursts.filter(|&b| b > 1) {
        notes.push(format!(
            "the crossings fall into {bursts} bursts separated by idle gaps; time interval error \
             is the worst burst's, each fitted against its own grid — one grid across all of them \
             would report the phase difference between bursts as jitter"
        ));
    }
    if let Some(tie) = jitter.tie_pp_s {
        if tie > ui_samples / rate_hz {
            notes.push(format!(
                "time interval error ({}) exceeds one unit interval even within a single burst: \
                 the crossings really are not where a periodic grid says they should be — read \
                 the cycle-to-cycle number as well",
                crate::units::si(tie, "s")
            ));
        }
    }

    Ok(EyeAnalysis {
        density,
        cols,
        rows,
        max_count,
        ui_s: ui_samples / rate_hz,
        ui_source,
        kind: opening.kind,
        traces,
        y_lo,
        y_hi,
        level_low,
        level_high,
        eye_height: opening.height,
        eye_phase: opening.phase,
        eye_width_s: opening.width_s,
        settled_high_s: opening.settled_high_s,
        settled_low_s: opening.settled_low_s,
        threshold,
        tie_pp_s: jitter.tie_pp_s,
        tie_bursts: jitter.tie_bursts,
        cycle_to_cycle_pp_s: jitter.cycle_to_cycle_pp_s,
        fitted_period_s: jitter.fitted_period_s,
        notes,
    })
}

/// A signal with two levels spends at least this share of its samples near
/// them: a square or NRZ line well over 90 %, a sine about 26 %, a triangle
/// 20 % (B-74).
const TWO_LEVEL_FRACTION: f64 = 0.5;

/// The share of `samples` within a tenth of the level difference of their
/// own level. The two levels are the medians of the samples above and below
/// `threshold` — what a square spends its time at; overshoot, ringing and a
/// stray spike move neither, where the extremes or the outer percentiles
/// would put a 28 %-overshooting square's settled top outside the band. On a
/// sine the median of the upper half is 0.71 of the amplitude, and only the
/// samples passing through that band count.
fn two_level_fraction(samples: &[f64], threshold: f64) -> f64 {
    let median = |mut v: Vec<f64>| -> Option<f64> {
        if v.is_empty() {
            return None;
        }
        let mid = v.len() / 2;
        v.select_nth_unstable_by(mid, f64::total_cmp);
        Some(v[mid])
    };
    let finite = samples.iter().copied().filter(|v| v.is_finite());
    let (Some(high), Some(low)) = (
        median(finite.clone().filter(|&v| v >= threshold).collect()),
        median(finite.clone().filter(|&v| v < threshold).collect()),
    ) else {
        return 1.0;
    };
    let band = (high - low) * 0.1;
    let near = finite
        .clone()
        .filter(|&v| (v - high).abs() <= band || (v - low).abs() <= band)
        .count();
    near as f64 / finite.count().max(1) as f64
}

/// What one column of the fold contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    /// Every trace was above the threshold here.
    High,
    /// Every trace was below it.
    Low,
    /// Both — the interesting case for data, an edge or jitter for a clock.
    Mixed,
}

struct Opening {
    kind: EyeKind,
    height: Option<f64>,
    phase: Option<f64>,
    width_s: Option<f64>,
    settled_high_s: Option<f64>,
    settled_low_s: Option<f64>,
}

/// The fraction trimmed from each end of a settled stretch before its level is
/// read. The columns next to a transition still hold the tail of the edge, and
/// including them would report the threshold itself as the level.
const SETTLE_TRIM: f64 = 0.2;

fn measure_opening(
    col_min_high: &[f64],
    col_max_low: &[f64],
    per_column_s: f64,
    notes: &mut Vec<String>,
) -> Opening {
    let cols = col_min_high.len();
    let classes: Vec<Column> = (0..cols)
        .map(
            |c| match (col_min_high[c].is_finite(), col_max_low[c].is_finite()) {
                (true, true) => Column::Mixed,
                (true, false) => Column::High,
                (false, true) => Column::Low,
                // No samples at all in this column cannot happen — every trace
                // contributes to every column — but classifying it as mixed keeps
                // it out of the settled runs either way.
                (false, false) => Column::Mixed,
            },
        )
        .collect();

    let gap_at = |c: usize| col_min_high[c] - col_max_low[c];
    let data_columns: Vec<usize> = (0..cols)
        .filter(|&c| classes[c] == Column::Mixed && gap_at(c) > 0.0)
        .collect();

    if !data_columns.is_empty() {
        // Data: the opening is the widest vertical gap between the two
        // symbols, and the width is the run of phases around it that stays
        // open.
        let best = data_columns
            .iter()
            .copied()
            .max_by(|&a, &b| {
                gap_at(a)
                    .partial_cmp(&gap_at(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty");
        let open = |c: usize| classes[c] != Column::Mixed || gap_at(c) > 0.0;
        let (lo, hi) = run_around(best, cols, open);
        // The window is two unit intervals, and an eye cannot be open for
        // longer than one (B-46). A run that reaches that far never closed:
        // on a logic line, whose samples are exactly 0 or 1, no sample ever
        // sits between the levels, so every column reads as open.
        let ui_cols = cols / 2;
        let run = hi - lo + 1;
        if run >= ui_cols {
            notes.push(
                "the eye never closes: no sample falls between the two levels anywhere in \
                 the period, so eye_width is capped at one unit interval and says nothing \
                 about the edges — read the jitter numbers instead"
                    .to_string(),
            );
        }
        return Opening {
            kind: EyeKind::Data,
            height: Some(gap_at(best)),
            phase: Some((best as f64 + 0.5) / cols as f64),
            width_s: Some(run.min(ui_cols) as f64 * per_column_s),
            settled_high_s: None,
            settled_low_s: None,
        };
    }

    // Persistence: no phase carries both levels, so what matters is how long
    // each level is held and how far apart the two levels are once settled.
    let high_run = longest_run(&classes, Column::High);
    let low_run = longest_run(&classes, Column::Low);
    let (Some(high), Some(low)) = (high_run, low_run) else {
        notes.push(
            "the fold never settles on both levels, so there is no opening to measure: the \
             signal may be slower than one period of the window, or all edge"
                .to_string(),
        );
        return Opening {
            kind: EyeKind::Persistence,
            height: None,
            phase: None,
            width_s: None,
            settled_high_s: high_run.map(|(a, b)| (b - a + 1) as f64 * per_column_s),
            settled_low_s: low_run.map(|(a, b)| (b - a + 1) as f64 * per_column_s),
        };
    };

    let worst_high = trimmed(high, SETTLE_TRIM)
        .map(|c| col_min_high[c])
        .fold(f64::INFINITY, f64::min);
    let worst_low = trimmed(low, SETTLE_TRIM)
        .map(|c| col_max_low[c])
        .fold(f64::NEG_INFINITY, f64::max);
    let high_s = (high.1 - high.0 + 1) as f64 * per_column_s;
    let low_s = (low.1 - low.0 + 1) as f64 * per_column_s;
    // The shorter settled stretch is the limiting one: it is how long a
    // reader may sample and still be certain of the level.
    let (limiting, limiting_centre) = if high_s <= low_s {
        (high_s, (high.0 + high.1) as f64 / 2.0)
    } else {
        (low_s, (low.0 + low.1) as f64 / 2.0)
    };
    Opening {
        kind: EyeKind::Persistence,
        height: (worst_high.is_finite() && worst_low.is_finite())
            .then(|| (worst_high - worst_low).max(0.0)),
        phase: Some((limiting_centre + 0.5) / cols as f64),
        width_s: Some(limiting),
        settled_high_s: Some(high_s),
        settled_low_s: Some(low_s),
    }
}

/// The widest run of consecutive columns of exactly `want`, as an inclusive
/// range.
fn longest_run(classes: &[Column], want: Column) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut start: Option<usize> = None;
    for c in 0..=classes.len() {
        let matches = c < classes.len() && classes[c] == want;
        match (matches, start) {
            (true, None) => start = Some(c),
            (false, Some(s)) => {
                let run = (s, c - 1);
                if best.is_none_or(|b| run.1 - run.0 > b.1 - b.0) {
                    best = Some(run);
                }
                start = None;
            }
            _ => {}
        }
    }
    best
}

/// The middle of an inclusive column range, with `fraction` trimmed off each
/// end — never empty, so a one-column run still yields that column.
fn trimmed((lo, hi): (usize, usize), fraction: f64) -> impl Iterator<Item = usize> {
    let len = hi - lo + 1;
    let cut = ((len as f64) * fraction).floor() as usize;
    let (a, b) = if len > 2 * cut + 1 {
        (lo + cut, hi - cut)
    } else {
        (lo, hi)
    };
    a..=b
}

/// Grows outward from `seed` while `open` holds, as an inclusive range.
fn run_around(seed: usize, cols: usize, open: impl Fn(usize) -> bool) -> (usize, usize) {
    let mut lo = seed;
    while lo > 0 && open(lo - 1) {
        lo -= 1;
    }
    let mut hi = seed;
    while hi + 1 < cols && open(hi + 1) {
        hi += 1;
    }
    (lo, hi)
}

struct Crossings {
    rising: Vec<f64>,
    /// Every crossing, both directions, in time order. The runs between them
    /// are what carry the unit interval on a line whose like-going crossings
    /// are multiples of it.
    all: Vec<f64>,
}

/// Fractional sample positions where the signal crosses `threshold` upward.
/// Linear interpolation between the bracketing samples, so the crossing time
/// is not quantised to the sample grid — which matters, because the jitter
/// being measured is often smaller than one sample.
fn crossings(samples: &[f64], threshold: f64, hysteresis: f64) -> Crossings {
    let mut rising = Vec::new();
    let mut all = Vec::new();
    // Schmitt trigger: a transition counts once the signal has reached the far
    // side of the band, and its *time* is still where it crossed the threshold
    // itself, so the hysteresis costs no timing accuracy.
    let (high, low) = (threshold + hysteresis, threshold - hysteresis);
    let mut above = samples.first().is_some_and(|&v| v >= threshold);
    for i in 1..samples.len() {
        let v = samples[i];
        if !above && v >= high {
            above = true;
            all.push(threshold_crossing(samples, i, threshold));
            rising.push(*all.last().expect("just pushed"));
        } else if above && v <= low {
            above = false;
            all.push(threshold_crossing(samples, i, threshold));
        }
    }
    Crossings { rising, all }
}

/// Where the signal last crossed `threshold` at or before `index`, as a
/// fractional sample position interpolated between the bracketing samples.
fn threshold_crossing(samples: &[f64], index: usize, threshold: f64) -> f64 {
    for i in (1..=index).rev() {
        let (a, b) = (samples[i - 1], samples[i]);
        let straddles = (a < threshold) != (b < threshold);
        if straddles {
            let t = if (b - a).abs() > f64::EPSILON {
                (threshold - a) / (b - a)
            } else {
                0.0
            };
            return (i - 1) as f64 + t;
        }
    }
    0.0
}

/// Share of runs that must be whole multiples of the shortest one for the line
/// to be read as data rather than as something whose edges are not on a grid.
const MIN_RUN_FIT: f64 = 0.85;
/// How far (in units) a run may be from a whole number of them.
const RUN_TOLERANCE: f64 = 0.2;
/// Half-width of the dead band around the eye's threshold, as a fraction of the
/// signal's own peak-to-peak span (B-72). Wide enough to reject the dither of a
/// real edge through the mid level, narrow enough that a signal with a genuinely
/// small opening still produces crossings.
const HYSTERESIS_FRACTION: f64 = 0.1;

/// A run this short between two crossings is ringing at the threshold, not a
/// bit. The eye's crossings are taken against a plain mid level with no
/// hysteresis - unlike `measure`, which has one - so a real signal that sits
/// near the threshold for a sample or two produces runs of one or two samples
/// that no bit time explains (B-72).
const MIN_RUN_SAMPLES: f64 = 3.0;

/// The commonest run, in units, allowed for a line to read as NRZ data.
///
/// Without this the whole-multiple test is trivially satisfiable: with a unit
/// of one sample *every* run is a whole number of them, so a noisy clock at
/// 101 kHz and 8 MS/s read its unit interval as 1.0 samples and the fold was
/// refused outright (B-72). A data line's commonest run is a small number of
/// bits - CAN's is five - so a unit tens of times shorter than the commonest
/// run is a divisor of the real one, not the real one.
const MAX_MEDIAN_RUN_UNITS: f64 = 8.0;

/// How many different run lengths make a line NRZ data. One is a clock, two is
/// a PWM or a duty-cycled clock - and for both of those the period between
/// like-going crossings *is* the unit interval. Three or more different
/// lengths is a data stream.
const MIN_RUN_LENGTHS: usize = 3;

/// The unit interval in samples, recovered from the crossing times.
///
/// The median interval between rising crossings is the period of a clock, and
/// for a clock it is the right answer. On NRZ data it is not: a run of equal
/// bits carries no crossing, so like-going crossings are whole multiples of
/// the bit time apart and their median is whichever multiple happens to be
/// commonest. On a 500 kbit/s CAN bus - 2 us bits - that read 6.00 us, and
/// every number measured against it was wrong by the same factor (B-57).
///
/// So the runs between *any* two crossings are measured first, the same way
/// the decoders find a bit time: a low percentile of them, then the mean of
/// the runs near it, which is one unit. If the runs are whole multiples of
/// that unit in [`MIN_RUN_LENGTHS`] or more different lengths, the line is
/// data and the unit is the interval. Otherwise the clock reading stands.
fn recover_ui(crossings: &Crossings, notes: &mut Vec<String>) -> Result<(f64, UiSource), String> {
    let mut gaps: Vec<f64> = crossings
        .rising
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| g.is_finite() && *g > 0.0)
        .collect();
    if gaps.is_empty() {
        return Err("could not measure an interval between crossings".to_string());
    }
    gaps.sort_by(|a, b| a.partial_cmp(b).expect("finite and positive"));
    let median = gaps[gaps.len() / 2];

    if let Some((unit, lengths)) = run_unit(&crossings.all) {
        // A clock's runs are all one unit long and its period is two of them;
        // only a line with several run lengths is data.
        if lengths.len() >= MIN_RUN_LENGTHS {
            notes.push(format!(
                "unit interval recovered from the shortest run between crossings ({} runs in {} \
                 lengths, 1..{} units): intervals between like-going crossings on this line are \
                 multiples of it, so their median ({:.2} units) is not the unit interval",
                crossings.all.len().saturating_sub(1),
                lengths.len(),
                lengths.iter().copied().max().unwrap_or(1),
                median / unit,
            ));
            return Ok((unit, UiSource::RecoveredFromRuns));
        }
    }
    notes.push(format!(
        "unit interval recovered from {} rising crossings (median of {} intervals)",
        crossings.rising.len(),
        gaps.len()
    ));
    Ok((median, UiSource::Recovered))
}

/// One unit from the runs between consecutive crossings, with the whole-number
/// multiples that were found. `None` when the runs are not multiples of
/// anything in particular - a sine, a PWM whose two widths share no common
/// bit - which is the case that must fall back to the clock reading.
fn run_unit(all: &[f64]) -> Option<(f64, Vec<u32>)> {
    let mut runs: Vec<f64> = all
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|r| r.is_finite() && *r >= MIN_RUN_SAMPLES)
        .collect();
    if runs.len() < 8 {
        return None;
    }
    runs.sort_by(|a, b| a.partial_cmp(b).expect("finite and positive"));
    // A low percentile rather than the minimum, so one narrow glitch cannot
    // halve the unit; then the mean of the runs near it, because at 16 samples
    // per bit the one-bit runs measure 15..17 and the percentile alone would
    // make a five-bit run read as 5.33 units.
    let low = runs[runs.len() / 20];
    let near: Vec<f64> = runs.iter().copied().filter(|&r| r / low < 1.25).collect();
    let unit = near.iter().sum::<f64>() / near.len() as f64;
    // A "unit" below what a fold needs cannot be a bit time: the view would be
    // refused for it anyway, and every run is a whole number of a short enough
    // unit (B-72).
    if unit < MIN_SAMPLES_PER_UI {
        return None;
    }
    let median_run = runs[runs.len() / 2];
    if median_run / unit > MAX_MEDIAN_RUN_UNITS {
        return None;
    }
    let mut fitting = 0usize;
    let mut counts: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    for &r in &runs {
        let units = r / unit;
        let n = units.round().max(1.0);
        if (units - n).abs() <= RUN_TOLERANCE {
            fitting += 1;
            *counts.entry(n as u32).or_default() += 1;
        }
    }
    if (fitting as f64) < MIN_RUN_FIT * runs.len() as f64 {
        return None;
    }
    // A length seen once is noise, not a run length the line uses.
    let lengths: Vec<u32> = counts
        .into_iter()
        .filter(|&(_, count)| count > 1)
        .map(|(n, _)| n)
        .collect();
    Some((unit, lengths))
}

/// Which unit interval each crossing sits on, for a line where consecutive
/// crossings are a whole number of intervals apart.
///
/// Rounded against the given interval, then once more against the interval the
/// least-squares fit of those indices reports: a unit interval off by a
/// fraction of a percent would otherwise slip the index by one part-way
/// through a long capture, and a slipped index is a whole interval of invented
/// jitter.
fn unit_indices(rising: &[f64], ui_samples: f64) -> Vec<f64> {
    let Some(&first) = rising.first() else {
        return Vec::new();
    };
    let round_against = |ui: f64| -> Vec<f64> {
        rising
            .iter()
            .map(|t| ((t - first) / ui).round().max(0.0))
            .collect()
    };
    if ui_samples <= 0.0 {
        return (0..rising.len()).map(|k| k as f64).collect();
    }
    let first_pass = round_against(ui_samples);
    match fit_slope(rising, &first_pass) {
        Some((slope, _)) if slope > 0.0 => round_against(slope),
        _ => first_pass,
    }
}

/// Least-squares `(slope, intercept)` of time against grid index.
fn fit_slope(times: &[f64], indices: &[f64]) -> Option<(f64, f64)> {
    let n = times.len();
    if n < 3 || indices.len() != n {
        return None;
    }
    let mean_k = indices.iter().sum::<f64>() / n as f64;
    let mean_t = times.iter().sum::<f64>() / n as f64;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for (&k, &t) in indices.iter().zip(times) {
        let dk = k - mean_k;
        sxy += dk * (t - mean_t);
        sxx += dk * dk;
    }
    if sxx <= 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    Some((slope, mean_t - slope * mean_k))
}

struct Jitter {
    tie_pp_s: Option<f64>,
    cycle_to_cycle_pp_s: Option<f64>,
    fitted_period_s: Option<f64>,
    /// How many bursts the crossings fell into; `None` when none could be
    /// fitted. More than one means TIE is the worst burst's, not one grid
    /// across the whole capture (B-70).
    tie_bursts: Option<usize>,
}

/// Both jitter numbers, from the crossing times.
///
/// TIE is measured against a **least-squares fit** of crossing time against
/// crossing index — the standard construction, and the only one that does not
/// turn a slightly wrong period into jitter that grows with capture length. An
/// earlier version fitted the grid with the median period and reported a
/// perfectly steady 40-cycle signal as having a whole period of jitter.
///
/// The fit assumes the crossings are consecutive. A signal that drops an edge
/// part-way through therefore inflates TIE, which is the honest outcome: the
/// crossings really are not where a periodic grid says they should be.
fn jitter(rising: &[f64], indices: &[f64], rate_hz: f64, ui_samples: f64, nrz: bool) -> Jitter {
    let none = Jitter {
        tie_pp_s: None,
        cycle_to_cycle_pp_s: None,
        fitted_period_s: None,
        tie_bursts: None,
    };
    let n = rising.len();
    if n < 3 || rate_hz <= 0.0 || indices.len() != n {
        return none;
    }
    // The period is fitted over everything: idle gaps make the baseline longer,
    // which is exactly what a rate estimate wants.
    let Some((period, _)) = fit_slope(rising, indices) else {
        return none;
    };
    if period <= 0.0 {
        return none;
    }

    // TIE is fitted per burst. A capture of CAN frames 10 ms apart holds a
    // handful of bursts, each starting on its own transmitter phase; one grid
    // across all of them measures the phase differences between bursts and
    // calls them jitter, which read 1.3-2.3 us of TIE on a 2 us bit whose
    // cycle-to-cycle jitter was 500 ns (B-70). The worst burst is reported,
    // because the question "how clean is this signal" is answered by its worst
    // stretch, not by an average over its idle time.
    let gap_limit = (BURST_GAP_UIS * ui_samples).max(f64::MIN_POSITIVE);
    let mut worst_tie = f64::NEG_INFINITY;
    let mut worst_c2c = f64::NEG_INFINITY;
    let mut bursts = 0usize;
    let mut fitted = 0usize;
    let mut from = 0usize;
    for end in burst_ends(rising, gap_limit) {
        bursts += 1;
        let times = &rising[from..end];
        // Indexed from this burst's own first crossing, not the capture's.
        // Rounding `(t - first) / ui` over a whole capture carries whatever
        // error `ui` has all the way out: 2000 intervals at 0.1 % is two
        // intervals of drift, and a burst that straddles the half-interval
        // where that rounding flips has some of its crossings indexed one
        // interval out. That tilts its fit and leaves half an interval of
        // residual - which is what two of sixteen frames of a real CAN capture
        // did, 0.93 UI against 0.06 UI for the other fourteen.
        let keys: Vec<f64> = if nrz {
            burst_indices(times, ui_samples)
        } else {
            (0..times.len()).map(|k| k as f64).collect()
        };
        from = end;
        if let Some(tie) = burst_tie(times, &keys) {
            fitted += 1;
            worst_tie = worst_tie.max(tie);
        }
        if let Some(c2c) = burst_cycle_to_cycle(times, &keys) {
            worst_c2c = worst_c2c.max(c2c);
        }
    }
    let _ = indices;

    Jitter {
        tie_pp_s: worst_tie.is_finite().then(|| worst_tie / rate_hz),
        cycle_to_cycle_pp_s: worst_c2c.is_finite().then(|| worst_c2c / rate_hz),
        fitted_period_s: Some(period / rate_hz),
        tie_bursts: (fitted > 0).then_some(bursts),
    }
}

/// Grid indices of one burst's crossings, counted from its own first crossing
/// and refined once against the interval its own crossings say - so a unit
/// interval that is slightly off cannot put part of a burst an interval out.
fn burst_indices(times: &[f64], ui_samples: f64) -> Vec<f64> {
    let Some(&first) = times.first() else {
        return Vec::new();
    };
    if ui_samples <= 0.0 {
        return (0..times.len()).map(|k| k as f64).collect();
    }
    let round_against = |ui: f64| -> Vec<f64> {
        times
            .iter()
            .map(|t| ((t - first) / ui).round().max(0.0))
            .collect()
    };
    let first_pass = round_against(ui_samples);
    match fit_slope(times, &first_pass) {
        Some((slope, _)) if slope > 0.0 => round_against(slope),
        _ => first_pass,
    }
}

/// Exclusive end index of each burst: a gap of more than [`BURST_GAP_UIS`]
/// unit intervals between crossings is the line going idle, not a long run of
/// equal bits (CAN stuffing bounds a run at six).
fn burst_ends(rising: &[f64], gap_limit: f64) -> Vec<usize> {
    let mut ends = Vec::new();
    for i in 1..rising.len() {
        if rising[i] - rising[i - 1] > gap_limit {
            ends.push(i);
        }
    }
    ends.push(rising.len());
    ends
}

/// Peak-to-peak time interval error of one burst against its own fitted grid,
/// in samples. `None` for a burst too short to fit a line through.
fn burst_tie(times: &[f64], keys: &[f64]) -> Option<f64> {
    if times.len() < 3 {
        return None;
    }
    let (period, intercept) = fit_slope(times, keys)?;
    if period <= 0.0 {
        return None;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for (&k, &t) in keys.iter().zip(times) {
        let error = t - (intercept + period * k);
        min = min.min(error);
        max = max.max(error);
    }
    (min.is_finite() && max.is_finite()).then_some(max - min)
}

/// Largest change from one unit interval to the next inside a burst, in
/// samples. Per unit interval, not per crossing: on data a gap of three
/// intervals is not three intervals of jitter, and with one interval per
/// crossing - a clock - this is the gap itself.
fn burst_cycle_to_cycle(times: &[f64], keys: &[f64]) -> Option<f64> {
    let per_unit: Vec<f64> = times
        .windows(2)
        .zip(keys.windows(2))
        .filter_map(|(t, k)| {
            let steps = k[1] - k[0];
            (steps > 0.0).then(|| (t[1] - t[0]) / steps)
        })
        .collect();
    if per_unit.len() < 2 {
        return None;
    }
    Some(
        per_unit
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f64, f64::max),
    )
}

fn span(samples: &[f64]) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &v in samples {
        if v.is_finite() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if lo.is_finite() && hi.is_finite() {
        (lo, hi)
    } else {
        (0.0, 0.0)
    }
}

fn mean((sum, count): (f64, usize)) -> Option<f64> {
    (count > 0).then(|| sum / count as f64)
}

/// Linear interpolation at a fractional sample index, clamped to the array.
fn interpolate(samples: &[f64], at: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let clamped = at.clamp(0.0, (samples.len() - 1) as f64);
    let i = clamped.floor() as usize;
    let frac = clamped - i as f64;
    let a = samples[i];
    let b = *samples.get(i + 1).unwrap_or(&a);
    a + (b - a) * frac
}

/// Row 0 is the top of the range, as in the drawn image.
fn value_row(v: f64, y_lo: f64, y_hi: f64, rows: usize) -> usize {
    let t = ((v - y_lo) / (y_hi - y_lo)).clamp(0.0, 1.0);
    let row = ((1.0 - t) * (rows - 1) as f64).round() as usize;
    row.min(rows - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean square wave: `period` samples per cycle, `cycles` of them.
    fn square(period: usize, cycles: usize, low: f64, high: f64) -> Vec<f64> {
        (0..period * cycles)
            .map(|i| if i % period < period / 2 { low } else { high })
            .collect()
    }

    /// An NRZ bit stream: `bits` at `samples_per_bit`, levels `low`/`high`,
    /// one sample of ramp at each transition so the crossing interpolates.
    fn nrz(bits: &[bool], samples_per_bit: usize, low: f64, high: f64) -> Vec<f64> {
        let mut out = Vec::with_capacity(bits.len() * samples_per_bit);
        for &bit in bits {
            let level = if bit { high } else { low };
            out.extend(std::iter::repeat_n(level, samples_per_bit));
        }
        out
    }

    /// Runs of 1, 2, 3, 4 and 5 equal bits - what bit stuffing leaves on a CAN
    /// bus - repeated enough times to fold.
    fn stuffed_bits(repeats: usize) -> Vec<bool> {
        let pattern = [
            true, false, true, true, false, false, false, true, true, true, true, false, false,
            false, false, false, true, true, true, true, true, false,
        ];
        pattern
            .iter()
            .copied()
            .cycle()
            .take(pattern.len() * repeats)
            .collect()
    }

    /// B-57: on a 500 kbit/s CAN bus (2 us bits, 16 samples per bit at
    /// 8 MS/s) the eye recovered a 6.00 us unit interval - three bits, the
    /// commonest gap between rising crossings - and reported jitter_tie_pp of
    /// 16.2 ms against that grid.
    #[test]
    fn an_nrz_stream_folds_on_one_bit_not_on_a_run_of_them() {
        let rate_hz = 8_000_000.0;
        let samples = nrz(&stuffed_bits(40), 16, 1.4, 4.1);
        let eye = analyze(&samples, rate_hz, 256, 128, None).expect("should fold");
        assert_eq!(eye.ui_source, UiSource::RecoveredFromRuns);
        let ui_us = eye.ui_s * 1e6;
        assert!(
            (ui_us - 2.0).abs() < 0.05,
            "unit interval {ui_us:.3} us, want 2 us"
        );
        // The number that gave the bug away: against a grid of whole unit
        // intervals the crossings of a clean stream are where they belong.
        let tie = eye.tie_pp_s.expect("measurable");
        assert!(
            tie < eye.ui_s,
            "tie {tie:e} s is not under one {:e} s interval",
            eye.ui_s
        );
        // And the note says why the median interval was not used.
        assert!(
            eye.notes.iter().any(|n| n.contains("shortest run")),
            "{:?}",
            eye.notes
        );

        // What the old recovery would have returned, so this test fails again
        // if the median of like-going crossings ever comes back: on this stream
        // it is a whole multiple of the bit time, not the bit time.
        let threshold = (1.4 + 4.1) / 2.0;
        let hysteresis = (4.1 - 1.4) * HYSTERESIS_FRACTION;
        let mut gaps: Vec<f64> = crossings(&samples, threshold, hysteresis)
            .rising
            .windows(2)
            .map(|w| w[1] - w[0])
            .collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        let median_units = gaps[gaps.len() / 2] / (eye.ui_s * rate_hz);
        assert!(
            median_units >= 2.0,
            "the median rising interval is {median_units:.2} units: this stream no longer \
             reproduces B-57"
        );
    }

    /// B-70: after B-57 the unit interval was right (1.99 µs) but
    /// `jitter_tie_pp` read 1.3–2.3 µs on a 2 µs bit, which is not plausible
    /// for a clean bus whose cycle-to-cycle jitter is 500 ns. The capture holds
    /// frames 10 ms apart, each starting on its own phase, and one TIE grid
    /// across all of them measures the phase differences between frames.
    #[test]
    fn tie_is_fitted_per_burst_not_across_the_idle_gaps_between_them() {
        let rate_hz = 8_000_000.0;
        let samples_per_bit = 16;
        let bits = stuffed_bits(8);
        // Three bursts of the same clean stream, separated by idle gaps far
        // longer than a bit, each offset by a third of a bit so the bursts do
        // not share a phase - exactly what separate frames look like.
        let mut samples = Vec::new();
        for burst in 0..3 {
            samples.extend(std::iter::repeat_n(1.4, 40 * samples_per_bit));
            samples.extend(std::iter::repeat_n(1.4, burst * samples_per_bit / 3));
            samples.extend(nrz(&bits, samples_per_bit, 1.4, 4.1));
        }
        samples.extend(std::iter::repeat_n(1.4, 40 * samples_per_bit));

        let eye = analyze(&samples, rate_hz, 256, 128, None).expect("should fold");
        let ui = eye.ui_s;
        assert!(
            (ui * 1e6 - 2.0).abs() < 0.05,
            "unit interval {} us",
            ui * 1e6
        );
        assert_eq!(eye.tie_bursts, Some(3), "three bursts, fitted separately");
        let tie = eye.tie_pp_s.expect("measurable");
        assert!(
            tie < ui / 2.0,
            "tie {:.3} us on a {:.3} us interval: the grid is still spanning the gaps",
            tie * 1e6,
            ui * 1e6
        );
        assert!(
            eye.notes.iter().any(|n| n.contains("3 bursts")),
            "the reading has to say it is the worst burst's: {:?}",
            eye.notes
        );
    }

    /// One burst is one grid, and nothing about the reading changes for it.
    #[test]
    fn a_single_burst_is_fitted_as_one_grid() {
        let samples = nrz(&stuffed_bits(40), 16, 1.4, 4.1);
        let eye = analyze(&samples, 8_000_000.0, 256, 128, None).expect("folds");
        assert_eq!(eye.tie_bursts, Some(1));
        assert!(
            !eye.notes.iter().any(|n| n.contains("bursts")),
            "{:?}",
            eye.notes
        );
    }

    /// B-72: the eye takes its crossings against a plain mid level with no
    /// hysteresis - unlike `measure`, which has one - so a real clock whose
    /// edges dither across the threshold leaves runs of one and two samples
    /// between crossings. Every run is then a whole number of a one-sample
    /// "unit" in several different lengths, which is exactly what the NRZ test
    /// looks for, so the run-based recovery took 1.0 samples as the bit time
    /// and the fold was refused outright: "the period is only 1.0 samples
    /// long". Seen on a 101 kHz square on the bench at 8 MS/s.
    #[test]
    fn threshold_dither_does_not_become_a_one_sample_bit_time() {
        let rate_hz = 8_000_000.0;
        let period = 80usize; // 100 kHz at 8 MS/s
        let (low, high) = (0.3, 3.4);
        let threshold = (low + high) / 2.0;
        // Up, down, up, down, down, up across the threshold at each edge: four
        // distinct short run lengths, which is what fooled the NRZ test.
        let dither = [true, false, true, false, false, true];
        let mut samples = Vec::new();
        for _ in 0..40 {
            for (j, level) in std::iter::repeat_n(high, period / 2).enumerate() {
                samples.push(match dither.get(j) {
                    Some(true) => threshold + 0.01,
                    Some(false) => threshold - 0.01,
                    None => level,
                });
            }
            for (j, level) in std::iter::repeat_n(low, period / 2).enumerate() {
                samples.push(match dither.get(j) {
                    Some(true) => threshold + 0.01,
                    Some(false) => threshold - 0.01,
                    None => level,
                });
            }
        }

        let eye = analyze(&samples, rate_hz, 256, 128, None)
            .expect("a clock with dithering edges still folds on its period");
        assert_eq!(
            eye.ui_source,
            UiSource::Recovered,
            "a clock is not NRZ data"
        );
        let want = period as f64 / rate_hz;
        assert!(
            (eye.ui_s - want).abs() < want * 0.05,
            "unit interval {:.3} us, want {:.3} us",
            eye.ui_s * 1e6,
            want * 1e6
        );
    }

    /// The clock cases keep the reading that is right for them: one run length
    /// is a clock, two is a duty-cycled one, and for both the interval between
    /// like-going crossings is the unit interval.
    #[test]
    fn a_clock_still_folds_on_its_period() {
        let rate_hz = 1_000_000.0;
        let eye = analyze(&square(100, 40, 0.0, 3.3), rate_hz, 256, 128, None).expect("folds");
        assert_eq!(eye.ui_source, UiSource::Recovered);
        assert!((eye.ui_s - 100e-6).abs() < 1e-7, "{} s", eye.ui_s);

        // 20 % duty: runs of 20 and 80 samples, two lengths, still a clock.
        let duty: Vec<f64> = (0..100 * 40)
            .map(|i| if i % 100 < 20 { 3.3 } else { 0.0 })
            .collect();
        let eye = analyze(&duty, rate_hz, 256, 128, None).expect("folds");
        assert_eq!(eye.ui_source, UiSource::Recovered);
        assert!((eye.ui_s - 100e-6).abs() < 1e-7, "{} s", eye.ui_s);
    }

    /// A unit interval the caller gives is still used as given, whatever the
    /// runs say - it is the documented way round a recovery that is wrong.
    #[test]
    fn a_given_unit_interval_overrides_the_runs() {
        let samples = nrz(&stuffed_bits(40), 16, 1.4, 4.1);
        let eye = analyze(&samples, 8_000_000.0, 256, 128, Some(6e-6)).expect("folds");
        assert_eq!(eye.ui_source, UiSource::Given);
        assert!((eye.ui_s - 6e-6).abs() < 1e-12, "{} s", eye.ui_s);
    }

    /// B-74: a 101 kHz sine at 8 MS/s, with the sample of jitter a real
    /// capture has, folded as data with an eye height of 13.6 mV.
    #[test]
    fn a_sine_is_periodic_not_data() {
        let rate_hz = 8e6;
        let f = 101e3;
        let s: Vec<f64> = (0..80_000)
            .map(|i| {
                // A deterministic wobble of a fraction of a sample in time.
                let t = (i as f64 + 0.4 * ((i * 7919) % 13) as f64 / 13.0) / rate_hz;
                1.6 * (2.0 * std::f64::consts::PI * f * t).sin() + 1.9
            })
            .collect();
        let eye = analyze(&s, rate_hz, 256, 128, None).expect("should analyze");
        assert_eq!(eye.kind, EyeKind::Periodic);
        assert!(
            (1.0 / eye.ui_s - f).abs() / f < 0.01,
            "{} Hz",
            1.0 / eye.ui_s
        );
        assert_eq!(eye.eye_height, None);
        assert_eq!(eye.eye_width_s, None);
        assert_eq!(eye.settled_high_s, None);
        assert!(
            eye.notes.iter().any(|n| n.contains("periodic analog")),
            "{:?}",
            eye.notes
        );
        // A square of the same span still opens, noise and all.
        let sq: Vec<f64> = square(80, 400, 0.3, 3.5)
            .iter()
            .enumerate()
            .map(|(i, v)| v + 0.05 * (((i * 7919) % 17) as f64 / 17.0 - 0.5))
            .collect();
        let eye = analyze(&sq, rate_hz, 256, 128, None).expect("should analyze");
        assert_ne!(eye.kind, EyeKind::Periodic);
        assert!(eye.eye_height.is_some());
        // A square that overshoots 28 % and settles over a fifth of each
        // half period (round 8's CH1) still has two levels, ...
        let overshoot: Vec<f64> = (0..400 * 80)
            .map(|i| {
                let (half, k) = ((i / 40) % 2, (i % 40) as f64);
                let ring = 0.56 * (-k / 3.0).exp();
                if half == 1 {
                    2.0 + ring
                } else {
                    -ring
                }
            })
            .collect();
        let eye = analyze(&overshoot, rate_hz, 256, 128, None).expect("should analyze");
        assert_ne!(eye.kind, EyeKind::Periodic, "{:?}", eye.notes);
        // ... and so does a 2 %-duty pulse train.
        let pulses: Vec<f64> = (0..400 * 100)
            .map(|i| if i % 100 < 2 { 3.3 } else { 0.0 })
            .collect();
        let eye = analyze(&pulses, rate_hz, 256, 128, None).expect("should analyze");
        assert_ne!(eye.kind, EyeKind::Periodic);
        // A triangle has none.
        let tri: Vec<f64> = (0..400 * 80)
            .map(|i| ((i % 80) as f64 - 40.0).abs() / 40.0 * 3.0)
            .collect();
        let eye = analyze(&tri, rate_hz, 256, 128, None).expect("should analyze");
        assert_eq!(eye.kind, EyeKind::Periodic);
    }

    #[test]
    fn a_clean_square_wave_gives_a_fully_open_eye() {
        let s = square(100, 40, 0.0, 3.3);
        let eye = analyze(&s, 1e6, 200, 100, None).expect("should analyze");
        assert_eq!(eye.ui_source, UiSource::Recovered);
        // 100 samples at 1 MHz.
        assert!((eye.ui_s - 100e-6).abs() < 1e-9, "{}", eye.ui_s);
        assert!(eye.traces >= 35, "{} traces", eye.traces);
        assert!((eye.level_low - 0.0).abs() < 1e-9, "{}", eye.level_low);
        assert!((eye.level_high - 3.3).abs() < 1e-9, "{}", eye.level_high);
        // A perfect square wave has the whole span open and no jitter.
        let height = eye.eye_height.expect("open");
        assert!((height - 3.3).abs() < 0.2, "eye height {height}");
        assert_eq!(eye.tie_pp_s, Some(0.0));
        let width = eye.eye_width_s.expect("open");
        // Half of the two-interval window is one whole period of settled
        // signal on each side of an edge, so the open run is a good fraction
        // of a period rather than a sliver.
        assert!(
            width > eye.ui_s * 0.2,
            "eye width {width} vs UI {}",
            eye.ui_s
        );
    }

    /// B-46: a logic line folds to a data eye that is open in every column —
    /// its levels are exactly 0 and 1, so no sample ever sits between them —
    /// and the open run used to span the whole two-interval window: SCL at
    /// 8 MS/s read `eye_width` 5.50 us on a 2.75 us unit interval.
    #[test]
    fn a_logic_lines_eye_is_never_wider_than_one_unit_interval() {
        // 22-sample periods whose high time wanders 8..14 samples, as a
        // clock-stretched SCL does: the folded phases carry both levels.
        let s: Vec<f64> = (0..400)
            .flat_map(|c| {
                let high = 8 + (c * 3) % 7;
                (0..22).map(move |i| if i < high { 1.0 } else { 0.0 })
            })
            .collect();
        let eye = analyze(&s, 8e6, 256, 128, None).expect("should analyze");
        assert_eq!(eye.kind, EyeKind::Data);
        let width = eye.eye_width_s.expect("open");
        assert!(
            width <= eye.ui_s * (1.0 + 1e-9),
            "eye width {width} exceeds the unit interval {}",
            eye.ui_s
        );
    }

    #[test]
    fn a_given_unit_interval_is_used_instead_of_being_recovered() {
        let s = square(100, 40, 0.0, 1.0);
        let eye = analyze(&s, 1e6, 64, 64, Some(200e-6)).expect("should analyze");
        assert_eq!(eye.ui_source, UiSource::Given);
        assert!((eye.ui_s - 200e-6).abs() < 1e-12);
        assert!(eye.notes.iter().all(|n| !n.contains("recovered")));
    }

    /// Jitter is the whole point of the view, so it has to show up as a
    /// number and not merely as a blurrier picture.
    ///
    /// The signal has to be built with care: alternating the *period* while
    /// splitting each cycle 50/50 leaves the rising crossings exactly 100
    /// samples apart — the wander cancels — and measures nothing. Holding the
    /// high time fixed and varying the low time is what actually moves them.
    #[test]
    fn jitter_in_the_signal_appears_as_crossing_jitter() {
        let mut s = Vec::new();
        for cycle in 0..40 {
            let period = if cycle % 2 == 0 { 98 } else { 102 };
            let high = 50;
            s.extend(std::iter::repeat_n(0.0, period - high));
            s.extend(std::iter::repeat_n(1.0, high));
        }
        let eye = analyze(&s, 1e6, 200, 100, None).expect("should analyze");
        // The crossings alternate 98 and 102 samples apart. Against the
        // best-fit 100-sample grid they sit alternately on it and 2 samples
        // late, so TIE peak-to-peak is 2 us; the change from one period to the
        // next is 4 samples, so cycle-to-cycle is 4 us. Quoting either as "the
        // jitter" without the other is how these two get confused.
        let tie = eye.tie_pp_s.expect("measurable");
        assert!((1.5e-6..2.5e-6).contains(&tie), "TIE {tie}");
        let c2c = eye.cycle_to_cycle_pp_s.expect("measurable");
        assert!((3.5e-6..4.5e-6).contains(&c2c), "cycle-to-cycle {c2c}");
        let fitted = eye.fitted_period_s.expect("fitted");
        assert!((fitted - 100e-6).abs() < 1e-7, "fitted period {fitted}");
    }

    /// The regression the fit above exists for: a steady signal measured over
    /// many cycles must not accumulate drift into its jitter number.
    #[test]
    fn a_steady_signal_reports_no_jitter_however_long_the_capture() {
        for cycles in [10usize, 100, 400] {
            let s = square(100, cycles, 0.0, 1.0);
            let eye = analyze(&s, 1e6, 64, 64, None).expect("should analyze");
            let tie = eye.tie_pp_s.expect("measurable");
            assert!(tie < 1e-9, "{cycles} cycles reported TIE {tie}");
            assert_eq!(eye.cycle_to_cycle_pp_s, Some(0.0));
        }
    }

    #[test]
    fn a_constant_signal_is_refused_with_a_reason_not_an_empty_picture() {
        let err = analyze(&[1.0; 1000], 1e6, 64, 64, None).unwrap_err();
        assert!(err.contains("constant"), "{err}");
    }

    #[test]
    fn too_few_periods_is_refused_with_a_reason() {
        let s = square(100, 2, 0.0, 1.0);
        let err = analyze(&s, 1e6, 64, 64, None).unwrap_err();
        assert!(err.contains("period"), "{err}");
    }

    #[test]
    fn a_period_of_a_few_samples_is_refused_rather_than_aliased() {
        let s = square(6, 200, 0.0, 1.0);
        let err = analyze(&s, 1e6, 64, 64, None).unwrap_err();
        assert!(err.contains("higher rate"), "{err}");
    }

    /// The opening is found, not assumed: a 20 %-duty clock opens nowhere
    /// near the middle of the window, and a version of this that measured a
    /// fixed centre column reported no eye at all for a plain square wave.
    #[test]
    fn the_opening_is_found_whatever_the_duty_cycle() {
        for high in [20usize, 50, 80] {
            let mut s = Vec::new();
            for _ in 0..40 {
                s.extend(std::iter::repeat_n(0.0, 100 - high));
                s.extend(std::iter::repeat_n(3.3, high));
            }
            let eye =
                analyze(&s, 1e6, 200, 100, None).unwrap_or_else(|e| panic!("duty {high}%: {e}"));
            let height = eye
                .eye_height
                .unwrap_or_else(|| panic!("duty {high}%: no opening found"));
            assert!(
                (height - 3.3).abs() < 0.2,
                "duty {high}%: eye height {height}"
            );
            let phase = eye.eye_phase.expect("a located opening");
            assert!((0.0..=1.0).contains(&phase), "duty {high}%: phase {phase}");
        }
    }

    #[test]
    fn nothing_in_the_density_grid_falls_outside_it() {
        let s = square(50, 30, -2.0, 2.0);
        let eye = analyze(&s, 1e6, 37, 29, None).expect("should analyze");
        assert_eq!(eye.density.len(), 37 * 29);
        assert!(eye.max_count > 0);
        let total: u64 = eye.density.iter().map(|&c| u64::from(c)).sum();
        assert_eq!(
            total,
            (eye.traces * eye.cols) as u64,
            "every folded point must land in exactly one cell"
        );
    }

    #[test]
    fn interpolation_is_clamped_at_both_ends() {
        let s = [0.0, 10.0];
        assert_eq!(interpolate(&s, -5.0), 0.0);
        assert_eq!(interpolate(&s, 0.5), 5.0);
        assert_eq!(interpolate(&s, 99.0), 10.0);
        assert_eq!(interpolate(&[], 1.0), 0.0);
    }
}
