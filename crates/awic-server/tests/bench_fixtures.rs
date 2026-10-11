//! Survey, decode, measure and render against captures recorded on the real
//! bench (`testdata/bench/`, see `awic_core::fixtures`).
//!
//! Every other test here feeds synthetic signals, which test what their
//! author thought of. These are the SSD1306 I²C bus and the oscilloscope's CAL
//! output as the hardware delivered them on 2026-10-03 — clock stretching,
//! ACK hand-over spikes on SDA, a zero offset that moves with the range — so
//! a change that only breaks on real signals breaks here first. Counts are
//! asserted exactly (the data is fixed); measured values with the tolerance
//! the number deserves. Re-record with `examples/record_fixture.rs`.

use awic_core::fixtures::load;
use awic_core::{channel_series, measure_capture, Capture};
use awic_proto::{
    ChannelMapping, ChannelRole, DecodeFormat, DecoderId, MeasurementKind as K, MeasurementResult,
    OptionSetting, OptionValue,
};
use awic_render::timing::IntervalKind;
use awic_render::{render, View, ViewMode, ViewRequest};
use awic_server::decode::{decode_capture, decode_capture_range, DecodeSpec};
use awic_server::survey::survey_capture;

/// `(value, note)` of one measurement, or a panic naming why it was
/// unavailable.
fn measure(capture: &Capture, channel: u8, kind: K) -> (f64, Option<String>) {
    match measure_capture(capture, channel, &[kind])
        .unwrap()
        .remove(0)
    {
        MeasurementResult::Ok { value, note, .. } => (value, note),
        other => panic!("{kind:?} on channel {channel}: {other:?}"),
    }
}

fn view(capture: &Capture, mode: ViewMode, channel: u8) -> View {
    render(
        capture,
        &ViewRequest {
            mode,
            channels: vec![channel],
            ..Default::default()
        },
    )
    .unwrap_or_else(|e| panic!("{mode:?} on channel {channel}: {e}"))
}

fn metric<'a>(view: &'a View, name: &str) -> &'a str {
    &view
        .metrics
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("no metric {name}"))
        .text
}

fn close(value: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        (value - expected).abs() <= tolerance,
        "{what}: {value} is not {expected} ± {tolerance}"
    );
}

