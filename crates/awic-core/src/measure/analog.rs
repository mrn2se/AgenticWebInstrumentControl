//! Analog measurements (SPECIFICATION.md §5.4): Vmin/Vmax/Vpp/Vavg/Vrms,
//! frequency/period/duty via hysteresis edge detection, and 10%-90%
//! rise/fall time — see `super`'s module docs for the exact definitions.

use awic_proto::{MeasurementKind, MeasurementResult};

use super::edges::{
    find_edges, mean_duty, mean_period_samples, mean_rise_fall_samples, min_max, top_base, Edge,
};
use super::{not_applicable, ok, ok_noted, unavailable};
use crate::capture_store::{Capture, CaptureData};
use crate::error::Result;

/// Smallest peak-to-peak span, in ADC codes, that frequency/period/duty and
/// rise/fall time are reported for. Hysteresis alone cannot save them below
/// this: it scales with the waveform's own span, so on a flat input the dead
/// band shrinks with the noise and the converter's dither keeps qualifying as
/// edges. On the bench an open oscilloscope input spanning **one** code reported
/// 74.9 kHz at 16.6 % duty, and 1.38 MHz at 18.7 % on another day (live test
/// B-04). Four codes is the line: below it a "frequency" describes the
/// converter, not the signal.
///
/// Amplitude (Vpp/Vmin/Vmax/Vavg/Vrms) is never gated — those numbers are
/// exactly as meaningful on a noisy line as anywhere else.
const MIN_TIMEABLE_SPAN_CODES: f64 = 4.0;

/// Attached to every absolute-level reading (B-41): the oscilloscope's zero offset
/// is not calibrated per range yet (B-14), so the same signal read 0.216 V at
/// 5 V and 0.034 V at 1.25 V. Vpp and timing are differences and ratios, so
/// the offset cancels and they carry no note.
const ZERO_OFFSET_NOTE: &str = "zero offset not calibrated at this range: absolute level \
     uncertain by about \u{b1}0.2 V; Vpp and timing unaffected";

/// An edge this short is not resolved by the sample rate (B-40): at 1 MS/s a
/// 10%-90% time of 534 ns is interpolation between two samples, not a
/// measurement. At or under this many sample periods the value is an upper
/// bound.
const RESOLVED_EDGE_SAMPLES: f64 = 2.0;

/// `1 MS/s`, `250 kS/s` — a sample rate in the unit it lands in.
fn format_rate(hz: f64) -> String {
    if hz >= 1e6 {
        format!("{} MS/s", hz / 1e6)
    } else if hz >= 1e3 {
        format!("{} kS/s", hz / 1e3)
    } else {
        format!("{hz} S/s")
    }
}

/// A rise/fall time in seconds from its length in samples; an upper bound
/// with a note when the edge spans [`RESOLVED_EDGE_SAMPLES`] or fewer.
fn edge_time(kind: MeasurementKind, samples: f64, rate_hz: f64) -> MeasurementResult {
    if samples > RESOLVED_EDGE_SAMPLES {
        return ok(kind, samples / rate_hz, "s");
    }
    ok_noted(
        kind,
        samples / rate_hz,
        "s",
        format!(
            "upper bound: the edge spans \u{2264}2 samples at {}; capture at \u{2265}{} to resolve it",
            format_rate(rate_hz),
            format_rate(rate_hz * 4.0),
        ),
    )
}

