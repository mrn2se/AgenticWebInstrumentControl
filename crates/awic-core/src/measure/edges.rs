//! Shared analog waveform primitives: settled-level estimation and
//! hysteresis edge detection (see `super`'s module docs for the definitions).

/// Fraction of `(top - base)` used as the hysteresis dead band on each side
/// of the mid-level. A transition only registers once the signal clears the
/// *far* threshold from wherever it last settled, so noise that wobbles
/// within this band around the midpoint cannot register as a phantom edge.
const HYSTERESIS_FRACTION: f64 = 0.1;

/// Fractions of `(top - base)` used for the 10%-90% rise/fall definition.
const LOW_FRACTION: f64 = 0.1;
const HIGH_FRACTION: f64 = 0.9;

/// A directed, sub-sample-interpolated threshold crossing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Edge {
    /// Fractional sample index of the crossing (linear-interpolated).
    pub mid_index: f64,
    pub rising: bool,
    /// The tight bracket the mid-level crossing was found within: the last
    /// sample confirmed in the previous settled region, and the first
    /// confirmed in the new one. Used as a seed to search for the 10%/90%
    /// crossings of the same physical transition.
    pub from: usize,
    pub to: usize,
}

pub(super) fn min_max(values: &[f64]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &v in values {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    (min, max)
}

/// Estimates the waveform's settled `(top, base)` levels by histogram mode
/// rather than raw extrema (SPECIFICATION.md §5.4): a waveform spends most
/// of its time near its two plateaus (or, for a sine, near its two peaks),
/// so the most populous bin in the upper/lower half of the range is a robust
/// stand-in for "the high/low level" that a single spike sample cannot skew
/// the way `min`/`max` can.
///
/// Returns `(top, base)` with `top >= base`. If the signal is exactly flat
/// (`max == min`), both equal that one level.
pub(super) fn top_base(values: &[f64]) -> (f64, f64) {
    let (min, max) = min_max(values);
    if max <= min {
        return (max, min);
    }
    const BINS: usize = 256;
    let mut hist = [0u32; BINS];
    let width = (max - min) / BINS as f64;
    for &v in values {
        let bin = (((v - min) / width) as usize).min(BINS - 1);
        hist[bin] += 1;
    }
    let mid = BINS / 2;
    let base_bin = (0..mid).max_by_key(|&i| hist[i]).unwrap_or(0);
    let top_bin = (mid..BINS).max_by_key(|&i| hist[i]).unwrap_or(BINS - 1);
    let base = min + (base_bin as f64 + 0.5) * width;
    let top = min + (top_bin as f64 + 0.5) * width;
    (top, base)
}

/// Linearly interpolates the fractional index in `[from, to]` where `values`
/// crosses `level`, scanning forward for the first bracketing pair. `from`
/// and `to` are expected to straddle the level (that's the caller's
/// contract); if no bracketing pair is found the midpoint is returned as a
/// safe fallback rather than panicking.
fn interpolate_crossing(values: &[f64], from: usize, to: usize, level: f64) -> f64 {
    for i in from..to {
        let a = values[i] - level;
        let b = values[i + 1] - level;
        if a == 0.0 {
            return i as f64;
        }
        if (a < 0.0) != (b < 0.0) {
            return i as f64 + a / (a - b);
        }
    }
    (from as f64 + to as f64) / 2.0
}

/// Finds the first (if `rising`) or last (if `!rising`) crossing of `level`
/// within `values[range]`, in the given direction. Used to locate the
/// 10%/90% crossings within a transition's search window, independent of
/// the coarser hysteresis bracket used to detect the transition itself.
fn directed_crossing(
    values: &[f64],
    range: std::ops::Range<usize>,
    level: f64,
    rising: bool,
    take_last: bool,
) -> Option<f64> {
    let mut found = None;
    if range.end <= range.start || range.end > values.len() {
        return None;
    }
    for i in range.start..range.end - 1 {
        let a = values[i];
        let b = values[i + 1];
        let crossed = if rising {
            a < level && b >= level
        } else {
            a > level && b <= level
        };
        if crossed {
            let frac = (level - a) / (b - a);
            found = Some(i as f64 + frac);
            if !take_last {
                return found;
            }
        }
    }
    found
}

/// Detects hysteresis-qualified edges in `values` given the waveform's
/// settled `(top, base)` levels. Returns edges in chronological order.
///
/// A sample is "confirmed" high once it reaches `mid + hysteresis` and
/// confirmed low once it reaches `mid - hysteresis`; anything between is
/// ambiguous and ignored. An edge is recorded only when a confirmation
/// follows a confirmation of the *other* state — never on repeated
/// confirmations of the same state — so noise that dips into the dead band
/// and back out on the same side registers nothing.
pub(super) fn find_edges(values: &[f64], top: f64, base: f64) -> Vec<Edge> {
    let span = top - base;
    if span <= 0.0 || values.len() < 2 {
        return Vec::new();
    }
    let mid = (top + base) / 2.0;
    let hi = mid + HYSTERESIS_FRACTION * span;
    let lo = mid - HYSTERESIS_FRACTION * span;

    #[derive(Clone, Copy, PartialEq)]
    enum Region {
        High,
        Low,
    }
    fn region(v: f64, lo: f64, hi: f64) -> Option<Region> {
        if v >= hi {
            Some(Region::High)
        } else if v <= lo {
            Some(Region::Low)
        } else {
            None
        }
    }

    let mut edges = Vec::new();
    let mut confirmed: Option<Region> = None;
    let mut confirmed_index = 0usize;

    for (i, &v) in values.iter().enumerate() {
        let Some(r) = region(v, lo, hi) else {
            continue;
        };
        match confirmed {
            None => {
                confirmed = Some(r);
                confirmed_index = i;
            }
            Some(prev) if prev != r => {
                let mid_index = interpolate_crossing(values, confirmed_index, i, mid);
                edges.push(Edge {
                    mid_index,
                    rising: r == Region::High,
                    from: confirmed_index,
                    to: i,
                });
                confirmed = Some(r);
                confirmed_index = i;
            }
            _ => confirmed_index = i,
        }
    }
    edges
}

/// Mean of complete like-edge-to-like-edge periods, in samples — never
/// first-to-last-edge divided by a guessed count, which would silently
/// assume every gap is exactly one period. `None` if fewer than two
/// like-polarity edges are present (less than one complete period).
/// Longest run, in unit intervals, an NRZ line is judged on; longer runs are
/// idle gaps (the same rule as the anomaly view's, B-102).
const NRZ_MAX_RUN: f64 = 16.0;
/// How close (in unit intervals) a run must be to a whole number of units.
const NRZ_FIT: f64 = 0.25;
/// Least share of runs at one unit, and at two units or more, and least
/// number of distinct run lengths, for a line to read as NRZ data rather than
/// a clock with a few long periods.
const NRZ_MIN_SHARE: f64 = 0.1;
const NRZ_MIN_LENGTHS: usize = 3;
/// Runs this many samples long or shorter are glitches (an I²C SDA's ACK
/// hand-over spikes), never a unit interval: everything is a whole number
/// of one sample.
const NRZ_GLITCH_SAMPLES: f64 = 2.0;

/// The unit interval in samples when the runs between `edges` are whole
/// numbers of one short run — NRZ data (UART, CAN, any line carrying bits),
/// where a frequency or a duty describes the data, not a clock (B-103).
/// The unit is the median of the runs within 20 % of the shortest; glitches
/// and runs longer than [`NRZ_MAX_RUN`] units (idle gaps) are left out, and
/// runs that fit no whole number (a broken bit) may make up under 10 %.
pub(super) fn nrz_unit_samples(edges: &[Edge]) -> Option<f64> {
    let mut runs: Vec<f64> = edges
        .windows(2)
        .map(|w| w[1].mid_index - w[0].mid_index)
        .filter(|&r| r > NRZ_GLITCH_SAMPLES)
        .collect();
    if runs.len() < 8 {
        return None;
    }
    runs.sort_by(|a, b| a.total_cmp(b));
    let shortest = runs[0];
    let unit_runs: Vec<f64> = runs
        .iter()
        .copied()
        .filter(|&r| r <= shortest * 1.2)
        .collect();
    let unit = unit_runs[unit_runs.len() / 2];
    let mut lengths = std::collections::BTreeSet::new();
    let (mut judged, mut at_unit, mut longer, mut misfit) = (0usize, 0usize, 0usize, 0usize);
    for &r in &runs {
        let n = (r / unit).round().max(1.0);
        if n > NRZ_MAX_RUN {
            continue;
        }
        judged += 1;
        if (r - n * unit).abs() > NRZ_FIT * unit {
            misfit += 1;
            continue;
        }
        lengths.insert(n as u32);
        if n == 1.0 {
            at_unit += 1;
        } else {
            longer += 1;
        }
    }
    let share = |k: usize| k as f64 / judged.max(1) as f64;
    (share(at_unit) >= NRZ_MIN_SHARE
        && share(longer) >= NRZ_MIN_SHARE
        && share(misfit) < NRZ_MIN_SHARE
        && lengths.len() >= NRZ_MIN_LENGTHS)
        .then_some(unit)
}

pub(super) fn mean_period_samples(edges: &[Edge]) -> Option<f64> {
    let rising: Vec<f64> = edges
        .iter()
        .filter(|e| e.rising)
        .map(|e| e.mid_index)
        .collect();
    if rising.len() < 2 {
        return None;
    }
    let diffs: Vec<f64> = rising.windows(2).map(|w| w[1] - w[0]).collect();
    Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
}

/// Duty cycle (0.0-1.0): for each pair of consecutive rising edges, the
/// fraction of that period occupied before the single falling edge inside
/// it, averaged over every complete period that contains exactly one such
/// falling edge. `None` if no complete period has one.
pub(super) fn mean_duty(edges: &[Edge]) -> Option<f64> {
    let rising_positions: Vec<usize> = edges
        .iter()
        .enumerate()
        .filter(|(_, e)| e.rising)
        .map(|(i, _)| i)
        .collect();
    let mut fractions = Vec::new();
    for pair in rising_positions.windows(2) {
        let (i0, i1) = (pair[0], pair[1]);
        let t0 = edges[i0].mid_index;
        let t1 = edges[i1].mid_index;
        let period = t1 - t0;
        if period <= 0.0 {
            continue;
        }
        let falling_in_between: Vec<f64> = edges[i0 + 1..i1]
            .iter()
            .filter(|e| !e.rising)
            .map(|e| e.mid_index)
            .collect();
        if let [tf] = falling_in_between[..] {
            fractions.push((tf - t0) / period);
        }
    }
    if fractions.is_empty() {
        None
    } else {
        Some(fractions.iter().sum::<f64>() / fractions.len() as f64)
    }
}

/// Mean duration (in samples) of each pulse of the requested polarity: for
/// `high == true`, the span from each rising edge to the very next falling
/// edge; for `high == false`, from each falling edge to the very next rising
/// edge. Edges from both this module's hysteresis detector and a digital
/// transition list strictly alternate direction, so an edge's immediate
/// successor, when it is the opposite polarity, always closes exactly one
/// pulse — no need to search further ahead.
pub(super) fn mean_pulse_width(edges: &[Edge], high: bool) -> Option<f64> {
    let widths: Vec<f64> = edges
        .windows(2)
        .filter(|w| w[0].rising == high && w[1].rising != high)
        .map(|w| w[1].mid_index - w[0].mid_index)
        .collect();
    if widths.is_empty() {
        None
    } else {
        Some(widths.iter().sum::<f64>() / widths.len() as f64)
    }
}

/// Mean rise time and mean fall time, in samples, from the 10%-90% (or
/// 90%-10%) crossings of each edge's own settled-level window — a window
/// bounded by the midpoints to its neighbouring edges, so a fast transition
/// elsewhere in the capture cannot bleed into this one's measurement.
pub(super) fn mean_rise_fall_samples(
    values: &[f64],
    edges: &[Edge],
    top: f64,
    base: f64,
) -> (Option<f64>, Option<f64>) {
    let span = top - base;
    if span <= 0.0 || edges.is_empty() {
        return (None, None);
    }
    let low = base + LOW_FRACTION * span;
    let high = base + HIGH_FRACTION * span;

    let mut rise_times = Vec::new();
    let mut fall_times = Vec::new();

    for (i, edge) in edges.iter().enumerate() {
        let window_start = if i == 0 {
            0
        } else {
            (edges[i - 1].to + edge.from) / 2
        };
        let window_end = if i + 1 == edges.len() {
            values.len() - 1
        } else {
            (edge.to + edges[i + 1].from) / 2
        }
        .max(window_start + 1)
        .min(values.len() - 1);

        if edge.rising {
            let before = directed_crossing(values, window_start..window_end + 1, low, true, true);
            let after = directed_crossing(values, window_start..window_end + 1, high, true, false);
            if let (Some(t_lo), Some(t_hi)) = (before, after) {
                if t_hi > t_lo {
                    rise_times.push(t_hi - t_lo);
                }
            }
        } else {
            let before = directed_crossing(values, window_start..window_end + 1, high, false, true);
            let after = directed_crossing(values, window_start..window_end + 1, low, false, false);
            if let (Some(t_hi), Some(t_lo)) = (before, after) {
                if t_lo > t_hi {
                    fall_times.push(t_lo - t_hi);
                }
            }
        }
    }

    let avg = |v: &[f64]| {
        if v.is_empty() {
            None
        } else {
            Some(v.iter().sum::<f64>() / v.len() as f64)
        }
    };
    (avg(&rise_times), avg(&fall_times))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f64, freq_hz: f64, rate_hz: f64, samples: usize) -> Vec<f64> {
        (0..samples)
            .map(|i| amplitude * (2.0 * std::f64::consts::PI * freq_hz * i as f64 / rate_hz).sin())
            .collect()
    }

    #[test]
    fn top_base_recovers_sine_amplitude() {
        let values = sine(2.0, 1_000.0, 1_000_000.0, 40_000);
        let (top, base) = top_base(&values);
        assert!((top - 2.0).abs() < 0.05, "top {top}");
        assert!((base + 2.0).abs() < 0.05, "base {base}");
    }

    #[test]
    fn top_base_of_a_flat_line_is_that_one_level() {
        let values = vec![1.5; 100];
        assert_eq!(top_base(&values), (1.5, 1.5));
    }

    #[test]
    fn find_edges_counts_one_rising_edge_per_period() {
        let rate_hz = 1_000_000.0;
        let freq_hz = 1_000.0;
        let samples = 10_000; // 10 periods
        let values = sine(2.0, freq_hz, rate_hz, samples);
        let (top, base) = top_base(&values);
        let edges = find_edges(&values, top, base);
        let rising = edges.iter().filter(|e| e.rising).count();
        // 10 periods -> ~10 rising edges (boundary effects may drop one).
        assert!((9..=10).contains(&rising), "rising edges: {rising}");
    }

    #[test]
    fn hysteresis_rejects_a_noisy_plateau_that_never_leaves_its_own_side() {
        // One real transition (base -> top), each plateau perturbed by small
        // noise that stays well inside the hysteresis dead band. Without
        // hysteresis, noise straddling the *raw* midpoint would register as
        // extra edges every time it wobbled past 0; with it, exactly one
        // edge — the real transition — must be found.
        let span = 2.0; // top - base, so 10% hysteresis band = 0.2 wide
        let noise_amplitude = 0.03; // well inside the 0.1 (each side) band
        let plateau_len = 200;
        let mut values = Vec::with_capacity(plateau_len * 2);
        for i in 0..plateau_len {
            values.push(-1.0 + noise_amplitude * ((i as f64) * 1.3).sin());
        }
        for i in 0..plateau_len {
            values.push(1.0 + noise_amplitude * ((i as f64) * 1.7).sin());
        }
        let (top, base) = top_base(&values);
        assert!((top - base - span).abs() < 0.1, "top {top} base {base}");
        let edges = find_edges(&values, top, base);
        assert_eq!(
            edges.len(),
            1,
            "noise confined to one plateau must not add spurious edges"
        );
        assert!(edges[0].rising);
    }

    #[test]
    fn mean_period_needs_at_least_two_like_edges() {
        assert_eq!(mean_period_samples(&[]), None);
        let one = Edge {
            mid_index: 10.0,
            rising: true,
            from: 9,
            to: 11,
        };
        assert_eq!(mean_period_samples(&[one]), None);
    }

    #[test]
    fn duty_of_a_50_percent_square_wave_is_close_to_half() {
        let rate_hz = 1_000_000.0;
        let freq_hz = 1_000.0;
        let period_samples = rate_hz / freq_hz;
        let samples = 10_000;
        let values: Vec<f64> = (0..samples)
            .map(|i| {
                let phase = (i as f64 / period_samples).rem_euclid(1.0);
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            })
            .collect();
        let (top, base) = top_base(&values);
        let edges = find_edges(&values, top, base);
        let duty = mean_duty(&edges).expect("square wave must have a measurable duty");
        assert!((duty - 0.5).abs() < 0.02, "duty {duty}");
    }

    #[test]
    fn rise_time_of_a_trapezoid_matches_its_known_ramp_length() {
        // Settled plateaus at top and base (so histogram-mode top/base finds
        // them exactly), joined by linear ramps of a known length: the
        // 10%-90% crossing of a linear ramp spans exactly 80% of it.
        let low_plateau = 50usize;
        let rise = 40usize;
        let high_plateau = 50usize;
        let fall = 40usize;
        let period = low_plateau + rise + high_plateau + fall;
        let periods = 10;
        let values: Vec<f64> = (0..period * periods)
            .map(|i| {
                let phase = i % period;
                if phase < low_plateau {
                    -1.0
                } else if phase < low_plateau + rise {
                    let t = (phase - low_plateau) as f64 / rise as f64;
                    -1.0 + 2.0 * t
                } else if phase < low_plateau + rise + high_plateau {
                    1.0
                } else {
                    let t = (phase - low_plateau - rise - high_plateau) as f64 / fall as f64;
                    1.0 - 2.0 * t
                }
            })
            .collect();
        let (top, base) = top_base(&values);
        assert!((top - 1.0).abs() < 0.05, "top {top}");
        assert!((base + 1.0).abs() < 0.05, "base {base}");

        let edges = find_edges(&values, top, base);
        let (rise_time, fall_time) = mean_rise_fall_samples(&values, &edges, top, base);
        let rise_time = rise_time.expect("trapezoid must have a measurable rise time");
        let fall_time = fall_time.expect("trapezoid must have a measurable fall time");
        assert!(
            (rise_time - 0.8 * rise as f64).abs() < 2.0,
            "rise time {rise_time} samples, expected ~{}",
            0.8 * rise as f64
        );
        assert!(
            (fall_time - 0.8 * fall as f64).abs() < 2.0,
            "fall time {fall_time} samples, expected ~{}",
            0.8 * fall as f64
        );
    }
}
