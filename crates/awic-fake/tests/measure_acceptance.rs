//! Acceptance tests for server-side measurements (SPECIFICATION.md §5.4),
//! ground-truthed against `awic-fake`'s synthetic signals — the same
//! contract the M3/M4 exit criteria named ("measure
//! its period", "reads correct amplitude and frequency").
//!
//! Every test drives a real `Instrument` end to end (`prepare` ->
//! `configure` -> `start` -> drain -> `stop`), files the result into a real
//! `CaptureStore` exactly as `awic-server` does, and then calls
//! `awic_core::measure_capture` — never awic-core's measurement internals
//! directly — so this is the same path a client's `capture.measure` request
//! exercises.

use awic_core::{
    measure_capture, AcqConfig, CaptureId, CaptureStore, Instrument, NewAnalogCapture,
    NewDigitalCapture, SampleSink, Trigger,
};
use awic_fake::{
    AnalogChannelConfig, AnalogDeviceSpec, AnalogInstrument, AnalogWaveform, DigitalChannelSource,
    DigitalDeviceSpec, DigitalInstrument,
};
use awic_proto::{MeasurementKind, MeasurementResult};

async fn drain(mut rx: tokio::sync::mpsc::Receiver<awic_core::SampleChunk>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk.data);
    }
    out
}

fn ok_value(result: &MeasurementResult) -> f64 {
    match result {
        MeasurementResult::Ok { value, .. } => *value,
        MeasurementResult::Unavailable { kind, unavailable } => {
            panic!("expected Ok for {kind:?}, got Unavailable({unavailable:?})")
        }
    }
}

fn assert_unavailable(result: &MeasurementResult) {
    assert!(
        matches!(result, MeasurementResult::Unavailable { .. }),
        "expected Unavailable, got {result:?}"
    );
}

/// Runs one analog channel-0 acquisition end to end and files it into a
/// fresh [`CaptureStore`], returning the store and the capture id.
async fn capture_analog(
    channel_cfg: AnalogChannelConfig,
    rate_hz: u64,
    samples: u64,
    trigger: Option<Trigger>,
) -> (CaptureStore, CaptureId) {
    let spec = AnalogDeviceSpec::scope_2ch_like();
    let mut instrument = AnalogInstrument::new(spec);
    instrument.set_channel(0, channel_cfg);
    instrument.prepare().await.unwrap();

    let effective = instrument
        .configure(AcqConfig {
            rate_hz,
            channels: vec![0],
            samples,
            trigger,
            ranges_v: vec![],
            continuous: false,
        })
        .await
        .unwrap();

    let (sink, rx) = SampleSink::channel(8);
    instrument.start(sink).await.unwrap();
    let raw = drain(rx).await;
    instrument.stop().await.unwrap();
    assert_eq!(raw.len(), samples as usize);

    let cal = &effective.calibration[0];
    let store = CaptureStore::with_defaults();
    let id = CaptureId::new();
    let filed = store.insert_analog(NewAnalogCapture {
        channels: (0..1).collect(),
        id,
        device: effective_device(),
        rate_hz: effective.cfg.rate_hz,
        channel_count: 1,
        sample_bits: 8,
        channel_scale: vec![cal.scale],
        channel_offset: vec![cal.offset],
        raw,
        t0_ns: 0,
        trigger_position: None,
    });
    assert_eq!(filed, id);
    (store, id)
}

fn effective_device() -> awic_core::DeviceId {
    AnalogDeviceSpec::scope_2ch_like().id
}