pub(super) fn measure(
    capture: &Capture,
    channel: u8,
    kinds: &[MeasurementKind],
) -> Result<Vec<MeasurementResult>> {
    let voltages = channel_voltages(capture, channel)?;
    let rate_hz = capture.meta.rate_hz as f64;

    // Shared structure, computed once regardless of how many kinds were
    // requested.
    let extrema = if voltages.is_empty() {
        None
    } else {
        Some(min_max(&voltages))
    };
    let (top, base) = if voltages.len() >= 2 {
        top_base(&voltages)
    } else {
        (0.0, 0.0)
    };
    // Why timing is refused, when it is — computed before any edge hunting,
    // because on a signal this small the edges themselves are the noise.
    let within_noise = within_noise_floor(capture, channel, extrema);
    // A capture pinned to a converter rail describes the range, not the signal
    // (B-54). Every number below carries it, and where timing is refused it is
    // the reason given: "spans under 4 codes" is true of a clipped capture but
    // names the wrong cause and suggests the wrong fix.
    let clipped = crate::series::channel_clipping(capture, channel).map(|c| c.describe());
    let edge_list = if voltages.len() >= 2 && within_noise.is_none() {
        find_edges(&voltages, top, base)
    } else {
        Vec::new()
    };

    let mut results: Vec<MeasurementResult> = kinds
        .iter()
        .map(|&kind| {
            measure_one(
                kind,
                &voltages,
                extrema,
                top,
                base,
                &edge_list,
                rate_hz,
                within_noise.as_deref(),
                clipped.as_deref(),
            )
        })
        .collect();
    super::note_nrz(&mut results, &edge_list, rate_hz);
    Ok(results)
}

/// `Some(reason)` when the channel's peak-to-peak span is under
/// [`MIN_TIMEABLE_SPAN_CODES`] of its own converter's codes. `None` when it
/// is above the floor — or when the capture carries no usable scale, since a
/// floor in codes means nothing then.
pub(super) fn within_noise_floor(
    capture: &Capture,
    channel: u8,
    extrema: Option<(f64, f64)>,
) -> Option<String> {
    let CaptureData::Analog { channel_scale, .. } = &capture.data else {
        return None;
    };
    let lsb = f64::from(*channel_scale.get(channel as usize)?).abs();
    let (min, max) = extrema?;
    let span = max - min;
    if lsb <= 0.0 || span >= MIN_TIMEABLE_SPAN_CODES * lsb {
        return None;
    }
    Some(format!(
        "signal spans {} ({:.1} ADC codes at this range), under the {:.0} codes timing needs: \
         a frequency, period or duty here would describe the converter's own noise, not the \
         signal. Amplitude readings are unaffected — to time it, raise the amplitude or use a \
         smaller range",
        format_volts(span),
        span / lsb,
        MIN_TIMEABLE_SPAN_CODES,
    ))
}

/// `9.8 mV`, `1.250 V` — a span is easier to judge in the unit it lands in.
fn format_volts(v: f64) -> String {
    if v.abs() < 1.0 {
        format!("{:.1} mV", v * 1e3)
    } else {
        format!("{v:.3} V")
    }
}

/// Extracts one channel's voltages from the interleaved raw samples, using
/// the capture's own per-channel calibration (never derived from bit depth —
/// SPECIFICATION.md §2.3.1). `channel` is a position in the interleave,
/// `0..meta.channel_count`: see [`super::measure_capture`]'s doc comment for
/// why the store cannot address by original hardware channel number.
fn channel_voltages(capture: &Capture, channel: u8) -> Result<Vec<f64>> {
    match crate::series::channel_series(capture, channel)? {
        crate::series::Series::Volts(volts) => Ok(volts),
        crate::series::Series::Levels(_) => {
            unreachable!("analog::measure called on a non-analog capture")
        }
    }
}

/// The reason no period/frequency/duty could be measured, chosen from the
/// most specific cause available.
fn no_edges_reason(voltages: &[f64], top: f64, base: f64) -> String {
    if voltages.len() < 2 {
        "capture has fewer than 2 samples".to_string()
    } else if top <= base {
        "signal is flat: no threshold crossings detected".to_string()
    } else {
        "capture is shorter than one complete period (fewer than two like-polarity edges)"
            .to_string()
    }
}

/// A value with the clipping reason attached when there is one: a number
/// measured off a rail is still a number, and still must not travel alone.
fn noted_if_clipped(
    kind: MeasurementKind,
    value: f64,
    unit: &str,
    clipped: Option<&str>,
) -> MeasurementResult {
    match clipped {
        Some(note) => ok_noted(kind, value, unit, note),
        None => ok(kind, value, unit),
    }
}

