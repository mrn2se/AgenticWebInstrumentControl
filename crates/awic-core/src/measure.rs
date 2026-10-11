//! Server-side measurements (SPECIFICATION.md §5.4; DEVELOPMENT.md §1 rule
//! 4: measurements run on full-resolution data, server-side, never on a
//! decimated preview).
//!
//! This module reads [`Capture`] directly — the same full-resolution
//! `CaptureData` the store holds, never anything [`crate::CaptureStore::read_capture`]
//! produces — and returns a typed [`MeasurementResult`] per requested
//! [`MeasurementKind`]: a value with its unit, or an honest "not measurable"
//! with a reason. **No kind ever returns `NaN`, `0`, or a guessed value in
//! place of a reason it could not be computed** — a plausible wrong number is
//! exactly the failure this module exists to prevent.
//!
//! # Channel addressing
//!
//! `channel` addresses the capture **as the store lays it out**, which is not
//! always the original hardware channel number the acquisition requested —
//! see the doc comment on [`measure_capture`] for why, and the note in this
//! crate's top-level docs / the implementation report for what that cost.
//!
//! # Definitions
//!
//! - **Vmin/Vmax/Vpp/Vavg**: plain extrema and arithmetic mean of the
//!   channel's voltages (`raw * scale + offset`, never derived from bit
//!   depth).
//! - **Vrms**: true RMS, `sqrt(mean(v^2))`, which **includes** any DC
//!   component — a waveform offset from 0 V has a higher Vrms than its AC
//!   component alone, exactly as a real meter/scope reports it.
//! - **Frequency/period**: detected via hysteresis edge detection (below),
//!   then the *mean of complete periods between like edges* — never
//!   first-edge-to-last-edge divided by an edge count, which silently
//!   assumes every gap between edges is exactly one period.
//! - **Duty**: for each pair of consecutive rising edges (one period), the
//!   fraction of that period before the one falling edge inside it,
//!   averaged over every complete period found.
//! - **Rise/fall time**: 10%-90% (or 90%-10%) between the waveform's
//!   *settled* low and high levels, estimated by histogram mode ("top"/
//!   "base") rather than raw min/max — a single spike sample skews min/max
//!   but not the mode of a waveform that spends most of its time at its two
//!   plateaus.
//! - **Hysteresis**: edges are detected with a Schmitt-trigger-style dead
//!   band around the waveform's mid-level (`(top + base) / 2 ±
//!   10% of (top - base)`), so noise near the threshold does not register as
//!   a phantom edge. A signal only counts as having "arrived" at a level once
//!   it clears the far side of the dead band from wherever it last settled.
//! - **Digital** measurements are derived from the capture's transition list
//!   (never by rescanning the packed bytes), extracting just the requested
//!   channel's bit from each transition's bitmask — so an unrelated channel
//!   toggling cannot leak into another channel's measurement.
//!
//! "Not measurable" is reported whenever the necessary structure isn't
//! present: fewer than two like edges (no complete period), a flat signal
//! (no threshold crossings at all), or no complete pulse of the requested
//! polarity.

use awic_proto::{MeasurementKind, MeasurementResult};

use crate::capture_store::{Capture, CaptureData};
use crate::error::{CoreError, Result};

mod analog;
mod digital;
mod edges;

/// `Some(reason)` when analog hardware channel `channel` spans fewer ADC
/// codes in `values` (its samples in volts, over any window) than timing
/// needs: a period, or an eye folded on one, would describe the converter's
/// noise rather than a signal. `None` on a digital capture, which has no
/// codes. The eye refuses on the same ground as `measure` (B-105).
pub fn below_timing_floor(capture: &Capture, channel: u8, values: &[f64]) -> Option<String> {
    let position = crate::series::hardware_channels(capture)
        .iter()
        .position(|&c| c == channel)?;
    let extrema = values.iter().fold(None, |acc: Option<(f64, f64)>, &v| {
        Some(acc.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))))
    });
    analog::within_noise_floor(capture, position as u8, extrema)
}

