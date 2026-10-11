//! Pulse and period timing: the populations a signal's edges fall into, and
//! the ones that do not fit (SPECIFICATION.md §5.6.3, §5.6.4).
//!
//! These two views exist because of what a long capture does to a reader. Two
//! seconds at 24 MS/s is 48 million samples; a person scrolls it and an agent
//! cannot look at it at all. But almost every real question about a digital
//! line is a question about *populations* — "is every bit the same width?",
//! "are there runt pulses?", "did the clock stutter?" — and populations
//! compress: `97 % at 8.68 us, 3 % at 4.34 us` is a complete diagnosis of a
//! line running at half the expected baud rate, in two numbers.
//!
//! As with the eye ([`crate::eye`]), the analysis is separate from the drawing:
//! an agent checking whether anything is wrong wants [`analyze`] and nothing
//! else.

/// One cluster of similar intervals.
#[derive(Debug, Clone, PartialEq)]
pub struct Population {
    pub mean_s: f64,
    pub min_s: f64,
    pub max_s: f64,
    pub count: usize,
    /// Of all intervals of this kind, `0.0..=1.0`.
    pub share: f64,
}

/// One interval that did not fit the dominant population, with where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Outlier {
    /// Seconds from the start of the analysed window.
    pub at_s: f64,
    pub length_s: f64,
    /// Signed fraction by which it differs from the reference: `+0.5` is half
    /// again as long. On NRZ data, from the nearest whole number of unit
    /// intervals, in unit intervals.
    pub deviation: f64,
    /// On NRZ data: the whole number of unit intervals it was held against.
    pub units: Option<u32>,
}

/// What an interval is held against (B-102).
///
/// On a clock or a pulse train every interval should be one length: the
/// dominant population's. On NRZ data — UART, CAN, any line carrying bits —
/// a run is a whole number of bit times, so a 10 us run on a 2 us bit is
/// five bits, not an interval 400 % too long; it is held against the
/// nearest whole multiple of the unit interval instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reference {
    Single(f64),
    Nrz { unit_s: f64 },
}

impl Reference {
    /// The length every deviation is a fraction of.
    pub fn length_s(&self) -> f64 {
        match *self {
            Reference::Single(r) => r,
            Reference::Nrz { unit_s } => unit_s,
        }
    }

    /// The signed deviation of `length_s` and the whole number of units it was
    /// held against; `None` for an idle gap on NRZ data, which is not judged.
    pub fn deviation(&self, length_s: f64) -> Option<(f64, Option<u32>)> {
        match *self {
            Reference::Single(r) => Some(((length_s - r) / r, None)),
            Reference::Nrz { unit_s } => {
                let n = (length_s / unit_s).round().max(1.0);
                (n <= NRZ_MAX_RUN as f64)
                    .then(|| ((length_s - n * unit_s) / unit_s, Some(n as u32)))
            }
        }
    }
}

/// The longest run of equal bits judged on NRZ data; longer runs are idle
/// gaps between frames or bytes, whose length means nothing.
pub const NRZ_MAX_RUN: u32 = 16;
/// How close (in unit intervals) a population must sit to a whole multiple.
const NRZ_FIT: f64 = 0.25;
/// Least share of intervals at one unit, and at two units or more, for a
/// line to read as NRZ data — and least number of distinct multiples. A
/// clock that drops a pulse now and then has a few periods at 2x: those are
/// anomalies, not bits, and two multiples with a 1 % share do not pass.
const NRZ_MIN_SHARE: f64 = 0.1;
const NRZ_MIN_MULTIPLES: usize = 3;

