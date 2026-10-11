//! `render` against real [`Capture`]s, which is the only level at which the
//! channel addressing, the windowing and the two modes meet.

use awic_core::capture_store::{Capture, CaptureData, CaptureMeta};
use awic_core::{CaptureId, DeviceId};
use awic_proto::frame::Encoding;
use awic_render::timing::IntervalKind;
use awic_render::{render, ViewMode, ViewRequest};

fn meta(channels: Vec<u8>, rate_hz: u64, encoding: Encoding) -> CaptureMeta {
    CaptureMeta {
        device: DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e),
        rate_hz,
        channel_count: channels.len() as u8,
        channels,
        encoding,
        t0_ns: 0,
        trigger_position: None,
    }
}

/// A 1 kHz square wave on CH1 at 1 MS/s, 8-bit, 39.2 mV per code — the bench's
/// own CAL output as the oscilloscope digitises it.
fn analog_cal(cycles: usize) -> Capture {
    let period = 1000usize;
    let raw: Vec<u8> = (0..period * cycles)
        .map(|i| if i % period < period / 2 { 10 } else { 90 })
        .collect();
    Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 1_000_000, Encoding::U8Interleaved),
        data: CaptureData::Analog {
            raw,
            sample_bits: 8,
            channel_scale: vec![0.0392157],
            channel_offset: vec![0.0],
        },
    }
}

/// Two digital channels: D0 a 100 kHz clock, D1 half its rate.
fn digital_clocks(samples: usize) -> Capture {
    let packed: Vec<u8> = (0..samples)
        .map(|i| {
            let d0 = u8::from((i / 5) % 2 == 1);
            let d1 = u8::from((i / 10) % 2 == 1);
            d0 | (d1 << 1)
        })
        .collect();
    Capture {
        id: CaptureId::new(),
        meta: meta(vec![0, 1], 1_000_000, Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    }
}

fn metric<'a>(view: &'a awic_render::View, name: &str) -> &'a awic_render::Metric {
    view.metrics
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no metric {name}; have {:?}",
                view.metrics.iter().map(|m| &m.name).collect::<Vec<_>>()
            )
        })
}

#[test]
fn a_waveform_view_of_every_channel_is_a_well_formed_document() {
    let capture = digital_clocks(20_000);
    let view = render(&capture, &ViewRequest::default()).expect("should render");
    assert_eq!(view.mode, "waveform");
    assert_eq!(view.channels, vec![0, 1]);
    assert!(view.svg.starts_with("<svg xmlns="), "{}", &view.svg[..40]);
    assert!(view.svg.ends_with("</svg>"));
    // Both lanes are labelled with the names the rest of AWIC uses.
    assert!(view.svg.contains(">D0<"), "D0 lane label missing");
    assert!(view.svg.contains(">D1<"), "D1 lane label missing");
    // 20 ms of samples at 1 MS/s.
    let window = metric(&view, "window");
    assert!((window.value.unwrap() - 0.02).abs() < 1e-9, "{window:?}");
    assert_eq!(window.text, "20.0 ms");
}