/// Measures hardware channel `channel` of `capture`, returning one
/// [`MeasurementResult`] per entry of `kinds`, in the same order.
///
/// `channel` is a **hardware** channel number — the one the UI shows and an
/// agent names — and it is translated here, once, using
/// [`CaptureMeta::channels`](crate::capture_store::CaptureMeta::channels):
///
/// - [`CaptureData::Analog`] interleaves the acquired channels in order, so
///   hardware channel `c` is the position of `c` in `meta.channels`. An
///   earlier version treated `channel` as that position directly: measuring
///   hardware channel 1 of an acquisition of [1, 2] silently returned
///   channel 2's data.
/// - [`CaptureData::Digital`] packs hardware channel `n` as bit `n`.
///
/// Returns [`CoreError::ChannelNotFound`] for a channel the capture did not
/// acquire. A kind that does not apply to the capture's type (e.g. `vpp` on a digital
/// capture) is not an error: it comes back as
/// [`MeasurementResult::Unavailable`] like any other "not measurable" case,
/// so one bad kind in a batch does not fail the other, valid ones.
pub fn measure_capture(
    capture: &Capture,
    channel: u8,
    kinds: &[MeasurementKind],
) -> Result<Vec<MeasurementResult>> {
    // `channel` is a HARDWARE channel number, as the UI and agents know it.
    // Translate it here, once, rather than trusting it to be a position.
    let acquired = &capture.meta.channels;
    let Some(position) = acquired.iter().position(|&c| c == channel) else {
        return Err(CoreError::ChannelNotFound {
            capture: capture.id.to_string(),
            channel,
            valid: format!("this capture holds hardware channels {acquired:?}"),
        });
    };
    match &capture.data {
        // Analog samples interleave the acquired channels in order.
        CaptureData::Analog { .. } => analog::measure(capture, position as u8, kinds),
        // Digital bit n is hardware channel n.
        CaptureData::Digital { .. } => digital::measure(capture, channel, kinds),
    }
}

/// Adds the NRZ note to a frequency, period or duty measured on NRZ data
/// (B-103): on a CAN or UART line those are averages over runs of one to
/// several bits and read like a clock that is not there. The value stays;
/// the note says what it is, and gives the bit rate that does mean something.
fn note_nrz(results: &mut [MeasurementResult], edges: &[edges::Edge], rate_hz: f64) {
    use MeasurementKind::{Duty, Frequency, Period};
    let Some(unit) = edges::nrz_unit_samples(edges) else {
        return;
    };
    let unit_s = unit / rate_hz;
    let nrz = format!(
        "NRZ data, not a clock: the runs between edges are whole numbers of a {} unit \
         interval (~{} bit/s), so this is an average over runs of different lengths and \
         describes the data, not a rate the line runs at",
        format_seconds(unit_s),
        format_rate_bits(1.0 / unit_s)
    );
    for r in results.iter_mut() {
        if let MeasurementResult::Ok { kind, note, .. } = r {
            if matches!(kind, Frequency | Period | Duty) {
                *note = Some(match note.take() {
                    Some(old) => format!("{nrz}; {old}"),
                    None => nrz.clone(),
                });
            }
        }
    }
}

/// `2.01 us`, `104 us`, `1.5 ms`.
fn format_seconds(s: f64) -> String {
    if s < 1e-6 {
        format!("{:.0} ns", s * 1e9)
    } else if s < 1e-3 {
        format!("{:.3} us", s * 1e6)
    } else {
        format!("{:.3} ms", s * 1e3)
    }
}

/// `498 k`, `9.6 k`, `1.00 M` — a bit rate to three figures.
fn format_rate_bits(bps: f64) -> String {
    if bps >= 1e6 {
        format!("{:.2} M", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.3} k", bps / 1e3)
    } else {
        format!("{bps:.0} ")
    }
    .trim_end()
    .to_string()
}

/// Builds the [`MeasurementResult::Ok`] variant.
fn ok(kind: MeasurementKind, value: f64, unit: &str) -> MeasurementResult {
    MeasurementResult::Ok {
        kind,
        value,
        unit: unit.to_string(),
        note: None,
    }
}

/// [`ok`] with a qualifier the value must not be read without (B-39/B-40/B-41).
fn ok_noted(
    kind: MeasurementKind,
    value: f64,
    unit: &str,
    note: impl Into<String>,
) -> MeasurementResult {
    MeasurementResult::Ok {
        kind,
        value,
        unit: unit.to_string(),
        note: Some(note.into()),
    }
}

/// Builds the [`MeasurementResult::Unavailable`] variant with a reason.
fn unavailable(kind: MeasurementKind, reason: impl Into<String>) -> MeasurementResult {
    MeasurementResult::Unavailable {
        kind,
        unavailable: reason.into(),
    }
}

/// Builds the "not applicable" result shared by both analog and digital
/// measure functions for a kind that belongs to the other capture type —
/// e.g. `vpp` requested on a digital capture, or `edge_count` on an analog
/// one. Not an error: the rest of a batch's kinds are unaffected.
fn not_applicable(
    kind: MeasurementKind,
    this_capture_is: &str,
    kind_belongs_to: &str,
) -> MeasurementResult {
    // "an analog-only measurement", not "a analog-only": the two callers pass
    // "analog" and "digital", and the article has to follow.
    let article = if kind_belongs_to.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    unavailable(
        kind,
        format!(
            "{kind:?} is {article} {kind_belongs_to}-only measurement; \
             this capture is {this_capture_is}"
        ),
    )
}

