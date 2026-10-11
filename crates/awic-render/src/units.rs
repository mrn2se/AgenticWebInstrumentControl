//! Axis ticks and SI-prefixed numbers.
//!
//! A plot whose y axis reads `0.0039062` says less than one reading `3.91 mV`,
//! and a model parsing the label has the same problem a person does.

/// `value` with an SI prefix and three significant figures, e.g. `1.00 kHz`,
/// `3.91 mV`, `250 ns`. `unit` may be empty (a unitless digital level).
pub fn si(value: f64, unit: &str) -> String {
    if !value.is_finite() {
        return format!("n/a {unit}").trim().to_string();
    }
    if value == 0.0 {
        return if unit.is_empty() {
            "0".to_string()
        } else {
            format!("0 {unit}")
        };
    }
    // A number with no unit is a ratio or a count, and a percentage is already
    // scaled: an SI prefix on either is nonsense. A fraction of 0.5 came back
    // as "500 m" and a share of 0.6 % as "599 m%" until this was here.
    if unit.is_empty() || unit == "%" {
        let digits = if value.abs() >= 100.0 {
            0
        } else if value.abs() >= 10.0 {
            1
        } else {
            3
        };
        let number = format!("{value:.digits$}");
        let trimmed = if digits > 0 {
            number
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        } else {
            number
        };
        return if unit.is_empty() {
            trimmed
        } else {
            format!("{trimmed} {unit}")
        };
    }
    let (prefix, scale) = prefix_for(value.abs());
    let mantissa = value / scale;
    let digits = if mantissa.abs() >= 100.0 {
        0
    } else if mantissa.abs() >= 10.0 {
        1
    } else {
        2
    };
    let number = format!("{mantissa:.digits$}");
    if unit.is_empty() && prefix.is_empty() {
        number
    } else {
        format!("{number} {prefix}{unit}")
    }
}

/// The SI prefix and its scale for `magnitude`: the largest prefix that still
/// leaves a mantissa of 1 or more.
fn prefix_for(magnitude: f64) -> (&'static str, f64) {
    const PREFIXES: [(&str, f64); 9] = [
        ("p", 1e-12),
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("", 1.0),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
    ];
    PREFIXES
        .iter()
        .rev()
        .find(|(_, s)| magnitude / s >= 1.0)
        .copied()
        .unwrap_or(("p", 1e-12))
}

/// Labels for one axis's `ticks`: all in the same unit, each with the
/// precision the tick *spacing* calls for.
///
/// [`si`] is given one number and shows three significant figures of it, which
/// is right for a measurement and wrong for an axis: a 320 µs window starting
/// at 398.78 ms has 50 µs between ticks, and three figures of each tick spells
/// every one of them "399 ms" (B-53). The unit comes from the largest tick, so
/// the whole axis shares it, and the decimals from the smallest gap, so
/// neighbouring labels always differ.
pub fn tick_labels(ticks: &[f64], unit: &str) -> Vec<String> {
    let largest = ticks.iter().copied().fold(0.0f64, |a, t| a.max(t.abs()));
    let step = ticks
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .filter(|g| *g > 0.0)
        .fold(f64::INFINITY, f64::min);
    // A single tick has no spacing to go by; `si` already does the right thing
    // with one number on its own.
    if !step.is_finite() || step <= 0.0 {
        return ticks.iter().map(|&t| si(t, unit)).collect();
    }
    // An axis whose ticks are all at zero-ish magnitudes takes its unit from
    // the spacing instead — "0 s, 50 µs, 100 µs" would otherwise mix units.
    let (prefix, scale) = prefix_for(if largest > 0.0 { largest } else { step });
    let scaled_step = step / scale;
    let decimals = if scaled_step > 0.0 {
        (-scaled_step.log10().floor()).clamp(0.0, 6.0) as usize
    } else {
        0
    };
    ticks
        .iter()
        .map(|&t| {
            let number = format!("{:.decimals$}", t / scale);
            if unit.is_empty() && prefix.is_empty() {
                number
            } else {
                format!("{number} {prefix}{unit}")
            }
        })
        .collect()
}

/// Tick positions covering `[lo, hi]` at a 1/2/5 × 10^n step, aiming for
/// about `target` of them. Returns an empty vector for a degenerate range
/// rather than looping.
pub fn ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    if !lo.is_finite() || !hi.is_finite() || hi <= lo || target == 0 {
        return Vec::new();
    }
    let raw = (hi - lo) / target as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    let normalized = raw / magnitude;
    let step = if normalized <= 1.0 {
        magnitude
    } else if normalized <= 2.0 {
        2.0 * magnitude
    } else if normalized <= 5.0 {
        5.0 * magnitude
    } else {
        10.0 * magnitude
    };
    let first = (lo / step).ceil() * step;
    let mut out = Vec::new();
    let mut t = first;
    // Bounded: `step > 0` and the loop is also capped, so a pathological
    // range cannot spin here.
    while t <= hi + step * 1e-9 && out.len() <= target * 4 {
        out.push(t);
        t += step;
    }
    out
}