/// The reference intervals are held against: the unit interval when the
/// populations sit at whole multiples of the shortest one (NRZ data), else the
/// dominant population's mean. `None` when there are no populations.
/// Populations no longer than `glitch_s` (glitches, like an I²C SDA's ACK
/// hand-over spikes) are never the unit: everything is a whole number of one
/// sample. They still count as outliers. Nor is a population too rare to be a
/// bit (B-108): one 53 us runt among a bit-banged UART's 101 us bits made it
/// the "unit", and every run of real bits an outlier. The unit is the
/// shortest population with [`NRZ_MIN_SHARE`]; rarer, shorter ones are
/// judged against it like any other interval.
pub fn reference(populations: &[Population], glitch_s: f64) -> Option<Reference> {
    let dominant = populations.first()?.mean_s;
    if dominant <= 0.0 {
        return None;
    }
    let Some(shortest) = populations
        .iter()
        .filter(|p| p.mean_s > glitch_s && p.share >= NRZ_MIN_SHARE)
        .min_by(|a, b| a.mean_s.total_cmp(&b.mean_s))
    else {
        return Some(Reference::Single(dominant));
    };
    let unit = shortest.mean_s;
    let mut multiples = std::collections::BTreeSet::new();
    let (mut longer, mut misfit) = (0.0, 0.0);
    for p in populations.iter().filter(|p| p.mean_s > glitch_s) {
        let n = (p.mean_s / unit).round();
        if n > NRZ_MAX_RUN as f64 {
            continue; // idle gaps
        }
        // A population between two multiples is the anomaly being looked
        // for, not evidence against NRZ data — unless it is a big one.
        if (p.mean_s - n * unit).abs() > NRZ_FIT * unit {
            misfit += p.share;
            continue;
        }
        multiples.insert(n as u32);
        if n >= 2.0 {
            longer += p.share;
        }
    }
    let nrz =
        misfit < NRZ_MIN_SHARE && multiples.len() >= NRZ_MIN_MULTIPLES && longer >= NRZ_MIN_SHARE;
    Some(if nrz {
        Reference::Nrz { unit_s: unit }
    } else {
        Reference::Single(dominant)
    })
}

/// Which intervals a view is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalKind {
    /// Time spent above the threshold.
    High,
    /// Time spent below it.
    Low,
    /// Rising edge to rising edge.
    Period,
}

impl IntervalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IntervalKind::High => "high",
            IntervalKind::Low => "low",
            IntervalKind::Period => "period",
        }
    }
}

/// Everything both views need.
#[derive(Debug, Clone)]
pub struct TimingAnalysis {
    /// Populations per kind, longest-share first.
    pub high: Vec<Population>,
    pub low: Vec<Population>,
    pub periods: Vec<Population>,
    /// Every interval of each kind, in order, with its start time — the raw
    /// material for the histogram's bars and the digest's timeline.
    pub high_intervals: Vec<(f64, f64)>,
    pub low_intervals: Vec<(f64, f64)>,
    pub period_intervals: Vec<(f64, f64)>,
    /// The threshold used to split the signal into high and low.
    pub threshold: f64,
    /// Intervals no longer than [`GLITCH_SAMPLES`] samples — too short to be
    /// a deliberate pulse at this sample rate, and the usual sign of a
    /// floating input or crosstalk rather than of a signal.
    pub glitches: usize,
    pub notes: Vec<String>,
}

/// An interval this many samples long or shorter is called a glitch. Two
/// samples is the shortest thing a sampler can represent at all, so anything
/// at that width says more about the sampling than about the signal.
pub const GLITCH_SAMPLES: usize = 2;

/// How far two intervals may differ and still be called the same population:
/// a relative gap, because the question is always "is this the same kind of
/// pulse", and at 8.68 us that tolerates 1.7 us while at 1 ms it tolerates
/// 200 us.
pub const CLUSTER_TOLERANCE: f64 = 0.2;

/// Beyond this many populations the answer is "a continuum", not a list, and
/// a list of forty is no more readable than the samples were.
pub const MAX_POPULATIONS: usize = 8;