#[test]
fn a_window_selects_a_slice_and_says_how_long_it_is() {
    let capture = digital_clocks(100_000);
    let view = render(
        &capture,
        &ViewRequest {
            t0_s: Some(0.010),
            t1_s: Some(0.015),
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    assert_eq!(view.channels, vec![0]);
    let window = metric(&view, "window");
    assert!((window.value.unwrap() - 0.005).abs() < 1e-6, "{window:?}");
    assert!(!view.svg.contains(">D1<"), "only D0 was asked for");
}

/// Decimation must be *announced*. A reader who does not know that one column
/// stands for a thousand samples will read the picture as if every edge in it
/// were a real edge.
#[test]
fn a_decimated_trace_says_so_in_its_notes() {
    let capture = digital_clocks(500_000);
    let view = render(&capture, &ViewRequest::default()).expect("should render");
    assert!(
        view.notes.iter().any(|n| n.contains("min/max columns")),
        "{:?}",
        view.notes
    );
}

#[test]
fn an_eye_view_of_the_cal_output_measures_its_period_and_levels() {
    let capture = analog_cal(40);
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Eye,
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    assert_eq!(view.mode, "eye");
    assert_eq!(metric(&view, "frequency").text, "1.00 kHz");
    assert_eq!(
        metric(&view, "unit_interval_source").text,
        "recovered from the signal"
    );
    // 10 and 90 codes at 39.2 mV: 392 mV and 3.53 V.
    let low = metric(&view, "level_low").value.unwrap();
    let high = metric(&view, "level_high").value.unwrap();
    assert!((low - 0.392157).abs() < 1e-3, "level_low {low}");
    assert!((high - 3.529413).abs() < 1e-3, "level_high {high}");
    // A clean square wave folds to a persistence view, not a data eye.
    assert!(
        metric(&view, "kind").text.starts_with("persistence"),
        "{}",
        metric(&view, "kind").text
    );
    let height = metric(&view, "eye_height").value.unwrap();
    assert!((height - (high - low)).abs() < 0.2, "eye_height {height}");
    // 50 % duty, within the resolution of the fold's columns.
    let duty = metric(&view, "duty").value.unwrap();
    assert!((45.0..55.0).contains(&duty), "duty {duty}");
}

/// Everything drawn is also in `<desc>`, so a reader with images switched off
/// still gets the reading.
#[test]
fn the_description_repeats_every_metric_in_words() {
    let capture = analog_cal(40);
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Eye,
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    let desc_start = view.svg.find("<desc>").expect("a desc element") + 6;
    let desc_end = view.svg.find("</desc>").expect("a closing desc");
    let desc = &view.svg[desc_start..desc_end];
    for m in &view.metrics {
        assert!(
            desc.contains(&m.name),
            "desc omits {}; desc was {desc}",
            m.name
        );
    }
}

/// A logic line's eye is a timing measurement, not a signal-quality one, and
/// the result has to say so: `eye_height = 1.00` on a one-bit channel is
/// arithmetic, not a reading.
#[test]
fn a_digital_eye_says_its_vertical_opening_is_full_by_construction() {
    let capture = digital_clocks(200_000);
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Eye,
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    assert!(
        view.notes
            .iter()
            .any(|n| n.contains("full by construction")),
        "{:?}",
        view.notes
    );
    // And the note reaches the document, not just the structured result.
    assert!(view.svg.contains("full by construction"));
}

/// A line whose pulses fall into two widths — the case the histogram exists
/// for. `97 % at one width, 3 % at another` is a diagnosis; a picture of
/// 4000 edges is not.
#[test]
fn a_histogram_names_the_populations_and_their_shares() {
    // D0 toggles every 5 samples except every tenth high pulse, which is half
    // as long. Built directly rather than through the fixture so the widths
    // are exactly known.
    let mut packed = Vec::new();
    for i in 0..200 {
        let high = if i % 10 == 9 { 3 } else { 6 };
        packed.extend(std::iter::repeat_n(1u8, high));
        packed.extend(std::iter::repeat_n(0u8, 6));
    }
    let capture = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 1_000_000, Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    };
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Histogram,
            channels: vec![0],
            interval_kind: Some(IntervalKind::High),
            ..Default::default()
        },
    )
    .expect("should render");
    assert_eq!(view.mode, "histogram");
    assert_eq!(metric(&view, "intervals_measured").text, "high");
    assert_eq!(metric(&view, "populations").text, "2");
    // 6 us dominant, 3 us the minority.
    let first = metric(&view, "population_1_length").value.unwrap();
    let second = metric(&view, "population_2_length").value.unwrap();
    assert!((first - 6e-6).abs() < 1e-9, "population 1 at {first}");
    assert!((second - 3e-6).abs() < 1e-9, "population 2 at {second}");
    let share = metric(&view, "population_1_share").value.unwrap();
    assert!(share > 85.0, "dominant share {share}");
    // Each population is also a finding, spelled out.
    assert_eq!(view.findings.len(), 2, "{:?}", view.findings);
    assert!(
        view.findings[0].detail.contains("6.00 us"),
        "{:?}",
        view.findings[0]
    );
    // And the dominant rate is stated, so a caller need not invert the period.
    let rate = metric(&view, "dominant_rate").value.unwrap();
    assert!((rate - 1.0 / 6e-6).abs() < 1.0, "dominant rate {rate}");
}