#[allow(clippy::too_many_arguments)]
fn measure_one(
    kind: MeasurementKind,
    voltages: &[f64],
    extrema: Option<(f64, f64)>,
    top: f64,
    base: f64,
    edge_list: &[Edge],
    rate_hz: f64,
    within_noise: Option<&str>,
    clipped: Option<&str>,
) -> MeasurementResult {
    use MeasurementKind::*;
    // Everything timed needs a signal above the noise floor; everything
    // measured in volts does not (live test B-04).
    if let (Some(reason), Frequency | Period | Duty | RiseTime | FallTime) = (within_noise, kind) {
        // A clipped capture flattened against a rail has no span left to time,
        // and the noise-floor wording would send the reader after a signal
        // that is too small when it is in fact too large (B-54).
        return unavailable(kind, clipped.unwrap_or(reason).to_string());
    }
    // Clipping does not stop a square wave being timed - the edges are still
    // there - but the levels it was timed against are the range's, so the
    // number travels with the reason.
    let level_note = clipped.unwrap_or(ZERO_OFFSET_NOTE);
    match kind {
        Vpp => match (extrema, clipped) {
            (Some((min, max)), None) => ok(kind, max - min, "V"),
            (Some((min, max)), Some(note)) => ok_noted(kind, max - min, "V", note),
            (None, _) => unavailable(kind, "capture is empty"),
        },
        Vmin => match extrema {
            Some((min, _)) => ok_noted(kind, min, "V", level_note),
            None => unavailable(kind, "capture is empty"),
        },
        Vmax => match extrema {
            Some((_, max)) => ok_noted(kind, max, "V", level_note),
            None => unavailable(kind, "capture is empty"),
        },
        Vavg => {
            if voltages.is_empty() {
                unavailable(kind, "capture is empty")
            } else {
                ok_noted(
                    kind,
                    voltages.iter().sum::<f64>() / voltages.len() as f64,
                    "V",
                    level_note,
                )
            }
        }
        // True RMS, including any DC component (SPECIFICATION.md §5.4): a
        // waveform offset from 0 V reads a higher Vrms than its AC swing
        // alone would, exactly as a real meter reports it.
        Vrms => {
            if voltages.is_empty() {
                unavailable(kind, "capture is empty")
            } else {
                let mean_sq = voltages.iter().map(|v| v * v).sum::<f64>() / voltages.len() as f64;
                ok_noted(kind, mean_sq.sqrt(), "V", level_note)
            }
        }
        Frequency => match mean_period_samples(edge_list) {
            Some(period_samples) if period_samples > 0.0 => {
                noted_if_clipped(kind, rate_hz / period_samples, "Hz", clipped)
            }
            _ => unavailable(kind, no_edges_reason(voltages, top, base)),
        },
        Period => match mean_period_samples(edge_list) {
            Some(period_samples) if period_samples > 0.0 => {
                noted_if_clipped(kind, period_samples / rate_hz, "s", clipped)
            }
            _ => unavailable(kind, no_edges_reason(voltages, top, base)),
        },
        Duty => match mean_duty(edge_list) {
            Some(duty) => noted_if_clipped(kind, duty * 100.0, "%", clipped),
            None => {
                let reason = if mean_period_samples(edge_list).is_some() {
                    "no complete period contains exactly one opposite-polarity edge".to_string()
                } else {
                    no_edges_reason(voltages, top, base)
                };
                unavailable(kind, reason)
            }
        },
        RiseTime => {
            let (rise, _fall) = mean_rise_fall_samples(voltages, edge_list, top, base);
            match rise {
                Some(samples) => edge_time(kind, samples, rate_hz),
                None => unavailable(
                    kind,
                    if edge_list.is_empty() {
                        no_edges_reason(voltages, top, base)
                    } else {
                        "no complete rising transition with clear 10%-90% crossings".to_string()
                    },
                ),
            }
        }
        FallTime => {
            let (_rise, fall) = mean_rise_fall_samples(voltages, edge_list, top, base);
            match fall {
                Some(samples) => edge_time(kind, samples, rate_hz),
                None => unavailable(
                    kind,
                    if edge_list.is_empty() {
                        no_edges_reason(voltages, top, base)
                    } else {
                        "no complete falling transition with clear 10%-90% crossings".to_string()
                    },
                ),
            }
        }
        PulseWidthHigh | PulseWidthLow | EdgeCount => not_applicable(kind, "analog", "digital"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::analog_capture;
    use super::*;

    fn sine_i16(amplitude: f64, freq_hz: f64, rate_hz: f64, samples: usize) -> Vec<u8> {
        let mut raw = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let v = amplitude * (2.0 * std::f64::consts::PI * freq_hz * i as f64 / rate_hz).sin();
            let code = v.round() as i16;
            raw.extend_from_slice(&code.to_le_bytes());
        }
        raw
    }

    #[test]
    fn vpp_vmin_vmax_vavg_of_a_known_signal() {
        // Codes 0, 128, 255 on a scale/offset that recovers exact volts.
        let capture = analog_capture(1, 8, vec![0.02], vec![-2.0], vec![0, 100, 255], 1_000);
        let results = measure(
            &capture,
            0,
            &[
                MeasurementKind::Vmin,
                MeasurementKind::Vmax,
                MeasurementKind::Vpp,
            ],
        )
        .unwrap();
        let MeasurementResult::Ok {
            value: vmin, unit, ..
        } = &results[0]
        else {
            panic!("expected Ok, got {:?}", results[0]);
        };
        assert_eq!(unit, "V");
        assert!((vmin - (-2.0)).abs() < 1e-9);
        let MeasurementResult::Ok { value: vmax, .. } = &results[1] else {
            panic!("expected Ok");
        };
        // Compare against the same f32 scale/offset converted to f64 the way
        // production code does — an f64 literal `0.02` is not bit-identical
        // to `f32::from(0.02f32)`, so comparing against the literal directly
        // would fail on a precision difference that has nothing to do with
        // correctness.
        let expected_vmax = f64::from(0.02f32) * 255.0 + f64::from(-2.0f32);
        assert!((vmax - expected_vmax).abs() < 1e-9);
        let MeasurementResult::Ok { value: vpp, .. } = &results[2] else {
            panic!("expected Ok");
        };
        assert!((vpp - (vmax - vmin)).abs() < 1e-9);
    }

    #[test]
    fn a_capture_shorter_than_one_period_reports_frequency_unavailable() {
        // Less than one full cycle of a 1 kHz sine at 1 MS/s.
        let raw = sine_i16(1000.0, 1_000.0, 1_000_000.0, 300);
        let capture = analog_capture(1, 16, vec![0.001], vec![0.0], raw, 1_000_000);
        let results = measure(&capture, 0, &[MeasurementKind::Frequency]).unwrap();
        match &results[0] {
            MeasurementResult::Unavailable { unavailable, .. } => {
                assert!(!unavailable.is_empty());
            }
            other => panic!("expected Unavailable for a sub-period capture, got {other:?}"),
        }
    }

    /// B-54: at the 2.5 V range a 2.7-4.1 V bus comes back as code 255 on
    /// every sample. `vpp 0`, `vmin = vmax = 2.50 V` and "spans under 4 codes"
    /// are all true of that capture and none of them about the signal.
    #[test]
    fn a_capture_clipped_against_a_rail_says_so_instead_of_blaming_the_noise_floor() {
        let capture = analog_capture(
            1,
            8,
            vec![5.0 / 255.0],
            vec![-2.5],
            vec![255; 2000],
            8_000_000,
        );
        let results = measure(
            &capture,
            0,
            &[
                MeasurementKind::Vpp,
                MeasurementKind::Vmax,
                MeasurementKind::Frequency,
                MeasurementKind::Duty,
            ],
        )
        .unwrap();
        for (i, result) in results[..2].iter().enumerate() {
            let MeasurementResult::Ok { note, .. } = result else {
                panic!("expected a value for {i}, got {result:?}");
            };
            let note = note.as_deref().unwrap_or_default();
            assert!(note.contains("clipped"), "{i}: {note}");
            assert!(note.contains("every sample"), "{i}: {note}");
            assert!(note.contains("larger range"), "{i}: {note}");
        }
        // Timing is still refused - a rail has no edges - but for the right
        // reason, which points at a range that is too small, not a signal that
        // is too quiet.
        for result in &results[2..] {
            let MeasurementResult::Unavailable { unavailable, .. } = result else {
                panic!("expected Unavailable, got {result:?}");
            };
            assert!(unavailable.contains("clipped"), "{unavailable}");
            assert!(
                !unavailable.contains("codes timing needs"),
                "the noise-floor reason names the wrong cause: {unavailable}"
            );
        }
    }

    /// Clipping does not stop a square wave being timed; the number simply
    /// travels with the reason its levels cannot be trusted.
    #[test]
    fn a_clipped_square_wave_is_still_timed_but_carries_the_reason() {
        // 1 kHz square at 1 MS/s, both halves driven past the rails.
        let raw: Vec<u8> = (0..4000)
            .map(|i| if (i / 500) % 2 == 0 { 0 } else { 255 })
            .collect();
        let capture = analog_capture(1, 8, vec![5.0 / 255.0], vec![-2.5], raw, 1_000_000);
        let results = measure(&capture, 0, &[MeasurementKind::Frequency]).unwrap();
        let MeasurementResult::Ok { value, note, .. } = &results[0] else {
            panic!("a clipped square wave still has edges: {:?}", results[0]);
        };
        assert!((value - 1000.0).abs() < 1.0, "{value} Hz");
        let note = note.as_deref().unwrap_or_default();
        assert!(note.contains("clipped"), "{note}");
    }

    /// B-04: an open oscilloscope input carries a code or two of dither. Timing it
    /// produced 74.9 kHz at 16.6 % duty on the bench; amplitude was fine.
    #[test]
    fn noise_a_couple_of_codes_wide_is_not_timed_but_is_still_measured() {
        // Two codes of dither around 128, the way an open input reads.
        let raw: Vec<u8> = (0..2000)
            .map(|i| if i % 3 == 0 { 129 } else { 128 })
            .collect();
        let capture = analog_capture(1, 8, vec![0.0098], vec![-1.25], raw, 1_000_000);
        let results = measure(
            &capture,
            0,
            &[
                MeasurementKind::Frequency,
                MeasurementKind::Duty,
                MeasurementKind::Period,
                MeasurementKind::RiseTime,
                MeasurementKind::Vpp,
            ],
        )
        .unwrap();
        for (kind, result) in ["frequency", "duty", "period", "rise_time"]
            .iter()
            .zip(&results)
        {
            let MeasurementResult::Unavailable { unavailable, .. } = result else {
                panic!("{kind} must not be timed on noise, got {result:?}");
            };
            assert!(
                unavailable.contains("ADC codes") && unavailable.contains("noise"),
                "{kind}: {unavailable}"
            );
        }
        // Amplitude is still a real answer: ~1 code.
        let MeasurementResult::Ok { value: vpp, .. } = &results[4] else {
            panic!("vpp must stay available on noise: {:?}", results[4]);
        };
        assert!((0.005..0.02).contains(vpp), "vpp {vpp}");
    }

    /// The floor must not swallow a real signal that happens to be small: a
    /// square wave spanning ten codes is timed as usual.
    #[test]
    fn a_small_but_real_square_wave_is_still_timed() {
        // 1 kHz square at 1 MS/s: 500 samples high, 500 low, 10 codes apart.
        let mut raw = Vec::new();
        for _ in 0..10 {
            raw.extend(std::iter::repeat_n(133u8, 500));
            raw.extend(std::iter::repeat_n(123u8, 500));
        }
        let capture = analog_capture(1, 8, vec![0.0098], vec![-1.25], raw, 1_000_000);
        let results = measure(
            &capture,
            0,
            &[MeasurementKind::Frequency, MeasurementKind::Duty],
        )
        .unwrap();
        let MeasurementResult::Ok { value: freq, .. } = &results[0] else {
            panic!(
                "a 10-code square wave must still be timed: {:?}",
                results[0]
            );
        };
        assert!((freq - 1000.0).abs() < 5.0, "frequency {freq}");
        let MeasurementResult::Ok { value: duty, .. } = &results[1] else {
            panic!("duty: {:?}", results[1]);
        };
        assert!((duty - 50.0).abs() < 2.0, "duty {duty}");
    }

    #[test]
    fn a_flat_line_reports_vpp_near_zero_and_period_unavailable() {
        let capture = analog_capture(1, 8, vec![0.04], vec![-5.0], vec![128; 1000], 1_000_000);
        let results = measure(
            &capture,
            0,
            &[
                MeasurementKind::Vpp,
                MeasurementKind::Period,
                MeasurementKind::Duty,
            ],
        )
        .unwrap();
        match &results[0] {
            MeasurementResult::Ok { value, .. } => assert!(value.abs() < 0.1, "vpp {value}"),
            other => panic!("expected Ok for vpp on a flat line, got {other:?}"),
        }
        for r in &results[1..] {
            assert!(
                matches!(r, MeasurementResult::Unavailable { .. }),
                "expected Unavailable, got {r:?}"
            );
        }
    }

    fn note_of(r: &MeasurementResult) -> Option<&str> {
        match r {
            MeasurementResult::Ok { note, .. } => note.as_deref(),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// 8-bit square wave, `rise` samples of ramp on each edge.
    /// A square wave between codes [`SQUARE_LOW`] and [`SQUARE_HIGH`], which
    /// sit just inside the converter's range on purpose: a fixture at codes 0
    /// and 255 is a capture clipped against both rails (B-54), and the
    /// clipping note would then travel with every reading these tests check.
    fn square_u8(plateau: usize, ramp: usize, periods: usize) -> Vec<u8> {
        let swing = f64::from(SQUARE_HIGH - SQUARE_LOW);
        let ramp_code =
            |i: usize| SQUARE_LOW + (swing * i as f64 / (ramp + 1) as f64).round() as u8;
        let mut raw = Vec::new();
        for _ in 0..periods {
            raw.extend(std::iter::repeat_n(SQUARE_LOW, plateau));
            raw.extend((1..=ramp).map(ramp_code));
            raw.extend(std::iter::repeat_n(SQUARE_HIGH, plateau));
            raw.extend((1..=ramp).rev().map(ramp_code));
        }
        raw
    }

    const SQUARE_LOW: u8 = 8;
    const SQUARE_HIGH: u8 = 247;

    /// B-41: absolute levels carry the zero-offset caveat; differences and
    /// timing do not.
    #[test]
    fn absolute_volts_carry_the_zero_offset_note_but_vpp_and_timing_do_not() {
        let capture = analog_capture(
            1,
            8,
            vec![0.04],
            vec![-5.0],
            square_u8(50, 10, 10),
            1_000_000,
        );
        let kinds = [
            MeasurementKind::Vmin,
            MeasurementKind::Vmax,
            MeasurementKind::Vavg,
            MeasurementKind::Vrms,
            MeasurementKind::Vpp,
            MeasurementKind::Frequency,
            MeasurementKind::Duty,
            MeasurementKind::RiseTime,
        ];
        let results = measure(&capture, 0, &kinds).unwrap();
        for r in &results[..4] {
            assert!(
                note_of(r).is_some_and(|n| n.contains("zero offset not calibrated")),
                "{r:?}"
            );
        }
        for r in &results[4..] {
            assert_eq!(note_of(r), None, "{r:?}");
        }
    }

    /// B-40: an edge that crosses in one sample is an upper bound, not the
    /// interpolated 0.8-sample "measurement"; a resolved edge is not.
    #[test]
    fn a_one_sample_edge_is_reported_as_an_upper_bound() {
        let capture = analog_capture(
            1,
            8,
            vec![0.04],
            vec![-5.0],
            square_u8(50, 0, 10),
            1_000_000,
        );
        let results = measure(
            &capture,
            0,
            &[MeasurementKind::RiseTime, MeasurementKind::FallTime],
        )
        .unwrap();
        for r in &results {
            let n = note_of(r).unwrap_or_else(|| panic!("no note on {r:?}"));
            assert!(n.contains("upper bound") && n.contains("1 MS/s"), "{n}");
            assert!(n.contains("4 MS/s"), "must suggest a rate: {n}");
        }
        let slow = analog_capture(
            1,
            8,
            vec![0.04],
            vec![-5.0],
            square_u8(50, 20, 10),
            1_000_000,
        );
        let results = measure(&slow, 0, &[MeasurementKind::RiseTime]).unwrap();
        assert_eq!(note_of(&results[0]), None, "{:?}", results[0]);
    }
}