fn channel_not_found(capture: &Capture, channel: u8, valid: impl Into<String>) -> CoreError {
    CoreError::ChannelNotFound {
        capture: capture.id.to_string(),
        channel,
        valid: valid.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_store::{CaptureId, CaptureMeta};
    use crate::device_id::DeviceId;
    use awic_proto::frame::Encoding;

    pub(crate) fn dev() -> DeviceId {
        DeviceId::new(253, vec![1], 0xffff, 1)
    }

    pub(crate) fn analog_capture(
        channel_count: u8,
        sample_bits: u8,
        channel_scale: Vec<f32>,
        channel_offset: Vec<f32>,
        raw: Vec<u8>,
        rate_hz: u64,
    ) -> Capture {
        Capture {
            id: CaptureId::new(),
            meta: CaptureMeta {
                channels: (0..channel_count).collect(),
                device: dev(),
                rate_hz,
                channel_count,
                encoding: if sample_bits <= 8 {
                    Encoding::U8Interleaved
                } else {
                    Encoding::I16
                },
                t0_ns: 0,
                trigger_position: None,
            },
            data: CaptureData::Analog {
                raw,
                sample_bits,
                channel_scale,
                channel_offset,
            },
        }
    }

    pub(crate) fn digital_capture(packed: Vec<u8>, rate_hz: u64) -> Capture {
        let transitions = crate::capture_store::derive_transitions(&packed);
        Capture {
            id: CaptureId::new(),
            meta: CaptureMeta {
                // All 8 bits acquired, as fx2lafw does by default.
                channels: (0..8).collect(),
                device: dev(),
                rate_hz,
                channel_count: 1,
                encoding: Encoding::PackedDigital,
                t0_ns: 0,
                trigger_position: None,
            },
            data: CaptureData::Digital {
                packed,
                transitions,
            },
        }
    }

    /// Bit runs like a CAN frame's: 1-5 bits, 8 samples per bit, repeated
    /// without idle gaps, as packed digital bit 0.
    fn nrz_bits(samples_per_bit: usize) -> Vec<u8> {
        let runs = [1, 5, 2, 1, 3, 1, 1, 2, 5, 1, 4, 1, 2, 1, 1, 3];
        let mut packed = Vec::new();
        let mut level = 0u8;
        for _ in 0..20 {
            for &r in &runs {
                packed.extend(std::iter::repeat_n(level, r * samples_per_bit));
                level ^= 1;
            }
        }
        packed
    }

    fn note_of(result: &MeasurementResult) -> Option<&str> {
        match result {
            MeasurementResult::Ok { note, .. } => note.as_deref(),
            other => panic!("expected a value, got {other:?}"),
        }
    }

    /// B-103: on NRZ data, frequency, period and duty are averages over runs
    /// of different lengths. They keep their value but say so, with the bit
    /// rate; amplitude and edge counts are untouched.
    #[test]
    fn frequency_of_nrz_data_says_it_is_no_clock() {
        let digital = digital_capture(nrz_bits(8), 4_000_000);
        let kinds = [
            MeasurementKind::Frequency,
            MeasurementKind::Period,
            MeasurementKind::Duty,
            MeasurementKind::EdgeCount,
        ];
        let r = measure_capture(&digital, 0, &kinds).unwrap();
        for result in &r[..3] {
            let note = note_of(result).expect("a note");
            assert!(note.contains("NRZ data, not a clock"), "{note}");
            assert!(
                note.contains("2.000 us") && note.contains("500.000 k"),
                "{note}"
            );
        }
        assert_eq!(note_of(&r[3]), None, "an edge count is just a count");

        // The same bits on the oscilloscope, 0 V / 3 V.
        let raw: Vec<u8> = nrz_bits(8)
            .iter()
            .map(|&b| if b == 1 { 200 } else { 50 })
            .collect();
        let analog = analog_capture(1, 8, vec![0.015], vec![0.0], raw, 4_000_000);
        let r = measure_capture(&analog, 0, &[MeasurementKind::Frequency]).unwrap();
        let note = note_of(&r[0]).expect("a note");
        assert!(note.contains("NRZ data, not a clock"), "{note}");
    }

    /// A clock, and a clock that drops a pulse now and then, are no NRZ data.
    #[test]
    fn a_clock_gets_no_nrz_note() {
        let mut packed = Vec::new();
        for i in 0..400 {
            let high = if i % 50 == 7 { 0u8 } else { 1u8 };
            packed.extend(std::iter::repeat_n(high, 5));
            packed.extend(std::iter::repeat_n(0u8, 5));
        }
        let r = measure_capture(
            &digital_capture(packed, 1_000_000),
            0,
            &[MeasurementKind::Frequency],
        )
        .unwrap();
        assert_eq!(note_of(&r[0]), None);
    }

    fn value(result: &MeasurementResult) -> f64 {
        match result {
            MeasurementResult::Ok { value, .. } => *value,
            other => panic!("expected a value, got {other:?}"),
        }
    }

    #[test]
    fn channel_means_the_hardware_channel_not_the_position() {
        // Hardware channels [1, 2] acquired and interleaved: ch1 at 1 V, ch2 at 3 V.
        let mut capture = analog_capture(
            2,
            8,
            vec![1.0 / 64.0, 1.0 / 64.0],
            vec![0.0, 0.0],
            [64u8, 192].repeat(100),
            1_000,
        );
        capture.meta.channels = vec![1, 2];
        let ch1 = measure_capture(&capture, 1, &[MeasurementKind::Vavg]).unwrap();
        assert!(
            (value(&ch1[0]) - 1.0).abs() < 1e-6,
            "channel 1 must read channel 1's data, not position 1's (channel 2's 3 V)"
        );
        let ch2 = measure_capture(&capture, 2, &[MeasurementKind::Vavg]).unwrap();
        assert!((value(&ch2[0]) - 3.0).abs() < 1e-6);
        assert!(
            matches!(
                measure_capture(&capture, 0, &[MeasurementKind::Vavg]),
                Err(CoreError::ChannelNotFound { .. })
            ),
            "channel 0 was not acquired"
        );
    }

    #[test]
    fn a_subset_acquisition_is_addressed_by_its_hardware_channel() {
        // Only CH2 (hardware 1) on a two-channel scope: one interleaved lane.
        let mut capture = analog_capture(1, 8, vec![1.0 / 64.0], vec![0.0], vec![128; 100], 1_000);
        capture.meta.channels = vec![1];
        let r = measure_capture(&capture, 1, &[MeasurementKind::Vavg]).unwrap();
        assert!((value(&r[0]) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn a_digital_channel_that_was_not_acquired_is_refused() {
        let mut capture = digital_capture(vec![0, 1, 0, 1], 1_000);
        capture.meta.channels = vec![0];
        assert!(matches!(
            measure_capture(&capture, 3, &[MeasurementKind::EdgeCount]),
            Err(CoreError::ChannelNotFound { .. })
        ));
    }

    #[test]
    fn unknown_analog_channel_is_a_named_error() {
        let capture = analog_capture(1, 8, vec![1.0], vec![0.0], vec![0, 1, 2], 1_000);
        let err = measure_capture(&capture, 5, &[MeasurementKind::Vpp]).unwrap_err();
        assert!(matches!(err, CoreError::ChannelNotFound { channel: 5, .. }));
    }

    #[test]
    fn unknown_digital_channel_is_a_named_error() {
        let capture = digital_capture(vec![0, 1, 0], 1_000);
        let err = measure_capture(&capture, 8, &[MeasurementKind::EdgeCount]).unwrap_err();
        assert!(matches!(err, CoreError::ChannelNotFound { channel: 8, .. }));
    }

    #[test]
    fn digital_only_kind_on_analog_capture_is_unavailable_not_an_error() {
        let capture = analog_capture(1, 8, vec![1.0], vec![0.0], vec![0, 1, 2], 1_000);
        let results = measure_capture(&capture, 0, &[MeasurementKind::EdgeCount]).unwrap();
        assert!(matches!(
            results[0],
            MeasurementResult::Unavailable {
                kind: MeasurementKind::EdgeCount,
                ..
            }
        ));
    }

    #[test]
    fn analog_only_kind_on_digital_capture_is_unavailable_not_an_error() {
        let capture = digital_capture(vec![0, 1, 0], 1_000);
        let results = measure_capture(&capture, 0, &[MeasurementKind::Vrms]).unwrap();
        assert!(matches!(
            results[0],
            MeasurementResult::Unavailable {
                kind: MeasurementKind::Vrms,
                ..
            }
        ));
    }

    #[test]
    fn results_preserve_the_requested_order() {
        let capture = analog_capture(1, 8, vec![1.0], vec![0.0], vec![0, 128, 255], 1_000);
        let kinds = [
            MeasurementKind::Vmax,
            MeasurementKind::Vmin,
            MeasurementKind::Vavg,
        ];
        let results = measure_capture(&capture, 0, &kinds).unwrap();
        let seen_kinds: Vec<MeasurementKind> = results
            .iter()
            .map(|r| match r {
                MeasurementResult::Ok { kind, .. }
                | MeasurementResult::Unavailable { kind, .. } => *kind,
            })
            .collect();
        assert_eq!(seen_kinds, kinds);
    }
}