/// A steady signal must come back with nothing to report, and say so — an
/// empty findings list that does not explain itself reads like a failure.
#[test]
fn an_anomaly_digest_of_a_steady_signal_finds_nothing_and_says_so() {
    let capture = digital_clocks(200_000);
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Anomalies,
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    assert_eq!(view.mode, "anomalies");
    assert_eq!(metric(&view, "outliers").text, "0");
    assert!(view.findings.is_empty());
    assert!(
        view.notes.iter().any(|n| n.contains("where it should be")),
        "{:?}",
        view.notes
    );
}

/// The digest's whole point: a long capture with a few bad periods comes back
/// as timestamps, not as a picture to squint at.
#[test]
fn an_anomaly_digest_names_where_the_bad_periods_are() {
    // 400 periods of 10 samples, with three stretched to 20 at known places.
    let stretched = [50usize, 200, 350];
    let mut packed = Vec::new();
    let mut expected_times = Vec::new();
    for i in 0..400 {
        if stretched.contains(&i) {
            expected_times.push(packed.len() as f64 / 1e6);
            packed.extend(std::iter::repeat_n(1u8, 10));
            packed.extend(std::iter::repeat_n(0u8, 10));
        } else {
            packed.extend(std::iter::repeat_n(1u8, 5));
            packed.extend(std::iter::repeat_n(0u8, 5));
        }
    }
    let capture = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 1_000_000, Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    };
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Anomalies,
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    let outliers = metric(&view, "outliers").value.unwrap();
    assert_eq!(outliers, 3.0, "{:?}", view.metrics);
    let reference = metric(&view, "reference").value.unwrap();
    assert!((reference - 10e-6).abs() < 1e-9, "reference {reference}");
    // Each one is located, and the times are the ones the signal was built with.
    assert_eq!(view.findings.len(), 3, "{:?}", view.findings);
    for finding in &view.findings {
        let at = finding.at_s.expect("a located finding");
        assert!(
            expected_times.iter().any(|&t| (t - at).abs() < 2e-5),
            "{at} is not near any of {expected_times:?}"
        );
        assert!(finding.detail.contains('%'), "{:?}", finding);
    }
    // The worst deviation is +100 %: 20 samples where 10 were expected.
    let worst = metric(&view, "worst_deviation").value.unwrap();
    assert!((worst - 100.0).abs() < 1.0, "worst deviation {worst}");
}

#[test]
fn a_tolerance_that_is_wide_enough_hides_the_outliers() {
    let capture = digital_clocks(100_000);
    for tolerance in [0.001, 0.5] {
        let view = render(
            &capture,
            &ViewRequest {
                mode: ViewMode::Anomalies,
                channels: vec![0],
                tolerance: Some(tolerance),
                ..Default::default()
            },
        )
        .expect("should render");
        assert_eq!(
            metric(&view, "tolerance").value.unwrap(),
            tolerance * 100.0,
            "the tolerance is reported as given"
        );
    }
}

#[test]
fn the_histogram_and_digest_refuse_more_than_one_channel() {
    let capture = digital_clocks(20_000);
    for mode in [ViewMode::Histogram, ViewMode::Anomalies] {
        let err = render(
            &capture,
            &ViewRequest {
                mode,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains(mode.as_str()), "{err}");
        assert!(err.contains("one channel"), "{err}");
    }
}

#[test]
fn the_eye_view_refuses_more_than_one_channel_by_name() {
    let capture = digital_clocks(20_000);
    let err = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Eye,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("folds one channel"), "{err}");
    assert!(
        err.contains("channels"),
        "the error names the way out: {err}"
    );
}