#[tokio::test]
async fn sine_1khz_at_1ms_reads_correct_frequency_vpp_and_vrms() {
    let rate_hz = 1_000_000u64;
    let periods = 40u64;
    let samples = periods * 1_000; // 1000 samples/period at 1 kHz, 1 MS/s
    let (store, id) = capture_analog(
        AnalogChannelConfig::sine(2.0, 1_000.0, 5.0),
        rate_hz,
        samples,
        None,
    )
    .await;
    let capture = store.get(&id).unwrap();

    let results = measure_capture(
        &capture,
        0,
        &[
            MeasurementKind::Frequency,
            MeasurementKind::Vpp,
            MeasurementKind::Vrms,
        ],
    )
    .unwrap();

    let freq = ok_value(&results[0]);
    let rel_err = (freq - 1_000.0).abs() / 1_000.0;
    assert!(
        rel_err < 0.001,
        "frequency {freq} Hz, relative error {rel_err}"
    );

    // One LSB on the ±5 V range over 8 bits.
    let lsb = (2.0 * 5.0) / 255.0;
    let vpp = ok_value(&results[1]);
    assert!((vpp - 4.0).abs() < 2.0 * lsb, "vpp {vpp} V, lsb {lsb}");

    let expected_vrms = 2.0 / std::f64::consts::SQRT_2;
    let vrms = ok_value(&results[2]);
    assert!(
        (vrms - expected_vrms).abs() / expected_vrms < 0.01,
        "vrms {vrms} V, expected ~{expected_vrms} V"
    );
}

#[tokio::test]
async fn sine_with_noise_floor_still_reads_frequency_within_half_a_percent() {
    let rate_hz = 1_000_000u64;
    let periods = 40u64;
    let samples = periods * 1_000;
    let channel_cfg = AnalogChannelConfig {
        waveform: AnalogWaveform::Sine {
            amplitude_v: 2.0,
            freq_hz: 1_000.0,
            offset_v: 0.0,
        },
        // A real noise floor, large enough to matter but well inside the
        // hysteresis dead band around the crossing.
        noise_floor_v: 0.15,
        range_v: 5.0,
    };
    let (store, id) = capture_analog(channel_cfg, rate_hz, samples, None).await;
    let capture = store.get(&id).unwrap();

    let results = measure_capture(&capture, 0, &[MeasurementKind::Frequency]).unwrap();
    let freq = ok_value(&results[0]);
    let rel_err = (freq - 1_000.0).abs() / 1_000.0;
    assert!(
        rel_err < 0.005,
        "frequency {freq} Hz with noise floor, relative error {rel_err} exceeds 0.5%"
    );
}

#[tokio::test]
async fn square_wave_with_known_duty_measures_within_one_sample_period() {
    let rate_hz = 1_000_000u64;
    let freq_hz = 1_000.0;
    let period_samples = rate_hz as f64 / freq_hz;
    let duty = 0.3f32;
    let channel_cfg = AnalogChannelConfig {
        waveform: AnalogWaveform::Square {
            amplitude_v: 2.0,
            freq_hz,
            offset_v: 0.0,
            duty,
        },
        noise_floor_v: 0.0,
        range_v: 5.0,
    };
    let samples = 40_000u64; // 40 periods
    let (store, id) = capture_analog(channel_cfg, rate_hz, samples, None).await;
    let capture = store.get(&id).unwrap();

    let results = measure_capture(&capture, 0, &[MeasurementKind::Duty]).unwrap();
    let measured_duty = ok_value(&results[0]) / 100.0;
    let tolerance = 1.0 / period_samples; // one sample period, as a fraction
    assert!(
        (measured_duty - duty as f64).abs() < tolerance,
        "duty {measured_duty}, expected {duty} +/- {tolerance}"
    );
}

#[tokio::test]
async fn a_capture_shorter_than_one_period_reports_frequency_unavailable_not_a_number() {
    let rate_hz = 1_000_000u64;
    // A 1 kHz sine has a 1000-sample period at this rate; 300 samples is
    // less than one third of a cycle.
    let (store, id) = capture_analog(
        AnalogChannelConfig::sine(2.0, 1_000.0, 5.0),
        rate_hz,
        300,
        None,
    )
    .await;
    let capture = store.get(&id).unwrap();

    let results = measure_capture(&capture, 0, &[MeasurementKind::Frequency]).unwrap();
    assert_unavailable(&results[0]);
}

