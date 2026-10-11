//! Digital measurements (SPECIFICATION.md §5.4), derived from the capture's
//! transition list rather than by rescanning the packed bytes — see
//! `super`'s module docs. A "channel" here is the bit index within the
//! packed byte (§3.2: "a packed byte is a bitmask; channel n is bit n"), so
//! extracting one channel's edges from the shared transition list is just
//! masking each transition's bit and keeping the ones where it actually
//! flipped — a neighbouring channel toggling inside the same transition
//! cannot register as an edge on this one.

use awic_proto::{MeasurementKind, MeasurementResult};

use super::edges::{mean_duty, mean_period_samples, mean_pulse_width, Edge};
use super::{channel_not_found, not_applicable, ok, ok_noted, unavailable};
use crate::capture_store::{Capture, CaptureData, Transition};
use crate::error::Result;

pub(super) fn measure(
    capture: &Capture,
    channel: u8,
    kinds: &[MeasurementKind],
) -> Result<Vec<MeasurementResult>> {
    // The packed encoding is always a `u8` bitmask (SPECIFICATION.md §3.2),
    // and the store does not retain which of those 8 bits were actually
    // driven by an enabled channel — see `super::measure_capture`'s doc
    // comment — so 0..=7 is the only bound that can honestly be enforced.
    if channel > 7 {
        return Err(channel_not_found(
            capture,
            channel,
            "digital channels are bits 0-7 of the packed byte",
        ));
    }
    let CaptureData::Digital {
        packed,
        transitions,
    } = &capture.data
    else {
        unreachable!("digital::measure called on a non-digital capture")
    };
    let rate_hz = capture.meta.rate_hz as f64;
    let edge_list = channel_edges(transitions, channel);
    let empty_capture = packed.is_empty();
    let bursts = burst_stats(&edge_list, rate_hz, packed.len());

    let mut results: Vec<MeasurementResult> = kinds
        .iter()
        .map(|&kind| match &bursts {
            Some(b) => burst_one(kind, b)
                .unwrap_or_else(|| measure_one(kind, &edge_list, rate_hz, empty_capture)),
            None => measure_one(kind, &edge_list, rate_hz, empty_capture),
        })
        .collect();
    super::note_nrz(&mut results, &edge_list, rate_hz);
    Ok(results)
}

/// What a bursty line does *inside* its bursts (B-39): the medians of each
/// in-burst period and pulse width, in samples.
struct BurstStats {
    count: usize,
    period: f64,
    high: f64,
    low: f64,
    rate_hz: f64,
}

/// Median of a non-empty slice (sorted in place).
fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// `Some` when the channel is bursty the way the survey defines it (B-47):
/// idle gaps longer than 1 ms — `rate_hz / 1000` samples — between groups of
/// edges, or idle before the first / after the last edge. A mean over such a
/// line averages the clock with its idle time (an I²C SCL at 365 kHz inside
/// 29 ms bursts read 39.8 kHz), so the figure that means something is the
/// median inside the bursts. Needs at least one burst long enough to hold a
/// whole period, or there is no in-burst clock to report and the plain
/// measurement stands (a slow 400 Hz clock has 1.25 ms gaps but no bursts).
fn burst_stats(edges: &[Edge], rate_hz: f64, capture_len: usize) -> Option<BurstStats> {
    let gap = (rate_hz / 1000.0).floor().max(1.0);
    let (first, last) = (edges.first()?, edges.last()?);
    let mut groups: Vec<&[Edge]> = Vec::new();
    let mut start = 0;
    for i in 1..edges.len() {
        if edges[i].mid_index - edges[i - 1].mid_index > gap {
            groups.push(&edges[start..i]);
            start = i;
        }
    }
    groups.push(&edges[start..]);
    let idle_edge = first.mid_index > gap || capture_len as f64 - last.mid_index > gap;
    if groups.len() < 2 && !idle_edge {
        return None;
    }
    let (mut periods, mut highs, mut lows) = (Vec::new(), Vec::new(), Vec::new());
    for g in &groups {
        let rising: Vec<f64> = g.iter().filter(|e| e.rising).map(|e| e.mid_index).collect();
        periods.extend(rising.windows(2).map(|w| w[1] - w[0]));
        for w in g.windows(2) {
            let width = w[1].mid_index - w[0].mid_index;
            if w[0].rising {
                highs.push(width);
            } else {
                lows.push(width);
            }
        }
    }
    if periods.is_empty() || highs.is_empty() || lows.is_empty() {
        return None;
    }
    Some(BurstStats {
        count: groups.len(),
        period: median(&mut periods),
        high: median(&mut highs),
        low: median(&mut lows),
        rate_hz,
    })
}