#[test]
fn a_channel_the_capture_does_not_hold_is_refused_with_what_it_does_hold() {
    let capture = digital_clocks(1_000);
    let err = render(
        &capture,
        &ViewRequest {
            channels: vec![5],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("channel 5"), "{err}");
    // The valid channels, by name and by the number to pass.
    assert!(err.contains("D0 (0), D1 (1)"), "{err}");
}

/// B-33: a logic analyzer capture of only D4 and D6, D4 a clock and D6 high, with D0
/// and D1 — bits 0 and 1 of the packed byte — constant low. `render` takes
/// the hardware numbers every other tool takes and draws those bits, not
/// positions 0 and 1 of the channel list.
fn digital_sparse(samples: usize) -> Capture {
    let packed: Vec<u8> = (0..samples)
        .map(|i| (u8::from((i / 5) % 2 == 1) << 4) | (1 << 6))
        .collect();
    Capture {
        id: CaptureId::new(),
        meta: meta(vec![4, 6], 1_000_000, Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    }
}

#[test]
fn a_sparse_digital_capture_is_addressed_by_hardware_channel() {
    let capture = digital_sparse(10_000);

    let all = render(&capture, &ViewRequest::default()).expect("should render");
    assert_eq!(all.channels, vec![4, 6]);
    assert!(all.svg.contains(">D4<") && all.svg.contains(">D6<"));

    // D4 is the 100 kHz clock — the bit, not position 0 of [4, 6].
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Eye,
            channels: vec![4],
            ..Default::default()
        },
    )
    .expect("D4 should render");
    assert_eq!(view.channels, vec![4]);
    assert_eq!(metric(&view, "frequency").text, "100.0 kHz");
    // B-49: notes are prose, not source-code indentation.
    assert!(!view.notes.is_empty());
    for note in &view.notes {
        assert!(!note.contains("  "), "a run of spaces in {note:?}");
    }

    // Positions are no longer accepted: D0 was not acquired.
    let err = render(
        &capture,
        &ViewRequest {
            channels: vec![0],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("D4 (4), D6 (6)"), "{err}");
}

/// The analog half of B-33: a CH2-only acquisition is drawn as channel 1.
#[test]
fn an_analog_capture_of_ch2_alone_is_addressed_as_channel_1() {
    let mut capture = analog_cal(10);
    capture.meta.channels = vec![1];
    let view = render(
        &capture,
        &ViewRequest {
            channels: vec![1],
            ..Default::default()
        },
    )
    .expect("CH2 should render");
    assert_eq!(view.channels, vec![1]);
    assert!(view.svg.contains(">CH2<"), "lane labelled CH2");
    let err = render(
        &capture,
        &ViewRequest {
            channels: vec![0],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("CH2 (1)"), "{err}");
}

#[test]
fn sizes_are_clamped_rather_than_trusted() {
    let capture = digital_clocks(5_000);
    let view = render(
        &capture,
        &ViewRequest {
            width: 1,
            height: 100_000,
            ..Default::default()
        },
    )
    .expect("should render");
    assert_eq!((view.width, view.height), (240, 4000));
    assert!(
        view.svg.contains(r#"width="240""#),
        "clamped width in the svg"
    );
}

/// A digital channel that never toggles has no eye. The picture must not be an
/// empty box: the caller needs to be told why.
#[test]
fn a_constant_channel_is_refused_with_a_reason() {
    let packed = vec![0u8; 10_000];
    let capture = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 1_000_000, Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    };
    let err = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Eye,
            channels: vec![0],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("constant"), "{err}");
}

/// B-105: a scope probe on no signal reads the converter's noise, a few codes
/// wide. The eye "recovered" an 889 kHz clock from it and called it data. It
/// is refused on the same floor `measure` refuses timing on; a small square
/// a few codes above that floor still folds.
#[test]
fn an_eye_on_converter_noise_is_refused_by_its_codes() {
    let analog = |raw: Vec<u8>| Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 8_000_000, Encoding::U8Interleaved),
        data: CaptureData::Analog {
            raw,
            sample_bits: 8,
            channel_scale: vec![0.0392157],
            channel_offset: vec![-5.0],
        },
    };
    let eye = ViewRequest {
        mode: ViewMode::Eye,
        channels: vec![0],
        ..Default::default()
    };
    // Codes 197..=200 (2.73-2.84 V) in a scrambled order, as on the bench.
    let noise: Vec<u8> = (0..200_000u32)
        .map(|i| 197 + (i.wrapping_mul(2_654_435_761) >> 30) as u8)
        .collect();
    let capture = analog(noise);
    let err = render(&capture, &eye).unwrap_err();
    assert!(err.starts_with("CH1 cannot be folded into an eye"), "{err}");
    assert!(err.contains("ADC codes"), "{err}");
    let err = awic_render::eye_metrics(&capture, 0, None).unwrap_err();
    assert!(err.contains("ADC codes"), "{err}");

    let square: Vec<u8> = (0..200_000)
        .map(|i| if i % 80 < 40 { 197 } else { 203 })
        .collect();
    render(&analog(square), &eye).expect("a 6-code square still folds");
}