/// Splits `samples` at the mid level and measures every high run, low run and
/// rising-to-rising period.
pub fn analyze(samples: &[f64], rate_hz: f64) -> Result<TimingAnalysis, String> {
    if rate_hz <= 0.0 {
        return Err("sample rate must be positive".to_string());
    }
    if samples.len() < 4 {
        return Err(format!("only {} samples to measure", samples.len()));
    }
    let (lo, hi) = span(samples);
    if hi <= lo {
        return Err(format!(
            "the signal is constant at {lo}: there are no pulses to measure"
        ));
    }
    let threshold = (lo + hi) / 2.0;

    // Runs of "above" and "below", as (start_sample, length_samples).
    let mut runs: Vec<(usize, usize, bool)> = Vec::new();
    let mut start = 0usize;
    let mut level = samples[0] >= threshold;
    for (i, &v) in samples.iter().enumerate().skip(1) {
        let this = v >= threshold;
        if this != level {
            runs.push((start, i - start, level));
            start = i;
            level = this;
        }
    }
    // The last run is discarded: it is cut short by the end of the capture,
    // not by an edge, so its length is an artefact of where the capture
    // stopped. The first is kept only if the capture began with an edge —
    // which it did not, so it goes too.
    if runs.len() < 3 {
        return Err(format!(
            "{} complete run(s) between edges: not enough to measure timing",
            runs.len().saturating_sub(1)
        ));
    }
    let complete = &runs[1..];

    let to_seconds = |samples: usize| samples as f64 / rate_hz;
    let at = |sample: usize| sample as f64 / rate_hz;

    let mut high_intervals = Vec::new();
    let mut low_intervals = Vec::new();
    let mut glitches = 0usize;
    for &(from, length, is_high) in complete {
        if length <= GLITCH_SAMPLES {
            glitches += 1;
        }
        let entry = (at(from), to_seconds(length));
        if is_high {
            high_intervals.push(entry);
        } else {
            low_intervals.push(entry);
        }
    }

    // Periods: the start of one high run to the start of the next.
    let rising: Vec<usize> = complete
        .iter()
        .filter(|&&(_, _, is_high)| is_high)
        .map(|&(from, _, _)| from)
        .collect();
    let period_intervals: Vec<(f64, f64)> = rising
        .windows(2)
        .map(|w| (at(w[0]), to_seconds(w[1] - w[0])))
        .collect();

    let mut notes = Vec::new();
    if glitches > 0 {
        notes.push(format!(
            "{glitches} interval(s) are {GLITCH_SAMPLES} samples or shorter — at this rate that \
             is the shortest thing the sampler can represent, so they say more about the sampling \
             (or a floating input) than about the signal. Capture faster to tell them apart."
        ));
    }
    if period_intervals.is_empty() {
        notes.push(
            "fewer than two rising edges, so there are no periods to report — only pulse widths"
                .to_string(),
        );
    }

    Ok(TimingAnalysis {
        high: cluster(&high_intervals),
        low: cluster(&low_intervals),
        periods: cluster(&period_intervals),
        high_intervals,
        low_intervals,
        period_intervals,
        threshold,
        glitches,
        notes,
    })
}

/// Groups `intervals` into populations by splitting the sorted lengths
/// wherever the step to the next one exceeds [`CLUSTER_TOLERANCE`] of the
/// cluster so far.
///
/// A relative gap rather than a fixed number of bins: the question a reader
/// asks is "is this the same kind of pulse", and a histogram with a fixed bin
/// width either merges 4.3 us with 8.68 us or splits 1 ms into forty bins,
/// depending on which end of the capture it was tuned for.
pub fn cluster(intervals: &[(f64, f64)]) -> Vec<Population> {
    let mut lengths: Vec<f64> = intervals
        .iter()
        .map(|&(_, length)| length)
        .filter(|l| l.is_finite() && *l > 0.0)
        .collect();
    if lengths.is_empty() {
        return Vec::new();
    }
    lengths.sort_by(|a, b| a.partial_cmp(b).expect("finite"));

    let total = lengths.len();
    let mut populations: Vec<Population> = Vec::new();
    let mut group: Vec<f64> = vec![lengths[0]];
    for &length in &lengths[1..] {
        let reference = group.iter().sum::<f64>() / group.len() as f64;
        if (length - reference).abs() / reference.max(f64::MIN_POSITIVE) > CLUSTER_TOLERANCE {
            populations.push(population(&group, total));
            group.clear();
        }
        group.push(length);
    }
    populations.push(population(&group, total));

    // Biggest share first: that is the order a reader wants to read them in,
    // because the first one is "what this signal normally does".
    populations.sort_by_key(|p| std::cmp::Reverse(p.count));
    if populations.len() > MAX_POPULATIONS {
        populations.truncate(MAX_POPULATIONS);
    }
    populations
}