/// Tick positions on a log10 axis: `1`, `2`, `5` times each power of ten
/// inside `[lo, hi]`. Empty for a non-positive or degenerate range.
///
/// Pulse widths need this. A capture can hold 80 ns glitches next to 1 ms
/// pulses, and a linear axis puts every one of them in the leftmost pixel.
pub fn log_ticks(lo: f64, hi: f64) -> Vec<f64> {
    if lo <= 0.0 || hi <= lo || !hi.is_finite() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let first = lo.log10().floor() as i32;
    let last = hi.log10().ceil() as i32;
    for decade in first..=last {
        for mantissa in [1.0, 2.0, 5.0] {
            let t = mantissa * 10f64.powi(decade);
            if t >= lo && t <= hi {
                out.push(t);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn si_picks_the_prefix_that_keeps_the_mantissa_readable() {
        assert_eq!(si(1000.0, "Hz"), "1.00 kHz");
        assert_eq!(si(999.99, "Hz"), "1000 Hz");
        assert_eq!(si(0.0039062, "V"), "3.91 mV");
        assert_eq!(si(250e-9, "s"), "250 ns");
        assert_eq!(si(24e6, "Hz"), "24.0 MHz");
        assert_eq!(si(-1.5, "V"), "-1.50 V");
    }

    #[test]
    fn a_unitless_number_gets_no_prefix() {
        assert_eq!(si(0.5, ""), "0.5");
        assert_eq!(si(1.0, ""), "1");
        assert_eq!(si(1234.0, ""), "1234");
        assert_eq!(si(0.125, ""), "0.125");
    }

    /// A percentage is already scaled. "599 m%" for a 0.6 % share is how this
    /// looked on the bench before the rule covered `%` too.
    #[test]
    fn a_percentage_gets_no_prefix_either() {
        assert_eq!(si(0.599, "%"), "0.599 %");
        assert_eq!(si(5.0, "%"), "5 %");
        assert_eq!(si(99.4, "%"), "99.4 %");
        assert_eq!(si(17199529.1, "%"), "17199529 %");
        assert_eq!(si(0.0, "%"), "0 %");
    }

    #[test]
    fn si_handles_zero_and_nonsense_without_panicking() {
        assert_eq!(si(0.0, "V"), "0 V");
        assert_eq!(si(0.0, ""), "0");
        assert_eq!(si(f64::NAN, "V"), "n/a V");
        assert_eq!(si(f64::INFINITY, ""), "n/a");
    }

    /// B-53: a 320 µs window starting at 398.78 ms has 50 µs between ticks.
    /// Three significant figures of each tick spells every label "399 ms".
    #[test]
    fn tick_labels_carry_the_digits_the_spacing_needs() {
        let t = ticks(0.39878, 0.39910, 8);
        let labels = tick_labels(&t, "s");
        assert!(labels.len() >= 4, "{labels:?}");
        // Every label distinct, which is the whole point.
        let mut seen = labels.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), labels.len(), "repeated labels: {labels:?}");
        assert!(labels.iter().all(|l| l.ends_with(" ms")), "{labels:?}");
        assert!(labels.contains(&"398.90 ms".to_string()), "{labels:?}");
        // And `si` on its own is what it used to do - the reason for the fix.
        assert_eq!(si(0.39890, "s"), "399 ms");
    }

    #[test]
    fn tick_labels_share_one_unit_across_the_axis() {
        // An axis through zero must not read "0 s, 50 µs, 100 µs".
        let labels = tick_labels(&[0.0, 50e-6, 100e-6, 150e-6], "s");
        assert_eq!(labels, ["0 us", "50 us", "100 us", "150 us"]);
        // Volts work the same way.
        assert_eq!(
            tick_labels(&[2.5, 3.0, 3.5], "V"),
            ["2.5 V", "3.0 V", "3.5 V"]
        );
    }

    #[test]
    fn a_single_tick_or_none_still_gets_a_label() {
        assert_eq!(tick_labels(&[0.001], "s"), ["1.00 ms"]);
        assert!(tick_labels(&[], "s").is_empty());
        // Identical ticks have no spacing to read; fall back rather than divide
        // by zero.
        assert_eq!(tick_labels(&[1.0, 1.0], "s"), ["1.00 s", "1.00 s"]);
    }

    #[test]
    fn ticks_are_round_numbers_inside_the_range() {
        let t = ticks(0.0, 1.0, 5);
        assert_eq!(t, vec![0.0, 0.2, 0.4, 0.6000000000000001, 0.8, 1.0]);
        let t = ticks(-3.0, 3.0, 4);
        assert!(t.contains(&0.0) && t.first().unwrap() >= &-3.0 && t.last().unwrap() <= &3.0);
    }

    #[test]
    fn log_ticks_land_on_one_two_five_per_decade() {
        let got = log_ticks(1e-6, 1e-4);
        let want = [1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4];
        assert_eq!(got.len(), want.len(), "{got:?}");
        // Approximately: `5.0 * 1e-6` is not exactly `5e-6` in binary.
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < w * 1e-9, "{g} vs {w}");
        }
        // Nothing outside the range, and a range inside one decade still works.
        let t = log_ticks(3e-6, 8e-6);
        assert_eq!(t.len(), 1, "{t:?}");
        assert!((t[0] - 5e-6).abs() < 1e-15, "{t:?}");
    }

    #[test]
    fn log_ticks_refuse_a_range_that_cannot_be_logarithmic() {
        assert!(log_ticks(0.0, 1.0).is_empty());
        assert!(log_ticks(-1.0, 1.0).is_empty());
        assert!(log_ticks(1.0, 1.0).is_empty());
        assert!(log_ticks(1.0, f64::INFINITY).is_empty());
    }

    #[test]
    fn a_degenerate_range_yields_no_ticks_rather_than_hanging() {
        assert!(ticks(1.0, 1.0, 5).is_empty());
        assert!(ticks(1.0, 0.0, 5).is_empty());
        assert!(ticks(f64::NAN, 1.0, 5).is_empty());
        assert!(ticks(0.0, 1.0, 0).is_empty());
    }
}