/// B-45: a 2 ms capture of a 1 kHz signal was refused with "only 0 whole
/// period(s) fit", counting two-period windows without saying so and without a
/// way forward. The refusal now names the span, the period and the length that
/// does render.
#[test]
fn a_too_short_eye_names_span_period_and_the_length_that_works() {
    let request = ViewRequest {
        mode: ViewMode::Eye,
        channels: vec![0],
        ..Default::default()
    };
    let err = render(&analog_cal(2), &request).expect_err("two periods are too few");
    assert!(!err.contains("whole period(s)"), "{err}");
    assert!(err.contains("2.00 ms"), "names the span: {err}");
    assert!(
        err.contains("2.0 periods of 1.00 ms"),
        "names the period: {err}"
    );
    assert!(
        err.contains("capture at least 6.00 ms"),
        "names the fix: {err}"
    );
    // And the length it asks for really does render.
    render(&analog_cal(6), &request).expect("six periods are enough");
}

/// B-42: the anomalies plot of a long capture stays a bounded document while
/// every number it reports is unchanged.
#[test]
fn an_anomaly_view_of_twenty_thousand_periods_stays_small() {
    let mut packed = Vec::new();
    for i in 0..21_000usize {
        // Every 97th period stretched, a scatter of mild jitter on the rest.
        let high = if i % 97 == 50 {
            10
        } else {
            5 + usize::from(i % 7 == 0)
        };
        packed.extend(std::iter::repeat_n(1u8, high));
        packed.extend(std::iter::repeat_n(0u8, 5));
    }
    let capture = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 1_000_000, Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    };
    let view = render(
        &capture,
        &ViewRequest {
            mode: ViewMode::Anomalies,
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    assert!(
        view.svg.len() < 60_000,
        "{} bytes for 21 000 periods (542 KB before B-42)",
        view.svg.len()
    );
    // The numbers and findings are the ones the unthinned plot reported.
    assert_eq!(metric(&view, "intervals").value, Some(20998.0));
    assert_eq!(metric(&view, "outliers").value, Some(3184.0));
    assert_eq!(metric(&view, "outlier_share").text, "15.2 %");
    assert_eq!(metric(&view, "worst_deviation").text, "47.9 %");
    assert_eq!(view.findings.len(), 12);
    assert_eq!(view.findings[0].label, "period at 508 us");
    assert_eq!(
        view.findings[0].detail,
        "15.0 us — +47.9 % of the 10.1 us reference"
    );
    assert_eq!(view.findings[11].label, "period at 11.4 ms");
}

/// B-36: `t0_s` alone runs to the end of the capture. Grok read the long
/// result as a dropped `t1_s`; the note says what the window actually is.
#[test]
fn an_open_ended_window_says_it_runs_to_the_end() {
    let capture = digital_clocks(100_000);
    let view = render(
        &capture,
        &ViewRequest {
            t0_s: Some(0.010),
            channels: vec![0],
            ..Default::default()
        },
    )
    .expect("should render");
    assert!(
        view.notes
            .iter()
            .any(|n| n.contains("to the end of the capture")
                && n.contains("90.0 ms")
                && n.contains("t1_s")),
        "{:?}",
        view.notes
    );
    // With both ends given there is nothing to explain.
    let bounded = render(
        &capture,
        &ViewRequest {
            t0_s: Some(0.010),
            t1_s: Some(0.015),
            channels: vec![0],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!bounded.notes.iter().any(|n| n.contains("to the end")));
}

/// Every stroke in a waveform path, as `(y_top, y_bottom)` pixel pairs in
/// drawing order. The trace is one path of `M x y1 V y2` segments.
fn strokes(svg: &str, nth_path: usize) -> Vec<(f64, f64)> {
    let d = svg
        .match_indices(" d=\"")
        .nth(nth_path)
        .map(|(i, m)| {
            let rest = &svg[i + m.len()..];
            &rest[..rest.find('"').expect("unterminated d attribute")]
        })
        .expect("no such path in the document");
    d.split('M')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let (head, tail) = seg.split_once('V').expect("stroke without a V");
            let y1: f64 = head
                .split_whitespace()
                .nth(1)
                .expect("stroke without a y")
                .parse()
                .expect("y1 not a number");
            (y1, tail.trim().parse().expect("y2 not a number"))
        })
        .collect()
}