/// The in-burst answer for the kinds a burst changes, with the note saying so
/// (B-39). `None` for kinds that are not about the clock (edge count).
fn burst_one(kind: MeasurementKind, b: &BurstStats) -> Option<MeasurementResult> {
    use MeasurementKind::*;
    let note = format!(
        "bursty line: describes the line inside {} burst{} (median of the in-burst values), \
         not the whole capture, which includes idle stretches longer than 1 ms",
        b.count,
        if b.count == 1 { "" } else { "s" }
    );
    Some(match kind {
        Frequency => ok_noted(kind, b.rate_hz / b.period, "Hz", note),
        Period => ok_noted(kind, b.period / b.rate_hz, "s", note),
        Duty => ok_noted(kind, b.high / (b.high + b.low) * 100.0, "%", note),
        PulseWidthHigh => ok_noted(kind, b.high / b.rate_hz, "s", note),
        PulseWidthLow => ok_noted(kind, b.low / b.rate_hz, "s", note),
        _ => return None,
    })
}

/// Extracts one channel's edges from the shared transition list: each
/// transition's `channel_mask` is the packed byte's *new* state, so a bit
/// only contributes an edge when it actually differs from its own previous
/// value — a neighbouring channel's bit flipping inside the same transition
/// changes `channel_mask` without producing an edge here.
fn channel_edges(transitions: &[Transition], channel: u8) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut prev_bit: Option<bool> = None;
    for t in transitions {
        let bit = (t.channel_mask >> channel) & 1 == 1;
        match prev_bit {
            None => prev_bit = Some(bit),
            Some(p) if p != bit => {
                edges.push(Edge {
                    mid_index: t.sample_index as f64,
                    rising: bit,
                    from: 0,
                    to: 0,
                });
                prev_bit = Some(bit);
            }
            _ => {}
        }
    }
    edges
}

fn no_edges_reason(edges: &[Edge]) -> String {
    if edges.is_empty() {
        "channel never toggles in this capture".to_string()
    } else {
        "capture is shorter than one complete period (fewer than two like-polarity edges)"
            .to_string()
    }
}

