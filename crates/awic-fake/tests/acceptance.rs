//! Acceptance tests for `awic-fake`.
//!
//! Exercises the crate exactly as a caller (server, or another test) would:
//! through `awic-core`'s `Instrument` trait, never awic-fake internals
//! directly, except where a helper (`analog_scale_offset`,
//! `trigger_target_index`) is part of the crate's public contract.

use awic_core::capture_store::{DEFAULT_MAX_BYTES, DEFAULT_MAX_CAPTURES};
use awic_core::{
    AcqConfig, CaptureId, CaptureStore, Instrument, NewAnalogCapture, SampleSink, Slope, Trigger,
};
use awic_fake::{
    analog_scale_offset, AnalogChannelConfig, AnalogDeviceSpec, AnalogInstrument,
    DigitalChannelSource, DigitalDeviceSpec, DigitalInstrument, UartSource,
};

/// Reads every chunk off `rx` into one contiguous byte buffer, in order.
async fn drain(mut rx: tokio::sync::mpsc::Receiver<awic_core::SampleChunk>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk.data);
    }
    out
}

#[tokio::test]
async fn measured_frequency_of_1khz_sine_at_1ms_per_s_is_within_0_1_percent() {
    let spec = AnalogDeviceSpec::scope_2ch_like();
    let mut instrument = AnalogInstrument::new(spec);
    instrument.set_channel(0, AnalogChannelConfig::sine(2.0, 1_000.0, 5.0));
    instrument.prepare().await.unwrap();

    let rate_hz = 1_000_000u64;
    let samples = 40_000u64; // 40 periods of a 1 kHz signal at 1 MS/s.
    instrument
        .configure(AcqConfig {
            rate_hz,
            channels: vec![0],
            samples,
            trigger: None,
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

    let (scale, offset) = analog_scale_offset(5.0);
    let voltages: Vec<f64> = raw
        .iter()
        .map(|&r| f64::from(r) * f64::from(scale) + f64::from(offset))
        .collect();

    // Zero-crossing frequency estimator, written here rather than shipped in
    // the crate: linear-interpolate between the two samples straddling each
    // rising zero crossing for sub-sample timing precision, then average the
    // period across every crossing found.
    let mut crossings = Vec::new();
    for i in 1..voltages.len() {
        let (a, b) = (voltages[i - 1], voltages[i]);
        if a < 0.0 && b >= 0.0 {
            let frac = -a / (b - a);
            crossings.push((i - 1) as f64 + frac);
        }
    }
    assert!(
        crossings.len() >= 10,
        "expected many zero crossings, got {}",
        crossings.len()
    );

    let span = crossings[crossings.len() - 1] - crossings[0];
    let periods = (crossings.len() - 1) as f64;
    let measured_period_samples = span / periods;
    let measured_freq_hz = rate_hz as f64 / measured_period_samples;

    let relative_error = (measured_freq_hz - 1_000.0).abs() / 1_000.0;
    assert!(
        relative_error < 0.001,
        "measured {measured_freq_hz} Hz, relative error {relative_error} exceeds 0.1%"
    );
}

#[tokio::test]
async fn one_million_sample_two_channel_capture_lands_in_capture_store_within_bounds() {
    let spec = AnalogDeviceSpec::scope_2ch_like();
    let device_id = spec.id.clone();
    let mut instrument = AnalogInstrument::new(spec);
    instrument.prepare().await.unwrap();

    let rate_hz = 24_000_000u64; // the oscilloscope's default rate
    let samples = 1_000_000u64;
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
    let raw = drain(rx).await;
    instrument.stop().await.unwrap();
    // Two channels, interleaved: 2 raw bytes per sample instant.
    assert_eq!(raw.len(), samples as usize * 2);

    let store = CaptureStore::with_defaults();
    let (scale, offset) = analog_scale_offset(5.0);
    let id = store.insert_analog(NewAnalogCapture {
        channels: (0..2).collect(),
        id: CaptureId::new(),
        device: device_id,
        rate_hz,
        channel_count: 2,
        sample_bits: 8,
        channel_scale: vec![scale, scale],
        channel_offset: vec![offset, offset],
        raw,
        t0_ns: 0,
        trigger_position: None,
    });

    assert!(store.len() <= DEFAULT_MAX_CAPTURES);
    assert!(store.total_bytes() <= DEFAULT_MAX_BYTES);
    let capture = store
        .get(&id)
        .expect("capture must be retrievable by its id");
    assert_eq!(capture.meta.channel_count, 2);
    assert_eq!(capture.meta.rate_hz, rate_hz);
}

#[tokio::test]
async fn configured_edge_trigger_lands_at_the_expected_sample_position() {
    let spec = AnalogDeviceSpec::scope_2ch_like();
    let mut instrument = AnalogInstrument::new(spec);
    // A square wave gives a clean, discontinuous transition to trigger on —
    // a sine's slope this close to its zero crossing (relative to an 8-bit
    // ADC's LSB, at 1000 samples/period) can span several samples at the
    // same quantized value, which is realistic ADC behaviour but makes the
    // "exact sample" assertion below ambiguous by construction, not a bug.
    instrument.set_channel(
        0,
        AnalogChannelConfig {
            waveform: awic_fake::AnalogWaveform::Square {
                amplitude_v: 2.0,
                freq_hz: 1_000.0,
                offset_v: 0.0,
                duty: 0.5,
            },
            noise_floor_v: 0.0,
            range_v: 5.0,
        },
    );
    instrument.prepare().await.unwrap();

    let rate_hz = 1_000_000u64;
    let samples = 4_000u64;
    let level = 0.0f32;
    instrument
        .configure(AcqConfig {
            rate_hz,
            channels: vec![0],
            samples,
            trigger: Some(Trigger::Edge {
                channel: 0,
                slope: Slope::Rising,
                level: Some(level),
            }),
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

    let (scale, offset) = analog_scale_offset(5.0);
    let voltages: Vec<f64> = raw
        .iter()
        .map(|&r| f64::from(r) * f64::from(scale) + f64::from(offset))
        .collect();

    let expected = awic_fake::trigger_target_index(samples) as usize;
    assert!(expected > 0 && expected < voltages.len());
    assert!(
        voltages[expected - 1] < f64::from(level) && voltages[expected] >= f64::from(level),
        "rising edge did not land at the configured trigger position {expected}: \
         v[{}]={} v[{}]={}",
        expected - 1,
        voltages[expected - 1],
        expected,
        voltages[expected]
    );
}

#[tokio::test]
async fn uart_digital_source_round_trips_through_the_full_instrument() {
    let spec = DigitalDeviceSpec::logic_8ch_like();
    let mut instrument = DigitalInstrument::new(spec);

    let bytes = vec![0xA5u8, 0x3C, 0x00, 0xFF, 0x81];
    let baud = 9_600u32;
    instrument.set_channel(
        0,
        DigitalChannelSource::Uart(UartSource::new(baud, bytes.clone())),
    );
    instrument.prepare().await.unwrap();

    let rate_hz = 1_000_000u64;
    let bit_samples = (rate_hz as f64 / f64::from(baud)).round() as i64;
    let frame_bits = 10 + 2; // start + 8 data + stop + UartSource's default 2-bit gap
    let frame_samples = frame_bits * bit_samples;
    // Two full loops through the byte sequence.
    let samples = (frame_samples * bytes.len() as i64 * 2) as u64;

    instrument
        .configure(AcqConfig {
            rate_hz,
            channels: vec![0],
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

    let bits: Vec<bool> = packed.iter().map(|&b| (b & 0x01) != 0).collect();

    // Inline UART decoder: find each start bit's falling edge, sample every
    // subsequent bit cell's midpoint LSB-first, and require a valid (high)
    // stop bit. Deliberately independent of `awic_fake`'s own generator
    // logic — this is what proves the waveform is genuinely decodable
    // rather than merely plausible-looking, ahead of T-M5.
    let bs = bit_samples as usize;
    let mut decoded = Vec::new();
    let mut i = 1usize;
    while i < bits.len() {
        if bits[i - 1] && !bits[i] {
            let mut byte = 0u8;
            let mut ok = true;
            for d in 0..8 {
                let idx = i + bs / 2 + (d + 1) * bs;
                if idx >= bits.len() {
                    ok = false;
                    break;
                }
                if bits[idx] {
                    byte |= 1 << d;
                }
            }
            let stop_idx = i + bs / 2 + 9 * bs;
            if ok && stop_idx < bits.len() && bits[stop_idx] {
                decoded.push(byte);
                i = stop_idx + bs;
                continue;
            }
        }
        i += 1;
    }

    // The capture starts mid-stream relative to the free-running UART
    // source (there is no "before sample 0" in a real acquisition either),
    // so the very first decoded byte can be a partial frame straddling the
    // capture boundary — exactly what a real decoder discards. Everything
    // after that must be a clean, exact round trip.
    let decoded = &decoded[1..];
    assert!(
        decoded.len() >= bytes.len(),
        "decoded too few complete bytes to prove a round trip: {decoded:?}"
    );

    // The capture window need not start exactly on a byte boundary, so
    // align on whichever of `bytes` the first (complete) decoded byte is,
    // then require every following byte to continue the exact same cyclic
    // sequence — proving the recovered bytes are exactly what was emitted.
    let start = bytes
        .iter()
        .position(|&b| b == decoded[0])
        .expect("first complete decoded byte must be one of the emitted bytes");
    for (j, &d) in decoded.iter().enumerate() {
        assert_eq!(
            d,
            bytes[(start + j) % bytes.len()],
            "decoded sequence diverged from the emitted bytes at position {j}"
        );
    }
}

/// The calibration `configure()` reports is the one the samples obey: a
/// requested range is applied, and `raw * scale + offset` recovers volts.
#[tokio::test]
async fn configure_applies_the_requested_range_and_its_calibration_recovers_volts() {
    let mut instrument = AnalogInstrument::new(AnalogDeviceSpec::scope_2ch_like());
    instrument.prepare().await.unwrap();
    // A 0.8 V DC level on channel 0, measured on the ±1.25 V range.
    instrument.set_channel(
        0,
        AnalogChannelConfig {
            waveform: awic_fake::AnalogWaveform::Dc { level_v: 0.8 },
            noise_floor_v: 0.0,
            range_v: 5.0,
        },
    );
    let effective = instrument
        .configure(AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0],
            samples: 1_000,
            trigger: None,
            ranges_v: vec![1.25],
            continuous: false,
        })
        .await
        .unwrap();
    assert_eq!(
        effective.cfg.ranges_v,
        vec![1.25],
        "the requested range is applied"
    );
    let cal = &effective.calibration[0];
    assert_eq!((cal.unit.as_str(), cal.range_v), ("V", Some(1.25)));

    let (sink, rx) = SampleSink::channel(8);
    instrument.start(sink).await.unwrap();
    let raw = drain(rx).await;
    let volts = f32::from(raw[500]) * cal.scale + cal.offset;
    // One LSB on ±1.25 V over 8 bits is ~9.8 mV.
    assert!(
        (volts - 0.8).abs() < 0.01,
        "calibration recovered {volts} V, expected 0.8 V"
    );
}

#[tokio::test]
async fn configure_rejects_a_range_the_device_does_not_have() {
    let mut instrument = AnalogInstrument::new(AnalogDeviceSpec::scope_2ch_like());
    instrument.prepare().await.unwrap();
    let err = instrument
        .configure(AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0],
            samples: 1_000,
            trigger: None,
            ranges_v: vec![3.3],
            continuous: false,
        })
        .await
        .expect_err("±3.3 V is not on the device's range list");
    assert!(err.to_string().contains("3.3"), "{err}");
}