/// B-52: with few samples per column an edge lands on a column boundary, and
/// drawing each column's own min/max left the transition unstroked — the trace
/// became disconnected ticks at two levels. Consecutive columns must touch.
#[test]
fn a_waveform_leaves_no_gap_between_columns() {
    // 1600 samples over ~820 columns: under two samples per column, and D0
    // toggles every other sample so most edges fall on a boundary.
    let packed: Vec<u8> = (0..1600).map(|i| u8::from((i / 2) % 2 == 1)).collect();
    let capture = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 1_000_000, Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    };
    let view = render(&capture, &ViewRequest::default()).expect("should render");
    let drawn = strokes(&view.svg, 0);
    assert!(
        drawn.len() > 400,
        "expected a stroke per column, got {}",
        drawn.len()
    );

    // The lane is tens of pixels tall; a stroke that spans it is an edge.
    let span = drawn.iter().map(|(y1, y2)| y2 - y1).fold(0.0f64, f64::max);
    assert!(
        span > 10.0,
        "no edge spans the lane: tallest stroke {span} px"
    );
    let edges = drawn.iter().filter(|(y1, y2)| y2 - y1 > span / 2.0).count();
    assert!(
        edges > 200,
        "only {edges} of {} strokes carry an edge",
        drawn.len()
    );

    // The property that was broken: no vertical gap anywhere in the trace.
    for (i, pair) in drawn.windows(2).enumerate() {
        let ((a_top, a_bot), (b_top, b_bot)) = (pair[0], pair[1]);
        assert!(
            a_bot + 1.0 >= b_top && b_bot + 1.0 >= a_top,
            "gap between column {i} ({a_top}..{a_bot}) and {} ({b_top}..{b_bot})",
            i + 1
        );
    }
}