fn population(group: &[f64], total: usize) -> Population {
    let count = group.len();
    let sum: f64 = group.iter().sum();
    Population {
        mean_s: sum / count as f64,
        min_s: group.iter().copied().fold(f64::INFINITY, f64::min),
        max_s: group.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        count,
        share: count as f64 / total.max(1) as f64,
    }
}

/// The intervals that do not fit, worst first, against [`reference`].
///
/// `tolerance` is a fraction: `0.05` reports everything more than 5 % away.
/// The reference is the *dominant* population rather than the overall mean,
/// because an overall mean is dragged by the very outliers being looked for —
/// or, on NRZ data, the nearest whole number of unit intervals.
pub fn outliers(
    intervals: &[(f64, f64)],
    populations: &[Population],
    tolerance: f64,
    limit: usize,
    glitch_s: f64,
) -> (Option<Reference>, Vec<Outlier>) {
    let Some(reference) = reference(populations, glitch_s) else {
        return (None, Vec::new());
    };
    let mut found: Vec<Outlier> = intervals
        .iter()
        .filter_map(|&(at_s, length_s)| {
            let (deviation, units) = reference.deviation(length_s)?;
            (deviation.abs() > tolerance).then_some(Outlier {
                at_s,
                length_s,
                deviation,
                units,
            })
        })
        .collect();
    // Worst first: an agent reads the top of the list and stops.
    found.sort_by(|a, b| {
        b.deviation
            .abs()
            .partial_cmp(&a.deviation.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = found.len();
    found.truncate(limit);
    let _ = total;
    (Some(reference), found)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `period` samples per cycle, half high, `cycles` of them.
    fn square(period: usize, cycles: usize) -> Vec<f64> {
        (0..period * cycles)
            .map(|i| if i % period < period / 2 { 0.0 } else { 1.0 })
            .collect()
    }

    #[test]
    fn a_clean_square_wave_is_one_population_per_kind() {
        let a = analyze(&square(100, 50), 1e6).expect("should analyze");
        assert_eq!(a.high.len(), 1, "{:?}", a.high);
        assert_eq!(a.low.len(), 1, "{:?}", a.low);
        assert_eq!(a.periods.len(), 1, "{:?}", a.periods);
        assert!((a.high[0].mean_s - 50e-6).abs() < 1e-9, "{:?}", a.high[0]);
        assert!((a.periods[0].mean_s - 100e-6).abs() < 1e-9);
        assert_eq!(a.high[0].share, 1.0);
        assert_eq!(a.glitches, 0);
    }

    /// The headline case: a line whose pulses fall into two widths. This is
    /// what "97 % at 8.68 us, 3 % at 4.34 us" looks like from the inside.
    #[test]
    fn two_pulse_widths_are_reported_as_two_populations_with_their_shares() {
        let mut s = Vec::new();
        for i in 0..100 {
            // Every tenth high pulse is half as long.
            let high = if i % 10 == 9 { 25 } else { 50 };
            s.extend(std::iter::repeat_n(1.0, high));
            s.extend(std::iter::repeat_n(0.0, 50));
        }
        let a = analyze(&s, 1e6).expect("should analyze");
        assert_eq!(a.high.len(), 2, "{:?}", a.high);
        // Dominant first.
        assert!((a.high[0].mean_s - 50e-6).abs() < 1e-9, "{:?}", a.high[0]);
        assert!(a.high[0].share > 0.85, "{:?}", a.high[0]);
        assert!((a.high[1].mean_s - 25e-6).abs() < 1e-9, "{:?}", a.high[1]);
        assert!((0.05..0.15).contains(&a.high[1].share), "{:?}", a.high[1]);
        // The low runs are all the same, so they stay one population.
        assert_eq!(a.low.len(), 1, "{:?}", a.low);
    }

    #[test]
    fn a_two_sample_spike_is_called_a_glitch_and_noted() {
        let mut s = Vec::new();
        for i in 0..40 {
            s.extend(std::iter::repeat_n(1.0, 50));
            s.extend(std::iter::repeat_n(0.0, 50));
            if i == 20 {
                // A runt pulse in the middle of a low period.
                s.extend(std::iter::repeat_n(1.0, 2));
                s.extend(std::iter::repeat_n(0.0, 48));
            }
        }
        let a = analyze(&s, 1e6).expect("should analyze");
        assert_eq!(a.glitches, 1, "{:?}", a.high);
        assert!(
            a.notes.iter().any(|n| n.contains("shortest thing")),
            "{:?}",
            a.notes
        );
    }

    #[test]
    fn the_partial_runs_at_each_end_are_not_measured() {
        // Starts mid-high and ends mid-high: both of those runs are cut by the
        // capture rather than by an edge, and counting them would invent a
        // short pulse at each end.
        let mut s = vec![1.0; 20];
        for _ in 0..10 {
            s.extend(std::iter::repeat_n(0.0, 50));
            s.extend(std::iter::repeat_n(1.0, 50));
        }
        s.extend(std::iter::repeat_n(0.0, 7));
        let a = analyze(&s, 1e6).expect("should analyze");
        for population in a.high.iter().chain(a.low.iter()) {
            assert!(
                (population.min_s - 50e-6).abs() < 1e-9,
                "a partial run leaked in: {population:?}"
            );
        }
    }

    #[test]
    fn a_constant_signal_is_refused_with_a_reason() {
        let err = analyze(&[1.0; 100], 1e6).unwrap_err();
        assert!(err.contains("constant"), "{err}");
    }

    #[test]
    fn outliers_are_measured_against_the_dominant_population_not_the_mean() {
        // Ninety periods of 100 us and ten of 200 us: the overall mean is
        // 110 us, which would call every normal period a 9 % outlier.
        let mut intervals = Vec::new();
        for i in 0..100 {
            let length = if i % 10 == 9 { 200e-6 } else { 100e-6 };
            intervals.push((i as f64 * 1e-3, length));
        }
        let populations = cluster(&intervals);
        let (reference, found) = outliers(&intervals, &populations, 0.05, 100, 0.0);
        assert!(
            matches!(reference, Some(Reference::Single(r)) if (r - 100e-6).abs() < 1e-12),
            "{reference:?}"
        );
        assert_eq!(found.len(), 10, "only the long ones are outliers");
        assert!((found[0].deviation - 1.0).abs() < 1e-9, "{:?}", found[0]);
    }

    #[test]
    fn outliers_come_worst_first_and_are_capped() {
        let intervals = vec![
            (0.0, 100e-6),
            (1.0, 100e-6),
            (2.0, 100e-6),
            (3.0, 100e-6),
            (4.0, 110e-6),
            (5.0, 400e-6),
            (6.0, 130e-6),
        ];
        let populations = cluster(&intervals);
        let (_, found) = outliers(&intervals, &populations, 0.05, 2, 0.0);
        assert_eq!(found.len(), 2);
        assert!((found[0].length_s - 400e-6).abs() < 1e-12, "{:?}", found[0]);
        assert!((found[1].length_s - 130e-6).abs() < 1e-12, "{:?}", found[1]);
    }

    /// B-102: CAN-H's dominant runs on the bench, 1, 2, 3 and 5 bits of
    /// 2 us. Every one is a whole number of bits, so nothing is an anomaly —
    /// except a run that is not (here 2.5 bits) — and the idle gap between
    /// frames is not judged at all.
    #[test]
    fn nrz_runs_are_held_against_whole_unit_intervals() {
        let mut intervals = Vec::new();
        let mut t = 0.0;
        for (bits, count) in [(1.0, 40), (5.0, 36), (2.0, 20), (3.0, 4)] {
            for _ in 0..count {
                intervals.push((t, bits * 2e-6 + 20e-9));
                t += 1e-4;
            }
        }
        intervals.push((t, 2.5 * 2e-6)); // a broken run
        intervals.push((t + 1e-4, 3000e-6)); // the idle gap after a frame
        let populations = cluster(&intervals);
        let (reference, found) = outliers(&intervals, &populations, 0.05, 100, 0.0);
        let Some(Reference::Nrz { unit_s }) = reference else {
            panic!("not taken for NRZ data: {reference:?} from {populations:?}");
        };
        assert!((unit_s - 2.02e-6).abs() < 1e-9, "{unit_s}");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            (0.45..=0.5).contains(&found[0].deviation.abs()),
            "{:?}",
            found[0]
        );
        assert!(matches!(found[0].units, Some(2 | 3)), "{:?}", found[0]);
        assert_eq!(Reference::Nrz { unit_s }.deviation(3000e-6), None);
    }

    /// B-108: a bit-banged UART's low runs on the bench — 1, 2, 3, 4 and 6
    /// bits of ~101 us — and one 53 us runt where the transmitter started a
    /// byte late. The runt is too rare to be the unit interval: it is the
    /// one anomaly, and the runs of real bits are not.
    #[test]
    fn a_rare_runt_is_not_the_unit_interval() {
        let mut intervals = Vec::new();
        let mut t = 0.0;
        for (bits, count) in [(1.0, 276), (2.0, 166), (4.0, 35), (3.0, 33), (6.0, 26)] {
            for _ in 0..count {
                intervals.push((t, bits * 101e-6));
                t += 1e-3;
            }
        }
        intervals.push((t, 53e-6));
        let populations = cluster(&intervals);
        let (reference, found) = outliers(&intervals, &populations, 0.05, 100, 0.0);
        let Some(Reference::Nrz { unit_s }) = reference else {
            panic!("not taken for NRZ data: {reference:?} from {populations:?}");
        };
        assert!((unit_s - 101e-6).abs() < 1e-9, "{unit_s}");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!((found[0].length_s - 53e-6).abs() < 1e-12, "{:?}", found[0]);
    }

    /// A pulse train with a runt every tenth pulse, and a clock that drops a
    /// pulse now and then: two lengths at 1x and 2x are anomalies, not bits.
    #[test]
    fn two_multiples_are_no_nrz_data() {
        let runts: Vec<(f64, f64)> = (0..100)
            .map(|i| (i as f64, if i % 10 == 9 { 25e-6 } else { 50e-6 }))
            .collect();
        let (reference, found) = outliers(&runts, &cluster(&runts), 0.05, 100, 0.0);
        assert!(
            matches!(reference, Some(Reference::Single(r)) if (r - 50e-6).abs() < 1e-12),
            "{reference:?}"
        );
        assert_eq!(found.len(), 10);
        let dropped: Vec<(f64, f64)> = (0..400)
            .map(|i| (i as f64, if i % 50 == 7 { 20e-6 } else { 10e-6 }))
            .collect();
        let (reference, found) = outliers(&dropped, &cluster(&dropped), 0.05, 100, 0.0);
        assert!(
            matches!(reference, Some(Reference::Single(r)) if (r - 10e-6).abs() < 1e-12),
            "{reference:?}"
        );
        assert_eq!(found.len(), 8);
    }

    #[test]
    fn no_populations_means_no_outliers_rather_than_a_panic() {
        assert_eq!(outliers(&[], &[], 0.05, 10, 0.0), (None, Vec::new()));
    }

    #[test]
    fn clustering_a_continuum_is_capped_rather_than_listing_everything() {
        // Forty distinct widths, each 30 % longer than the last.
        let intervals: Vec<(f64, f64)> = (0..40)
            .map(|i| (i as f64, 10e-6 * 1.3f64.powi(i)))
            .collect();
        let populations = cluster(&intervals);
        assert_eq!(populations.len(), MAX_POPULATIONS);
    }
}