fn measure_one(
    kind: MeasurementKind,
    edges: &[Edge],
    rate_hz: f64,
    empty_capture: bool,
) -> MeasurementResult {
    use MeasurementKind::*;
    if empty_capture {
        return unavailable(kind, "capture is empty");
    }
    match kind {
        Frequency => match mean_period_samples(edges) {
            Some(period_samples) if period_samples > 0.0 => {
                ok(kind, rate_hz / period_samples, "Hz")
            }
            _ => unavailable(kind, no_edges_reason(edges)),
        },
        Period => match mean_period_samples(edges) {
            Some(period_samples) if period_samples > 0.0 => ok(kind, period_samples / rate_hz, "s"),
            _ => unavailable(kind, no_edges_reason(edges)),
        },
        Duty => match mean_duty(edges) {
            Some(duty) => ok(kind, duty * 100.0, "%"),
            None => {
                let reason = if mean_period_samples(edges).is_some() {
                    "no complete period contains exactly one opposite-polarity edge".to_string()
                } else {
                    no_edges_reason(edges)
                };
                unavailable(kind, reason)
            }
        },
        PulseWidthHigh => match mean_pulse_width(edges, true) {
            Some(samples) => ok(kind, samples / rate_hz, "s"),
            None => unavailable(
                kind,
                "no complete high pulse observed (a rising edge followed by a falling edge)",
            ),
        },
        PulseWidthLow => match mean_pulse_width(edges, false) {
            Some(samples) => ok(kind, samples / rate_hz, "s"),
            None => unavailable(
                kind,
                "no complete low pulse observed (a falling edge followed by a rising edge)",
            ),
        },
        // Zero is a real, honest answer here (a channel that never toggles),
        // never withheld the way a period/duty guess would be.
        EdgeCount => ok(kind, edges.len() as f64, "count"),
        Vpp | Vmin | Vmax | Vavg | Vrms | RiseTime | FallTime => {
            not_applicable(kind, "digital", "analog")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::digital_capture;
    use super::*;

    /// A perfect square wave on bit 0: high for `half`, low for `half`,
    /// repeating — an exact, hand-computable ground truth.
    fn clock_packed(half_period: usize, cycles: usize, bit: u8) -> Vec<u8> {
        let mut packed = Vec::with_capacity(half_period * 2 * cycles);
        for _ in 0..cycles {
            packed.extend(std::iter::repeat_n(1u8 << bit, half_period));
            packed.extend(std::iter::repeat_n(0u8, half_period));
        }
        packed
    }

    #[test]
    fn known_clock_reads_exact_frequency_duty_and_edge_count() {
        let rate_hz = 1_000_000u64;
        let half = 500; // 500 samples high, 500 low => 1 kHz, 50% duty
        let cycles = 20;
        let packed = clock_packed(half, cycles, 0);
        let capture = digital_capture(packed, rate_hz);

        let results = measure(
            &capture,
            0,
            &[
                MeasurementKind::Frequency,
                MeasurementKind::Duty,
                MeasurementKind::EdgeCount,
            ],
        )
        .unwrap();

        let MeasurementResult::Ok {
            value: freq, unit, ..
        } = &results[0]
        else {
            panic!("expected Ok frequency, got {:?}", results[0]);
        };
        assert_eq!(unit, "Hz");
        assert!((freq - 1_000.0).abs() < 0.5, "frequency {freq}");

        let MeasurementResult::Ok { value: duty, .. } = &results[1] else {
            panic!("expected Ok duty, got {:?}", results[1]);
        };
        assert!((duty - 50.0).abs() < 0.5, "duty {duty}");

        let MeasurementResult::Ok {
            value: edge_count, ..
        } = &results[2]
        else {
            panic!("expected Ok edge_count, got {:?}", results[2]);
        };
        // 20 cycles => 40 edges (rising+falling), minus possibly the final
        // incomplete edge boundary effects are not expected here since the
        // capture ends exactly on a low-to-high period boundary.
        assert_eq!(*edge_count, 39.0, "edge count");
    }

    #[test]
    fn a_neighbouring_channel_toggling_differently_does_not_leak_in() {
        let rate_hz = 1_000_000u64;
        let half = 500;
        let cycles = 10;
        // Channel 0: a 1 kHz clock. Channel 1: toggles four times as fast.
        let mut packed = Vec::new();
        for i in 0..(half * 2 * cycles) {
            let bit0 = (i / half) % 2 == 0;
            let bit1 = (i / (half / 4)) % 2 == 0;
            packed.push(u8::from(bit0) | (u8::from(bit1) << 1));
        }
        let capture = digital_capture(packed, rate_hz);

        let ch0 = measure(&capture, 0, &[MeasurementKind::Frequency]).unwrap();
        let MeasurementResult::Ok { value: f0, .. } = &ch0[0] else {
            panic!("expected Ok, got {:?}", ch0[0]);
        };
        assert!((f0 - 1_000.0).abs() < 1.0, "channel 0 frequency {f0}");

        let ch1 = measure(&capture, 1, &[MeasurementKind::Frequency]).unwrap();
        let MeasurementResult::Ok { value: f1, .. } = &ch1[0] else {
            panic!("expected Ok, got {:?}", ch1[0]);
        };
        assert!((f1 - 4_000.0).abs() < 5.0, "channel 1 frequency {f1}");
    }

    #[test]
    fn a_channel_that_never_toggles_reports_zero_edges_but_no_frequency() {
        let capture = digital_capture(vec![0u8; 1000], 1_000_000);
        let results = measure(
            &capture,
            0,
            &[MeasurementKind::EdgeCount, MeasurementKind::Frequency],
        )
        .unwrap();
        assert_eq!(
            results[0],
            MeasurementResult::Ok {
                kind: MeasurementKind::EdgeCount,
                value: 0.0,
                unit: "count".into(),
                note: None,
            }
        );
        assert!(matches!(results[1], MeasurementResult::Unavailable { .. }));
    }

    /// B-39: `bursts` bursts of a clock (`half` samples high, `half` low,
    /// `cycles` periods each) separated by `idle` idle samples.
    fn bursty_packed(half: usize, cycles: usize, bursts: usize, idle: usize) -> Vec<u8> {
        let mut packed = Vec::new();
        for _ in 0..bursts {
            packed.extend(clock_packed(half, cycles, 0));
            packed.extend(std::iter::repeat_n(0u8, idle));
        }
        packed
    }

    #[test]
    fn a_bursty_line_reports_the_clock_inside_its_bursts_and_says_so() {
        // 10 MS/s: 12 high + 15 low = 27 samples = 370 kHz clock; 200 clocks
        // per burst, 20 ms idle between bursts. The old whole-capture mean
        // read ~tens of kHz and a ~tens-of-us pulse.
        let rate_hz = 10_000_000u64;
        let mut packed = Vec::new();
        for _ in 0..3 {
            for _ in 0..200 {
                packed.extend(std::iter::repeat_n(1u8, 12));
                packed.extend(std::iter::repeat_n(0u8, 15));
            }
            packed.extend(std::iter::repeat_n(0u8, 200_000));
        }
        let capture = digital_capture(packed, rate_hz);
        let results = measure(
            &capture,
            0,
            &[
                MeasurementKind::Frequency,
                MeasurementKind::Period,
                MeasurementKind::Duty,
                MeasurementKind::PulseWidthHigh,
                MeasurementKind::EdgeCount,
            ],
        )
        .unwrap();
        let get = |i: usize| match &results[i] {
            MeasurementResult::Ok {
                value, unit, note, ..
            } => (*value, unit.clone(), note.clone()),
            other => panic!("expected Ok, got {other:?}"),
        };
        let (freq, _, note) = get(0);
        assert!((freq - 370_370.0).abs() < 500.0, "frequency {freq}");
        assert!(
            note.as_deref().is_some_and(|n| n.contains("3 bursts")),
            "must say it describes the bursts: {note:?}"
        );
        let (period, unit, _) = get(1);
        assert_eq!(unit, "s");
        assert!((period - 2.7e-6).abs() < 1e-8, "period {period}");
        let (duty, _, _) = get(2);
        assert!((duty - 44.4).abs() < 0.5, "duty {duty}");
        let (high, _, _) = get(3);
        assert!((high - 1.2e-6).abs() < 1e-8, "pulse_width_high {high}");
        // Edge count is not a clock property: untouched, no note.
        assert!(get(4).2.is_none());
    }

    #[test]
    fn a_steady_clock_gets_no_burst_note() {
        let capture = digital_capture(clock_packed(500, 20, 0), 1_000_000);
        let results = measure(&capture, 0, &[MeasurementKind::Frequency]).unwrap();
        assert!(matches!(
            &results[0],
            MeasurementResult::Ok { note: None, .. }
        ));
        // A clock slower than the burst gap is not bursts either.
        let slow = digital_capture(bursty_packed(1_000, 1, 3, 0), 1_000_000);
        let results = measure(&slow, 0, &[MeasurementKind::Frequency]).unwrap();
        assert!(matches!(
            &results[0],
            MeasurementResult::Ok { note: None, .. }
        ));
    }

    #[test]
    fn analog_only_kinds_on_digital_are_unavailable() {
        let capture = digital_capture(vec![0, 1], 1_000);
        let results = measure(&capture, 0, &[MeasurementKind::Vpp]).unwrap();
        assert!(matches!(results[0], MeasurementResult::Unavailable { .. }));
    }
}