// --- The SSD1306 I²C bus: SDA on D4, SCL on D6, 4 MS/s, one 29 ms burst.

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_survey_names_the_i2c_bus_and_its_real_clock() {
    let (_, capture) = load("i2c_ssd1306_4msps");
    let survey = survey_capture(&capture, "fixture".into(), None, None).unwrap();
    let guess = serde_json::to_value(&survey.guesses).unwrap();
    assert_eq!(guess[0]["protocol"], "i2c", "{guess}");
    assert_eq!(
        (guess[0]["scl"].as_u64(), guess[0]["sda"].as_u64()),
        (Some(6), Some(4))
    );
    assert_eq!(guess[0]["starts"], 66);
    // From the period, not twice a pulse (B-38): this SCL is 1.25 us high,
    // 1.5 us low, and the old formula read it as 333 kHz.
    close(
        guess[0]["clock_hz"].as_f64().unwrap(),
        363_636.0,
        3_700.0,
        "SCL",
    );
    for c in &survey.channels {
        // One burst between idle stretches is bursty (B-47).
        assert_eq!(serde_json::to_value(c.activity).unwrap(), "bursty", "{c:?}");
    }
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_decode_finds_every_ssd1306_write_acked() {
    let (_, capture) = load("i2c_ssd1306_4msps");
    let channels = [
        ChannelMapping {
            role: ChannelRole::Scl,
            channel: 6,
            minus: None,
        },
        ChannelMapping {
            role: ChannelRole::Sda,
            channel: 4,
            minus: None,
        },
    ];
    let out = decode_capture(
        &capture,
        &DecodeSpec {
            decoder: DecoderId::I2c,
            channels: &channels,
            options: &[],
            max_annotations: Some(1000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            // Frame by frame: these check the decoder, not folding (F-14).
            fold: Some(false),
        },
    )
    .unwrap();
    assert!(!out.truncated);
    let tx = serde_json::to_value(&out.transactions).unwrap();
    let tx = tx.as_array().unwrap();
    assert_eq!(tx.len(), 66);
    // The display's window set-up opens the burst: columns 0..127, pages 0..7.
    assert_eq!(tx[0]["data"], "00 21 00 7F");
    assert_eq!(tx[1]["data"], "00 22 00 07");
    // Then the framebuffer: a 0x40 data control byte and 16 pixel bytes each.
    for t in &tx[2..] {
        assert!(t["data"].as_str().unwrap().starts_with("40 "), "{t}");
        assert_eq!(t["count"], 17, "{t}");
    }
    // The SDA spikes at ACK hand-over are not NACKs (B-43).
    assert!(tx
        .iter()
        .all(|t| t["address"] == 0x3C && t["acked"] == true));
    let summary = serde_json::to_value(out.summary.unwrap()).unwrap();
    assert_eq!(summary["transactions"], 66);
    assert_eq!(summary["nacks"], 0);
    assert_eq!(summary["errors"], 0);
    assert_eq!(summary["by_address"][0]["address"], 0x3C);
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn measure_reports_scl_inside_the_burst_not_averaged_over_idle() {
    let (_, capture) = load("i2c_ssd1306_4msps");
    // Over the whole 60 ms the line is idle about half the time; the mean
    // used to report the gaps (B-39).
    let (f, note) = measure(&capture, 6, K::Frequency);
    close(f, 363_636.0, 3_700.0, "SCL frequency");
    let note = note.expect("a bursty line says what the number describes");
    assert!(note.contains("inside 1 burst"), "{note}");
    close(
        measure(&capture, 6, K::Period).0,
        2.75e-6,
        0.03e-6,
        "SCL period",
    );
    close(
        measure(&capture, 6, K::PulseWidthHigh).0,
        1.25e-6,
        0.13e-6,
        "SCL high",
    );
    close(
        measure(&capture, 6, K::PulseWidthLow).0,
        1.5e-6,
        0.13e-6,
        "SCL low",
    );
    // Edge count is a count over the capture, not a clock property.
    assert_eq!(measure(&capture, 6, K::EdgeCount), (21_048.0, None));
    assert_eq!(measure(&capture, 4, K::EdgeCount), (1_834.0, None));
    // SDA is a data line, and the note does not call it a clock. Its ACK
    // hand-over spikes are 1 sample: never a unit interval (B-103), and SCL
    // is no NRZ data.
    let (_, sda_note) = measure(&capture, 4, K::Frequency);
    let sda_note = sda_note.unwrap();
    assert!(!sda_note.contains("clock"), "{sda_note}");
    assert!(!note.contains("NRZ"), "{note}");
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_views_of_scl_agree_with_each_other() {
    let (_, capture) = load("i2c_ssd1306_4msps");
    let histogram = view(&capture, ViewMode::Histogram, 6);
    assert_eq!(metric(&histogram, "intervals"), "10522");
    assert_eq!(metric(&histogram, "population_1_length"), "2.74 us");
    assert_eq!(metric(&histogram, "population_1_share"), "99.4 %");
    // A logic line's eye never closes; its width is one unit interval at
    // most (B-46), and it says so.
    let eye = view(&capture, ViewMode::Eye, 6);
    assert_eq!(metric(&eye, "unit_interval"), "2.75 us");
    assert_eq!(metric(&eye, "eye_width"), "2.75 us");
    assert!(eye.notes.iter().any(|n| n.contains("never closes")));
    // 10 522 periods stay a small document (B-42).
    let anomalies = view(&capture, ViewMode::Anomalies, 6);
    assert_eq!(metric(&anomalies, "outliers"), "557");
    assert!(
        anomalies.svg.len() < 60_000,
        "{} bytes",
        anomalies.svg.len()
    );
}

// --- The oscilloscope: CH2 on CAL (1 kHz, ~2 Vpp), CH1 on a ~0.35 Vpp square.

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_cal_output_measures_two_volts_at_one_kilohertz() {
    let (_, capture) = load("cal_1msps_r2v5");
    let (vpp, vpp_note) = measure(&capture, 1, K::Vpp);
    close(vpp, 2.0, 0.05, "CAL Vpp");
    assert_eq!(vpp_note, None, "Vpp is offset-independent");
    close(
        measure(&capture, 1, K::Frequency).0,
        1000.0,
        1.0,
        "CAL frequency",
    );
    close(measure(&capture, 1, K::Duty).0, 50.0, 0.5, "CAL duty");
    close(measure(&capture, 0, K::Vpp).0, 0.33, 0.05, "CH1 Vpp");
    // Absolute volts carry the uncalibrated-offset note (B-41).
    let (_, vmin_note) = measure(&capture, 1, K::Vmin);
    assert!(vmin_note.unwrap().contains("zero offset not calibrated"));
    // At 1 MS/s the CAL edge is inside two samples: an upper bound (B-40).
    let (rise, rise_note) = measure(&capture, 1, K::RiseTime);
    assert!(rise < 2e-6, "{rise}");
    assert!(rise_note.unwrap().starts_with("upper bound"));
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_range_moves_absolute_levels_but_not_vpp() {
    // The same signals at the 2.5 V and 5 V ranges (B-14/B-41): the low
    // level shifts by ~90 mV, the swing does not.
    let (_, r2v5) = load("cal_1msps_r2v5");
    let (_, r5v) = load("cal_1msps_r5v");
    let low = |c: &Capture| measure(c, 1, K::Vmin).0;
    assert!(
        low(&r5v) - low(&r2v5) > 0.05,
        "{} vs {}",
        low(&r5v),
        low(&r2v5)
    );
    close(
        measure(&r5v, 1, K::Vpp).0,
        measure(&r2v5, 1, K::Vpp).0,
        0.05,
        "Vpp across ranges",
    );
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_cal_eye_is_a_clean_persistence_view() {
    let (_, capture) = load("cal_1msps_r2v5");
    let eye = view(&capture, ViewMode::Eye, 1);
    assert_eq!(metric(&eye, "unit_interval"), "1.00 ms");
    assert!(metric(&eye, "kind").starts_with("persistence"));
    assert_eq!(metric(&eye, "eye_width"), "500 us");
    let height: f64 = eye
        .metrics
        .iter()
        .find(|m| m.name == "eye_height")
        .and_then(|m| m.value)
        .unwrap();
    close(height, 1.94, 0.1, "eye height");
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_software_trigger_put_the_edge_at_the_midpoint() {
    // B-32: recorded with a CH2 rising trigger at 1.0 V; the capture claims
    // the edge is at sample 8192, and the samples agree.
    let (meta, capture) = load("cal_16msps_trig");
    assert_eq!(capture.meta.trigger_position, Some(8192));
    assert_eq!(meta.trigger.as_ref().unwrap()["level"], 1.0);
    let ch2 = channel_series(&capture, 1).unwrap().to_f64();
    assert!(
        ch2[8191] < 1.0 && ch2[8192] >= 1.0,
        "{:?}",
        &ch2[8188..8196]
    );
    // The last microseconds before the edge are the low level, not a
    // crossing the trigger skipped.
    assert!(ch2[7192..8192].iter().all(|&v| v < 1.0));
    // At 16 MS/s the CAL edge spans ~4 samples: a measurement, no bound.
    let (rise, note) = measure(&capture, 1, K::RiseTime);
    close(rise, 240e-9, 60e-9, "CAL rise at 16 MS/s");
    assert_eq!(note, None);
}

// --- The oscilloscope on a CAN bus at 500 kbit/s (16 MS/s: 32 samples per bit),
// ~0.18 V of swing, about four ADC codes. CH2 is not connected.

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn a_four_code_can_swing_thresholds_cleanly_into_whole_bit_cells() {
    use awic_core::threshold::{digitize, AnalogSource, ThresholdConfig};
    let (_, capture) = load("can_500k_noack_scope");
    let source = |channel| AnalogSource {
        channel,
        minus: None,
    };
    let d = digitize(&capture, &[source(0)], &ThresholdConfig::default()).unwrap();
    // The threshold sits between the dominant (code 137) and the recessive
    // (code 141) level, i.e. between 0.37 V and 0.53 V.
    let t = d.thresholds[0];
    let volts = |code: f64| code * 0.039215688 - 5.0;
    assert!(
        t.threshold_v > volts(137.0) && t.threshold_v < volts(141.0),
        "{t:?}"
    );
    assert!(!t.flat);
    assert!(d
        .describe(&capture)
        .starts_with("analog: CH1 threshold 0.4"));

    // Intervals are whole numbers of 32-sample bit cells, and a threshold
    // that chattered would show as many short ones. Two stretches of the
    // recording are not like that in the signal itself: a dominant run whose
    // fall is a slow 6-code slide (151 samples for 5 bits) and a clean
    // half-bit dominant pulse (17 samples) — real, so thresholding rightly
    // keeps them. The other ~390 intervals are all within 3 samples.
    let edges: Vec<(u64, u8)> = d
        .transitions
        .iter()
        .map(|t| (t.sample_index, t.channel_mask))
        .collect();
    assert!(edges.len() > 100, "{} edges", edges.len());
    let mut cells = Vec::new();
    let mut irregular = Vec::new();
    for pair in edges.windows(2) {
        let len = pair[1].0 - pair[0].0;
        let bits = (len + 16) / 32;
        if bits == 0 || len.abs_diff(bits * 32) > 3 {
            irregular.push((pair[0].0, len));
        }
        cells.push((pair[0].1 & 1 == 1, bits.max(1) as usize));
    }
    assert_eq!(irregular, vec![(5313, 151), (15342, 17)]);
    assert_eq!(edges.len(), 391);
    // The first frame after an idle of at least 11 recessive bits: SOF,
    // identifier 0x123 and on, as sent (stuff bits included), 0 = dominant.
    let start = cells
        .windows(2)
        .position(|w| w[0].0 && w[0].1 >= 11 && !w[1].0)
        .expect("an idle bus before a frame")
        + 1;
    let bits: String = cells[start..]
        .iter()
        .flat_map(|&(high, n)| std::iter::repeat_n(if high { '1' } else { '0' }, n))
        .take(31)
        .collect();
    assert_eq!(bits, "0001001000110001000110111101010");

    // CH2 is flat: nothing to threshold, nothing to decode.
    let d = digitize(&capture, &[source(1)], &ThresholdConfig::default()).unwrap();
    assert!(d.transitions.is_empty());
    assert!(d.thresholds[0].flat);
    assert!(d.describe(&capture).contains("flat"));
}

// --- CAN through the whole path: scope capture → auto threshold → auto
// bitrate → CAN decoder. A CANable2 alone on the bus retransmits each frame
// until closed, so every complete frame is the same one, unacknowledged.

/// Decodes CH1 of a CAN fixture with `bitrate: "auto"`; returns the
/// detected bitrate and the transactions.
fn decode_can(name: &str) -> (Option<u32>, Vec<serde_json::Value>) {
    decode_can_line(name, 0, None, "dominant_low")
}

/// [`decode_can`] on `channel` (minus `minus`) read with `polarity`.
fn decode_can_line(
    name: &str,
    channel: u8,
    minus: Option<u8>,
    polarity: &str,
) -> (Option<u32>, Vec<serde_json::Value>) {
    let (_, capture) = load(name);
    let channels = [ChannelMapping {
        role: ChannelRole::Can,
        channel,
        minus,
    }];
    let options = [
        OptionSetting {
            id: "bitrate".into(),
            value: OptionValue::Text("auto".into()),
        },
        OptionSetting {
            id: "polarity".into(),
            value: OptionValue::Text(polarity.into()),
        },
    ];
    let out = decode_capture(
        &capture,
        &DecodeSpec {
            decoder: DecoderId::Can,
            channels: &channels,
            options: &options,
            max_annotations: Some(1000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            // Frame by frame: these check the decoder, not folding (F-14).
            fold: Some(false),
        },
    )
    .unwrap();
    let tx = serde_json::to_value(&out.transactions).unwrap();
    (out.detected_baud, tx.as_array().unwrap().clone())
}

/// The complete frames. The window opens and closes inside a frame (the
/// bus is never idle long): those two partial ones, first and last, carry no
/// address, and nothing else is partial.
fn frames(tx: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    let (first, last) = (&tx[0], &tx[tx.len() - 1]);
    assert!(
        first["address"].is_null() && first["start_sample"] == 0,
        "{first}"
    );
    assert!(last["address"].is_null(), "{last}");
    let frames: Vec<_> = tx.iter().filter(|t| !t["address"].is_null()).collect();
    assert_eq!(frames.len(), tx.len() - 2);
    frames
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn can_at_eight_msps_decodes_every_retransmission() {
    let (baud, tx) = decode_can("can_500k_8msps");
    assert_eq!(baud, Some(500_000));
    let frames = frames(&tx);
    assert_eq!(frames.len(), 14);
    for f in &frames {
        assert_eq!(f["address"], 0x123, "{f}");
        assert_eq!(f["data"], "DE AD BE EF 01 02 03 04", "{f}");
        assert_eq!(f["acked"], false, "{f}");
        assert_eq!(f["notes"], serde_json::json!(["no ACK"]), "{f}");
    }
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn can_extended_id_from_the_scope() {
    let (baud, tx) = decode_can("can_500k_ext_8msps");
    assert_eq!(baud, Some(500_000));
    let frames = frames(&tx);
    assert_eq!(frames.len(), 16);
    for f in &frames {
        assert_eq!(f["address"], 0x1ABC_DEF0, "{f}");
        assert_eq!(f["data"], "CA FE F0 0D", "{f}");
        assert_eq!(f["notes"], serde_json::json!(["extended", "no ACK"]), "{f}");
    }
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn can_remote_frame_at_125_kbit() {
    let (baud, tx) = decode_can("can_125k_rtr_4msps");
    assert_eq!(baud, Some(125_000));
    let frames = frames(&tx);
    assert_eq!(frames.len(), 15);
    for f in &frames {
        assert_eq!(f["address"], 0x7FF, "{f}");
        assert_eq!(f["count"], 0, "{f}");
        assert_eq!(
            f["notes"],
            serde_json::json!(["remote, DLC 0", "no ACK"]),
            "{f}"
        );
    }
}

#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn canl_canh_and_the_difference_all_read_the_same_frames() {
    // Probes at x1 on both lines, no termination: CANH relaxes slowly after
    // a dominant bit and sits above its idle level inside a frame. At 16
    // samples per bit the one-bit pulses measure 15..17 samples, which the
    // auto bitrate must not take for the bit time (it did, from the 5th
    // percentile, and refused the bus).
    let lines = [
        (0, None, "dominant_low"),     // CANL
        (1, None, "dominant_high"),    // CANH
        (1, Some(0), "dominant_high"), // CANH - CANL
    ];
    let mut counts = Vec::new();
    for (channel, minus, polarity) in lines {
        let (baud, tx) = decode_can_line("can_500k_diff_8msps", channel, minus, polarity);
        assert_eq!(baud, Some(500_000), "CH{} {minus:?}", channel + 1);
        let frames: Vec<_> = tx.iter().filter(|t| !t["address"].is_null()).collect();
        for f in &frames {
            assert_eq!(f["address"], 0x123, "{f}");
            assert_eq!(f["data"], "DE AD BE EF 01 02 03 04", "{f}");
            assert_eq!(f["notes"], serde_json::json!(["no ACK"]), "{f}");
        }
        counts.push(frames.len());
    }
    assert!(
        counts[0] >= 10 && counts.iter().all(|&n| n == counts[0]),
        "{counts:?}"
    );
}

/// B-70: the eye of a real CAN capture. Frames 10 ms apart each start on their
/// own transmitter phase, and one TIE grid fitted across all of them measured
/// those phase differences and called them jitter — 1.3–2.3 µs on a 2 µs bit
/// whose cycle-to-cycle jitter was 500 ns. Fitted per burst, the number
/// describes the signal again.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_can_eye_fits_tie_per_burst_on_a_real_bus() {
    let (_, capture) = load("can_500k_diff_8msps");
    // CANH on its own: the eye folds one channel, and the bit time is what
    // B-57 recovers from the runs between crossings.
    let eye = view(&capture, ViewMode::Eye, 1);
    let unit_interval: f64 = eye
        .metrics
        .iter()
        .find(|m| m.name == "unit_interval")
        .and_then(|m| m.value)
        .expect("a unit interval");
    close(unit_interval * 1e6, 2.0, 0.1, "CAN bit time, µs");

    let bursts: f64 = eye
        .metrics
        .iter()
        .find(|m| m.name == "jitter_bursts")
        .and_then(|m| m.value)
        .expect("the reading says which grid it used");
    assert!(bursts >= 1.0, "{bursts} bursts");

    let tie: f64 = eye
        .metrics
        .iter()
        .find(|m| m.name == "jitter_tie_pp")
        .and_then(|m| m.value)
        .expect("TIE is measurable on this capture");
    // Under half a bit on a clean bus. Across the idle gaps this was most of a
    // bit time or more.
    assert!(
        tie < unit_interval / 2.0,
        "tie {:.3} µs on a {:.3} µs bit over {bursts} burst(s): the grid is spanning the gaps \
         between frames again",
        tie * 1e6,
        unit_interval * 1e6
    );
    // And cycle-to-cycle, which never depended on the grid, stays small.
    let c2c: f64 = eye
        .metrics
        .iter()
        .find(|m| m.name == "jitter_cycle_to_cycle_pp")
        .and_then(|m| m.value)
        .expect("cycle-to-cycle is measurable");
    assert!(c2c < unit_interval, "cycle-to-cycle {:.3} µs", c2c * 1e6);
}

// --- An MCP2515 on SPI at the sampling limit, and a CAN bus with a node that
// does not drive CANH (bench run 2026-10-10).

fn spi_decode(
    capture: &Capture,
    t0_s: Option<f64>,
    t1_s: Option<f64>,
) -> Vec<awic_proto::DecodedTransaction> {
    let channels = [
        (ChannelRole::Clk, 6),
        (ChannelRole::Mosi, 7),
        (ChannelRole::Miso, 5),
        (ChannelRole::Cs, 3),
    ]
    .map(|(role, channel)| ChannelMapping {
        role,
        channel,
        minus: None,
    });
    let out = decode_capture_range(
        capture,
        &DecodeSpec {
            decoder: DecoderId::Spi,
            channels: &channels,
            options: &[],
            max_annotations: Some(5000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            // Frame by frame: these check the decoder, not folding (F-14).
            fold: Some(false),
        },
        t0_s,
        t1_s,
    )
    .unwrap();
    assert!(!out.truncated);
    out.transactions
}

/// B-100: SCK at ~5.5 MHz is 1-2 samples per pulse at 16 MS/s. The survey
/// called it "floating input or crosstalk", took a MISO line for the clock
/// and CS for a 2.45 Mbaud UART.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_survey_finds_an_spi_clock_at_the_sampling_limit() {
    let (_, capture) = load("spi_mcp2515_16msps");
    let survey = survey_capture(&capture, "fixture".into(), None, None).unwrap();
    let guesses = serde_json::to_value(&survey.guesses).unwrap();
    assert_eq!(guesses.as_array().unwrap().len(), 1, "{guesses}");
    let spi = &guesses[0];
    assert_eq!(spi["protocol"], "spi", "{guesses}");
    assert_eq!(
        (spi["clk"].as_u64(), spi["cs"].as_u64()),
        (Some(6), Some(3))
    );
    assert_eq!(spi["mode"], 0);
    assert_eq!(spi["undersampled"], true);
    assert!(spi.get("clock_hz").is_none(), "too fast to time: {spi}");
    assert!(spi["data"].as_array().unwrap().contains(&7.into()), "{spi}");
    let sck = survey.channels.iter().find(|c| c.channel == 6).unwrap();
    assert_eq!(serde_json::to_value(sck.activity).unwrap(), "undersampled");
    assert!(sck.median_pulse_s.is_none(), "its long 'pulses' are pauses");
}

/// The decoder reads the MCP2515's commands at under 3 samples per bit: RX
/// STATUS polls, TXB0CTRL reads, two loads of the 0x100 frame with
/// consecutive counters, one of the extended 0x18FF50E5 frame carrying the
/// same counter, and RTS after each load.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_decode_reads_the_mcp2515_at_the_sampling_limit() {
    let (_, capture) = load("spi_mcp2515_16msps");
    let tx = spi_decode(&capture, None, None);
    assert_eq!(tx.len(), 343);
    // Every CS window here holds one transfer: none is flagged as a clock
    // pause that would hint at a missed CS release (issue #32).
    let flagged: Vec<_> = tx.iter().filter(|t| !t.notes.is_empty()).collect();
    assert!(flagged.is_empty(), "{flagged:?}");
    let mut ops = std::collections::BTreeMap::new();
    for t in &tx {
        *ops.entry((t.data[..2].to_string(), t.count)).or_insert(0) += 1;
    }
    let ops: Vec<_> = ops.into_iter().collect();
    assert_eq!(
        ops,
        [
            (("03".into(), 3), 81),
            (("40".into(), 10), 1),
            (("40".into(), 14), 2),
            (("81".into(), 1), 3),
            (("B0".into(), 2), 256),
        ]
    );
    let loads: Vec<&str> = tx
        .iter()
        .filter(|t| t.data.starts_with("40 20 00"))
        .map(|t| t.data.as_str())
        .collect();
    assert_eq!(
        loads,
        [
            "40 20 00 00 00 08 7C 1F 00 00 EE 93 34 00",
            "40 20 00 00 00 08 7D 1F 00 00 F8 93 34 00",
        ]
    );
    // SIDH C7, SIDL EB (EXIDE), EID8 50, EID0 E5: ID 0x18FF50E5.
    let ext = tx.iter().find(|t| t.count == 10).unwrap();
    let b: Vec<u32> = ext
        .data
        .split(' ')
        .map(|h| u32::from_str_radix(h, 16).unwrap())
        .collect();
    let id = b[1] << 21 | (b[2] >> 5) << 18 | (b[2] & 3) << 16 | b[3] << 8 | b[4];
    assert_eq!((id, b[2] & 0x08 != 0), (0x18FF_50E5, true));
    assert_eq!(&ext.data[18..], "00 00 1F 7C");
}

/// B-101: a window of the capture is decoded, not the whole of it. Opened in
/// the middle of the first load, it starts at the next chip select; two
/// windows split there give the whole decode back.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn a_window_of_the_spi_capture_decodes_only_that_window() {
    let (meta, capture) = load("spi_mcp2515_16msps");
    let rate = meta.rate_hz as f64;
    let all = spi_decode(&capture, None, None);
    let load = all
        .iter()
        .position(|t| t.data.starts_with("40 20"))
        .unwrap();
    let mid = (all[load].start_sample + all[load].end_sample) as f64 / 2.0 / rate;
    let after = spi_decode(&capture, Some(mid), None);
    assert_eq!(after[0].start_sample, all[load + 1].start_sample);
    assert_eq!(after.len(), all.len() - load - 1);
    let split = (all[load].end_sample + all[load + 1].start_sample) as f64 / 2.0 / rate;
    let mut joined = spi_decode(&capture, None, Some(split));
    joined.extend(spi_decode(&capture, Some(split), None));
    assert_eq!(joined, all);
}

fn can_frames(capture: &Capture, channel: u8, minus: Option<u8>, polarity: &str) -> Vec<String> {
    let channels = [ChannelMapping {
        role: ChannelRole::Can,
        channel,
        minus,
    }];
    let options = [OptionSetting {
        id: "polarity".into(),
        value: OptionValue::Text(polarity.into()),
    }];
    decode_capture(
        capture,
        &DecodeSpec {
            decoder: DecoderId::Can,
            channels: &channels,
            options: &options,
            max_annotations: Some(5000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            // Frame by frame: these check the decoder, not folding (F-14).
            fold: Some(false),
        },
    )
    .unwrap()
    .transactions
    .iter()
    .map(|t| match t.address {
        Some(id) => format!("{id:X} [{}] acked={:?}", t.data, t.acked),
        None => t.notes.join(", "),
    })
    .collect()
}

const LONE: &str = "lone dominant bit, not a frame: an ACK, or a frame this line does not carry";

/// A node (MCP2515 + TJA1050 on 3.3 V) that never drives CANH: CANL and the
/// difference decode every frame ACKed; CANH alone sees only the other
/// node's frame, unacknowledged, and of the weak node's frames only the
/// other node's ACK — a lone dominant bit, once called a stuff error (#32).
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn a_node_that_does_not_drive_canh() {
    let (_, capture) = load("can_500k_weak_canh_4msps");
    let both = [
        "100 [9E 2D 00 00 42 21 35 00] acked=Some(true)",
        "100 [9F 2D 00 00 4D 21 35 00] acked=Some(true)",
        "321 [46 C8] acked=Some(true)",
        "322 [C8 46] acked=Some(true)",
    ];
    assert_eq!(can_frames(&capture, 0, None, "dominant_low"), both);
    assert_eq!(can_frames(&capture, 1, Some(0), "dominant_high"), both);
    assert_eq!(
        can_frames(&capture, 1, None, "dominant_high"),
        [LONE, LONE, "321 [46 C8] acked=Some(false)", LONE,]
    );
}

/// F-09: a view of a real capture as PNG.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn a_view_of_the_can_capture_as_png() {
    let (_, capture) = load("can_500k_weak_canh_4msps");
    let v = view(&capture, ViewMode::Waveform, 0);
    let png = awic_render::png::svg_to_png(&v.svg).unwrap();
    assert!(png.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
}

/// The same bench with that module on 5 V: both nodes drive both lines, so
/// CANH alone, CANL alone and the difference all read every frame, ACKed —
/// the counterpart of [`a_node_that_does_not_drive_canh`].
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn a_bus_driven_both_ways_reads_the_same_on_every_line() {
    let (_, capture) = load("can_500k_both_ways_4msps");
    let frames = [
        "100 [C4 2F 00 00 82 19 3E 00] acked=Some(true)",
        "321 [80 A5] acked=Some(true)",
        "322 [A5 80] acked=Some(true)",
        "100 [C5 2F 00 00 8D 19 3E 00] acked=Some(true)",
        "100 [C6 2F 00 00 96 19 3E 00] acked=Some(true)",
    ];
    assert_eq!(can_frames(&capture, 0, None, "dominant_high"), frames);
    assert_eq!(can_frames(&capture, 1, None, "dominant_low"), frames);
    assert_eq!(can_frames(&capture, 0, Some(1), "dominant_high"), frames);
}

// --- The same bus with one of its two 120 ohm terminators removed
// (2026-10-10, #34).

fn anomalies(capture: &Capture, channel: u8, kind: IntervalKind, tolerance: Option<f64>) -> View {
    render(
        capture,
        &ViewRequest {
            mode: ViewMode::Anomalies,
            channels: vec![channel],
            interval_kind: Some(kind),
            tolerance,
            ..Default::default()
        },
    )
    .unwrap()
}

/// B-102: CANH's dominant runs are 1-5 bits of 2 us — NRZ data, held against
/// whole bit times, not "+398 %" outliers. What is left at the default 5 %
/// is one sample of quantisation (12.5 % of an 8-sample bit), and the view
/// says so; at 20 % nothing is left. The recessive runs include the idle
/// gaps between frames, which are not judged.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_anomaly_view_reads_can_bits_as_nrz_data() {
    let (_, capture) = load("can_500k_one_terminator_4msps");
    let v = anomalies(&capture, 0, IntervalKind::High, None);
    assert_eq!(metric(&v, "reference_kind"), "unit interval (NRZ data)");
    assert_eq!(metric(&v, "reference"), "2.00 us");
    assert_eq!(metric(&v, "intervals"), "150");
    assert_eq!(metric(&v, "outliers"), "2");
    assert!(
        v.notes.iter().any(|n| n.contains("finer than one sample")),
        "{:?}",
        v.notes
    );
    let v = anomalies(&capture, 0, IntervalKind::High, Some(0.2));
    assert_eq!(metric(&v, "outliers"), "0");
    let v = anomalies(&capture, 0, IntervalKind::Low, Some(0.2));
    assert_eq!(metric(&v, "outliers"), "0");
    assert_eq!(metric(&v, "idle_gaps"), "7");
}

/// B-103: a frequency of CAN data is no clock rate, and says so with the
/// bit rate that is one.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn measure_says_can_data_is_not_a_clock() {
    let (_, capture) = load("can_500k_one_terminator_4msps");
    for channel in [0, 1] {
        let (_, note) = measure(&capture, channel, K::Frequency);
        let note = note.expect("a note");
        assert!(note.contains("NRZ data, not a clock"), "{note}");
        assert!(note.contains("k bit/s"), "{note}");
    }
}

/// B-99: CANH - CANL is cut at the standard 0.7 V, not at the 1.43 V
/// midpoint of this (stronger, one terminator) swing, and every frame
/// decodes ACKed.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn a_differential_can_decode_uses_the_standard_threshold() {
    let (_, capture) = load("can_500k_one_terminator_4msps");
    let channels = [ChannelMapping {
        role: ChannelRole::Can,
        channel: 0,
        minus: Some(1),
    }];
    let options = [OptionSetting {
        id: "polarity".into(),
        value: OptionValue::Text("dominant_high".into()),
    }];
    let out = decode_capture(
        &capture,
        &DecodeSpec {
            decoder: DecoderId::Can,
            channels: &channels,
            options: &options,
            max_annotations: Some(5000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            // Frame by frame: these check the decoder, not folding (F-14).
            fold: Some(false),
        },
    )
    .unwrap();
    let note = out.note.unwrap();
    assert!(
        note.contains("threshold 0.700 V (standard differential threshold"),
        "{note}"
    );
    assert_eq!(
        can_frames(&capture, 0, Some(1), "dominant_high"),
        [
            "100 [DB 05 00 00 AB 1E 57 00] acked=Some(true)",
            "100 [DC 05 00 00 B5 1E 57 00] acked=Some(true)",
            "321 [22 0D] acked=Some(true)",
            "322 [0D 22] acked=Some(true)",
            "18FF50E5 [00 00 05 DC] acked=Some(true)",
            "100 [DD 05 00 00 BF 1E 57 00] acked=Some(true)",
            "7DF [] acked=Some(true)",
            "200 [] acked=Some(true)",
        ]
    );
}

/// F-14: identical transactions in a row come back as one entry. The 14
/// unacknowledged retransmissions of 0x123 are one entry with `repeat: 14`
/// (the two partial frames at the window's ends stay apart), and the summary
/// still counts every frame. The MCP2515 bus polls RX STATUS every ~54 us,
/// which the burst structure finds; the one SSD1306 write is no bursts.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn repeated_transactions_fold_and_bursts_are_found() {
    let decode = |name: &str, decoder, channels: &[ChannelMapping], options: &[OptionSetting]| {
        let (_, capture) = load(name);
        decode_capture(
            &capture,
            &DecodeSpec {
                decoder,
                channels,
                options,
                max_annotations: Some(5000),
                max_level: None,
                format: Some(DecodeFormat::Transactions),
                max_bytes: None,
                fold: None,
            },
        )
        .unwrap()
    };
    let can = [ChannelMapping {
        role: ChannelRole::Can,
        channel: 0,
        minus: None,
    }];
    let auto = [OptionSetting {
        id: "bitrate".into(),
        value: OptionValue::Text("auto".into()),
    }];
    let out = decode("can_500k_8msps", DecoderId::Can, &can, &auto);
    assert_eq!(out.transactions.len(), 3);
    let frame = &out.transactions[1];
    assert_eq!(
        (frame.address, frame.repeat, frame.data.as_str()),
        (Some(0x123), Some(14), "DE AD BE EF 01 02 03 04")
    );
    assert_eq!(frame.notes, ["no ACK"]);
    let summary = out.summary.unwrap();
    assert_eq!((summary.transactions, summary.folded), (16, 13));
    assert_eq!(
        (summary.nacks, summary.by_address[0].transactions),
        (14, 14)
    );
    assert_eq!(summary.bursts, None, "back-to-back retransmissions");

    let spi = [
        (ChannelRole::Clk, 6),
        (ChannelRole::Mosi, 7),
        (ChannelRole::Miso, 5),
        (ChannelRole::Cs, 3),
    ]
    .map(|(role, channel)| ChannelMapping {
        role,
        channel,
        minus: None,
    });
    let out = decode("spi_mcp2515_16msps", DecoderId::Spi, &spi, &[]);
    let summary = out.summary.unwrap();
    assert_eq!(summary.transactions, 343, "the same count as unfolded");
    assert_eq!(out.transactions.len() as u32, 343 - summary.folded);
    let bursts = summary.bursts.expect("the poll loop");
    close(bursts.period_s, 54e-6, 1e-6, "poll period");
    assert_eq!(bursts.transactions_min, 1);
    assert!(bursts.gap_s > 3e-6 && bursts.gap_s < 50e-6, "{bursts:?}");

    let i2c =
        [(ChannelRole::Scl, 6), (ChannelRole::Sda, 4)].map(|(role, channel)| ChannelMapping {
            role,
            channel,
            minus: None,
        });
    let out = decode("i2c_ssd1306_4msps", DecoderId::I2c, &i2c, &[]);
    let summary = out.summary.unwrap();
    assert_eq!((summary.transactions, summary.folded), (66, 28));
    assert_eq!(summary.by_address[0].transactions, 66);
    assert_eq!(summary.bursts, None, "one write, evenly spaced");
    // Without folding, every transaction is its own entry.
    let (_, capture) = load("i2c_ssd1306_4msps");
    let all = decode_capture(
        &capture,
        &DecodeSpec {
            decoder: DecoderId::I2c,
            channels: &i2c,
            options: &[],
            max_annotations: Some(5000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            fold: Some(false),
        },
    )
    .unwrap();
    assert_eq!(all.transactions.len(), 66);
    assert!(all.transactions.iter().all(|t| t.repeat.is_none()));
}

// ---- uart_bitbanged_9600_200ksps: 9600 Bd bit-banged from Lua (B-108) ----
//
// An ESP32-C6 sends text lines on GPIO3 by waiting out each 104 us bit in a
// busy loop. Its one-bit pulses measure 86..120 us, every stop bit runs on
// for about a third of a bit, and the first start bit after each 100 ms
// sleep runs late — one runt per line. A UART receiver reads it; AWIC's
// automatic baud detection, the survey and the anomaly view did not.

/// The survey names the line a 9600 Bd UART: its low runs, start bit and
/// zeros, are whole bits even though its high runs end in stretched stop
/// bits.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_survey_names_a_bit_banged_uart() {
    let (_, capture) = load("uart_bitbanged_9600_200ksps");
    let survey = survey_capture(&capture, "fixture".into(), None, None).unwrap();
    let guesses = serde_json::to_value(&survey.guesses).unwrap();
    assert_eq!(guesses[0]["protocol"], "uart", "{guesses}");
    assert_eq!(guesses[0]["channel"], 6, "{guesses}");
    assert_eq!(guesses[0]["baud"], 9600, "{guesses}");
}

/// `baud: "auto"` finds 9600 and reads the five lines as explicitly given
/// 9600 does — including the three late-started first characters, which
/// the board's own receiver got wrong too.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn auto_baud_reads_a_bit_banged_uart() {
    let (_, capture) = load("uart_bitbanged_9600_200ksps");
    let channels = [ChannelMapping {
        role: ChannelRole::Rx,
        channel: 6,
        minus: None,
    }];
    let options = [OptionSetting {
        id: "baud".into(),
        value: OptionValue::Text("auto".into()),
    }];
    let out = decode_capture(
        &capture,
        &DecodeSpec {
            decoder: DecoderId::Uart,
            channels: &channels,
            options: &options,
            max_annotations: Some(5000),
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            fold: Some(false),
        },
    )
    .unwrap();
    assert_eq!(out.detected_baud, Some(9600));
    let summary = out.summary.unwrap();
    assert_eq!((summary.transactions, summary.errors), (5, 3));
    let tx = serde_json::to_value(&out.transactions).unwrap();
    assert_eq!(tx[1]["text"], "t=122 ms temp=21.1 C\\r\\n", "{tx}");
}

/// The anomaly view holds the low runs against whole bits: a runt is not
/// the unit interval.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn the_anomaly_view_reads_bit_banged_uart_as_nrz_data() {
    let (_, capture) = load("uart_bitbanged_9600_200ksps");
    let v = anomalies(&capture, 6, IntervalKind::Low, None);
    assert_eq!(metric(&v, "reference_kind"), "unit interval (NRZ data)");
    assert_eq!(metric(&v, "intervals"), "316");
    // At 200 kS/s a sample is 5 % of a bit, so the default tolerance calls
    // out the busy-wait jitter; at a quarter of a bit what remains is the
    // four runts and three long runs the jitter stretched past it.
    let v = anomalies(&capture, 6, IntervalKind::Low, Some(0.25));
    assert_eq!(metric(&v, "outliers"), "7");
}