#[tokio::test]
async fn a_flat_dc_capture_reads_zero_vpp_with_period_and_duty_unavailable() {
    let channel_cfg = AnalogChannelConfig {
        waveform: AnalogWaveform::Dc { level_v: 1.23 },
        noise_floor_v: 0.0,
        range_v: 5.0,
    };
    let (store, id) = capture_analog(channel_cfg, 1_000_000, 1_000, None).await;
    let capture = store.get(&id).unwrap();

    let results = measure_capture(
        &capture,
        0,
        &[
            MeasurementKind::Vpp,
            MeasurementKind::Period,
            MeasurementKind::Duty,
        ],
    )
    .unwrap();
    let vpp = ok_value(&results[0]);
    assert!(vpp.abs() < 1e-6, "flat DC must read Vpp ~= 0, got {vpp}");
    assert_unavailable(&results[1]);
    assert_unavailable(&results[2]);
}

/// A known digital clock on channel 0, plus a differently-toggling
/// neighbour on channel 1 — proves both the exact frequency/duty/edge-count
/// arithmetic and that the bitmask handling keeps the neighbour's edges from
/// leaking into channel 0's measurement.
#[tokio::test]
async fn digital_clock_reads_exact_measurements_unaffected_by_a_neighbouring_channel() {
    let spec = DigitalDeviceSpec::logic_8ch_like();
    let mut instrument = DigitalInstrument::new(spec);
    // Channel 0: toggles every 500 samples -> 1000-sample period -> 1 kHz at 1 MS/s.
    instrument.set_channel(
        0,
        DigitalChannelSource::CounterBit {
            divider: 500,
            bit: 0,
        },
    );
    // Channel 1: toggles every 100 samples -> 200-sample period -> 5 kHz.
    instrument.set_channel(
        1,
        DigitalChannelSource::CounterBit {
            divider: 100,
            bit: 0,
        },
    );
    instrument.prepare().await.unwrap();

    let rate_hz = 1_000_000u64;
    let samples = 20_000u64; // 20 periods of channel 0
    instrument
        .configure(AcqConfig {
            rate_hz,
            channels: vec![0, 1],
            samples,
            trigger: None,
            ranges_v: vec![],
            continuous: false,
        })
        .await
        .unwrap();

    let (sink, rx) = SampleSink::channel(8);
    instrument.start(sink).await.unwrap();
    let packed = drain(rx).await;
    instrument.stop().await.unwrap();
    assert_eq!(packed.len(), samples as usize);

    let store = CaptureStore::with_defaults();
    let id = CaptureId::new();
    store.insert_digital(NewDigitalCapture {
        channels: (0..2).collect(),
        id,
        device: DigitalDeviceSpec::logic_8ch_like().id,
        rate_hz,
        channel_count: 2,
        packed,
        t0_ns: 0,
        trigger_position: None,
    });
    let capture = store.get(&id).unwrap();

    let ch0 = measure_capture(
        &capture,
        0,
        &[
            MeasurementKind::Frequency,
            MeasurementKind::Duty,
            MeasurementKind::EdgeCount,
        ],
    )
    .unwrap();
    assert!(
        (ok_value(&ch0[0]) - 1_000.0).abs() < 1e-6,
        "ch0 frequency {:?}",
        ch0[0]
    );
    assert!(
        (ok_value(&ch0[1]) - 50.0).abs() < 1e-6,
        "ch0 duty {:?}",
        ch0[1]
    );
    assert_eq!(ok_value(&ch0[2]), 39.0, "ch0 edge count");

    // The neighbour toggles five times as fast and must not leak into ch0's
    // count above, nor should ch0 leak into ch1's own reading.
    let ch1 = measure_capture(&capture, 1, &[MeasurementKind::Frequency]).unwrap();
    assert!(
        (ok_value(&ch1[0]) - 5_000.0).abs() < 1e-6,
        "ch1 frequency {:?}",
        ch1[0]
    );
}