/// B-54: a capture pinned to a converter rail is drawn as a line welded to the
/// edge of its lane, and nothing in the picture says why. The view says so.
#[test]
fn a_clipped_channel_is_named_in_the_views_findings() {
    let capture = Capture {
        id: CaptureId::new(),
        // The 2.5 V range: 255 codes over -2.5..+2.5 V, every sample on the
        // top code, which is what the bench returned for a 2.7-4.1 V bus.
        meta: meta(vec![0], 8_000_000, Encoding::U8Interleaved),
        data: CaptureData::Analog {
            raw: vec![255; 8000],
            sample_bits: 8,
            channel_scale: vec![5.0 / 255.0],
            channel_offset: vec![-2.5],
        },
    };
    let view = render(&capture, &ViewRequest::default()).expect("should render");
    let finding = view
        .findings
        .iter()
        .find(|f| f.label.contains("clipped"))
        .unwrap_or_else(|| panic!("no clipping finding; have {:?}", view.findings));
    assert_eq!(finding.label, "CH1 clipped");
    assert!(
        finding.detail.contains("every sample"),
        "{}",
        finding.detail
    );
    assert!(finding.detail.contains("+2.50 V"), "{}", finding.detail);
    assert!(
        finding.detail.contains("larger range"),
        "must say what to do: {}",
        finding.detail
    );
    assert!(
        view.notes.iter().any(|n| n.starts_with("CH1: clipped")),
        "the note belongs with the picture too: {:?}",
        view.notes
    );

    // A signal inside the range says nothing of the kind.
    let clean = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], 8_000_000, Encoding::U8Interleaved),
        data: CaptureData::Analog {
            raw: (0..8000).map(|i| 40 + (i % 160) as u8).collect(),
            sample_bits: 8,
            channel_scale: vec![5.0 / 255.0],
            channel_offset: vec![-2.5],
        },
    };
    let view = render(&clean, &ViewRequest::default()).expect("should render");
    assert!(
        !view.findings.iter().any(|f| f.label.contains("clipped")),
        "{:?}",
        view.findings
    );
}

/// B-53: a narrow window late in a capture labelled every tick "399 ms", and
/// the lane's name was drawn on top of its own top value label — "3.1CH1".
#[test]
fn a_narrow_window_late_in_a_capture_gets_distinguishable_tick_labels() {
    // 1 MS/s, 500 ms long, with a 320 µs window starting at 398.78 ms: the
    // window from the report (its capture was 4 MS/s; what matters is a short
    // window a long way from zero).
    let capture = analog_cal(500);
    let view = render(
        &capture,
        &ViewRequest {
            channels: vec![0],
            t0_s: Some(0.39878),
            t1_s: Some(0.39910),
            ..ViewRequest::default()
        },
    )
    .expect("should render");

    // The tick labels are the <text> nodes ending in a time unit; they must not
    // all be the same string.
    let labels: Vec<&str> = view
        .svg
        .match_indices("</text>")
        .filter_map(|(end, _)| {
            let start = view.svg[..end].rfind('>')? + 1;
            let text = &view.svg[start..end];
            (text.ends_with(" ms") || text.ends_with(" us")).then_some(text)
        })
        .collect();
    assert!(labels.len() >= 4, "too few time labels: {labels:?}");
    let mut unique: Vec<&str> = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        labels.len(),
        "the same label on every tick: {labels:?}"
    );

    // The lane name must not be drawn where the value labels are, which is how
    // "3.17 V" under "CH1" came out as "3.1CH1".
    let name = text_node(&view.svg, "CH1").expect("the lane is labelled CH1");
    let values: Vec<TextNode> = view
        .svg
        .match_indices("</text>")
        .filter_map(|(end, _)| {
            let start = view.svg[..end].rfind('>')? + 1;
            let content = &view.svg[start..end];
            content
                .ends_with(" V")
                .then(|| text_node(&view.svg, content))?
        })
        .collect();
    assert!(!values.is_empty(), "no value labels to collide with");
    for value in &values {
        assert!(
            (value.x - name.x).abs() > 1.0 || (value.y - name.y).abs() > 8.0,
            "the lane name at ({}, {}) sits on the value label at ({}, {})",
            name.x,
            name.y,
            value.x,
            value.y
        );
    }
}

struct TextNode {
    x: f64,
    y: f64,
}

/// The first `<text>` node whose content is exactly `content`, with its
/// position — enough to tell whether two labels are drawn on top of each other.
fn text_node(svg: &str, content: &str) -> Option<TextNode> {
    let needle = format!(">{content}</text>");
    let at = svg.find(&needle)?;
    let open = svg[..at].rfind("<text ")?;
    let attr = |name: &str| -> Option<f64> {
        let key = format!("{name}=\"");
        let from = svg[open..at].find(&key)? + open + key.len();
        let to = from + svg[from..].find('"')?;
        svg[from..to].parse().ok()
    };
    Some(TextNode {
        x: attr("x")?,
        y: attr("y")?,
    })
}
