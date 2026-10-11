//! `capture.decode` on analog captures (GitHub issue #7): synthetic UART,
//! I²C and SPI waveforms with noise, cut into logic levels by
//! `awic_core::threshold` and decoded by the unchanged decoders.

use awic_core::{Capture, CaptureData, CaptureId, CaptureMeta, DeviceId};
use awic_proto::frame::Encoding;
use awic_proto::{
    ChannelMapping, ChannelRole, DecodeFormat, DecoderId, OptionSetting, OptionValue,
};
use awic_server::decode::{decode_capture, DecodeOpError, DecodeOutput, DecodeSpec};

const RATE_HZ: u64 = 1_000_000;
/// 8-bit codes over a 5 V range, as the oscilloscope reports them.
const SCALE: f32 = 10.0 / 255.0;
const OFFSET: f32 = -5.0;

fn meta(channels: Vec<u8>, encoding: Encoding) -> CaptureMeta {
    CaptureMeta {
        device: DeviceId::new(3, vec![1], 0, 0),
        rate_hz: RATE_HZ,
        channel_count: channels.len() as u8,
        channels,
        encoding,
        t0_ns: 0,
        trigger_position: None,
    }
}

/// Cheap deterministic noise in `[-1, 1]`.
fn noise(seed: &mut u32) -> f64 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    f64::from(*seed >> 8) / f64::from(1u32 << 23) - 1.0
}

/// Logic lines (one `Vec<bool>` per line, a level per sample) as an 8-bit
/// analog capture of hardware channels `channels`: `low`/`high` volts plus
/// `amp` volts of noise.
fn analog(lines: &[Vec<bool>], channels: Vec<u8>, low: f64, high: f64, amp: f64) -> Capture {
    let mut seed = 11;
    let n = lines.iter().map(Vec::len).min().unwrap();
    let mut raw = Vec::new();
    for i in 0..n {
        for line in lines {
            let v = if line[i] { high } else { low } + amp * noise(&mut seed);
            let code = ((v - f64::from(OFFSET)) / f64::from(SCALE)).round();
            raw.push(code.clamp(0.0, 255.0) as u8);
        }
    }
    Capture {
        id: CaptureId::new(),
        meta: meta(channels, Encoding::U8Interleaved),
        data: CaptureData::Analog {
            raw,
            sample_bits: 8,
            channel_scale: vec![SCALE; lines.len()],
            channel_offset: vec![OFFSET; lines.len()],
        },
    }
}

fn hold(line: &mut Vec<bool>, level: bool, samples: usize) {
    line.extend(std::iter::repeat_n(level, samples));
}

/// 8N1 at 50 kbaud (20 samples per bit), idle high.
fn uart(bytes: &[u8]) -> Vec<bool> {
    let mut line = Vec::new();
    hold(&mut line, true, 100);
    for &b in bytes {
        hold(&mut line, false, 20);
        for bit in 0..8 {
            hold(&mut line, b >> bit & 1 == 1, 20);
        }
        hold(&mut line, true, 40);
    }
    hold(&mut line, true, 100);
    line
}

/// An I²C write of `bytes` to `address`, every byte acknowledged; 10 samples
/// per quarter bit. Returns `(scl, sda)`.
fn i2c(address: u8, bytes: &[u8]) -> (Vec<bool>, Vec<bool>) {
    let (mut scl, mut sda) = (Vec::new(), Vec::new());
    let mut step = |c: bool, d: bool| {
        hold(&mut scl, c, 10);
        hold(&mut sda, d, 10);
    };
    step(true, true);
    step(true, true);
    step(true, false); // START
    step(false, false);
    let mut frame = vec![address << 1];
    frame.extend_from_slice(bytes);
    for byte in frame {
        for bit in (0..8).rev() {
            let d = byte >> bit & 1 == 1;
            step(false, d);
            step(true, d);
            step(false, d);
        }
        step(false, false); // ACK
        step(true, false);
        step(false, false);
    }
    step(false, false);
    step(true, false);
    step(true, true); // STOP
    step(true, true);
    (scl, sda)
}

/// SPI mode 0, MSB first, 20 samples per bit; returns `(clk, mosi, cs)`.
fn spi(words: &[u8]) -> (Vec<bool>, Vec<bool>, Vec<bool>) {
    let (mut clk, mut mosi, mut cs) = (Vec::new(), Vec::new(), Vec::new());
    let mut step = |c: bool, m: bool, s: bool| {
        hold(&mut clk, c, 10);
        hold(&mut mosi, m, 10);
        hold(&mut cs, s, 10);
    };
    step(false, false, true);
    step(false, false, false);
    for &w in words {
        for bit in (0..8).rev() {
            let m = w >> bit & 1 == 1;
            step(false, m, false);
            step(true, m, false);
        }
    }
    step(false, false, false);
    step(false, false, true);
    step(false, false, true);
    (clk, mosi, cs)
}

fn map(role: ChannelRole, channel: u8) -> ChannelMapping {
    ChannelMapping {
        role,
        channel,
        minus: None,
    }
}

fn opt(id: &str, value: OptionValue) -> OptionSetting {
    OptionSetting {
        id: id.to_string(),
        value,
    }
}

fn decode(
    capture: &Capture,
    decoder: DecoderId,
    channels: &[ChannelMapping],
    options: &[OptionSetting],
) -> Result<DecodeOutput, DecodeOpError> {
    decode_capture(
        capture,
        &DecodeSpec {
            decoder,
            channels,
            options,
            max_annotations: None,
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            fold: None,
        },
    )
}

#[test]
fn a_noisy_analog_uart_decodes_and_reports_its_threshold() {
    let capture = analog(&[uart(b"AWIC")], vec![0], 0.2, 3.1, 0.3);
    let out = decode(
        &capture,
        DecoderId::Uart,
        &[map(ChannelRole::Rx, 0)],
        &[opt("baud", OptionValue::Int(50_000))],
    )
    .unwrap();
    assert_eq!(out.transactions.len(), 1, "{out:?}");
    assert_eq!(out.transactions[0].text.as_deref(), Some("AWIC"));
    assert_eq!(out.transactions[0].notes, Vec::<String>::new());
    let note = out.note.expect("analog decodes say what they thresholded");
    assert!(note.starts_with("analog: CH1 threshold 1.6"), "{note}");
    assert!(note.contains("hysteresis 0.5"), "{note}");
}

#[test]
fn auto_baud_works_through_the_analog_front_end() {
    let capture = analog(&[uart(b"hello, world")], vec![0], 0.2, 3.1, 0.2);
    let out = decode(
        &capture,
        DecoderId::Uart,
        &[map(ChannelRole::Rx, 0)],
        &[opt("baud", OptionValue::Text("auto".into()))],
    )
    .unwrap();
    assert_eq!(out.detected_baud, Some(50_000));
    assert_eq!(out.transactions[0].text.as_deref(), Some("hello, world"));
}

#[test]
fn an_inverted_uart_decodes_with_the_invert_option() {
    let line: Vec<bool> = uart(b"Hi").iter().map(|b| !b).collect();
    let capture = analog(&[line], vec![0], 0.2, 3.1, 0.1);
    let out = decode(
        &capture,
        DecoderId::Uart,
        &[map(ChannelRole::Rx, 0)],
        &[
            opt("baud", OptionValue::Int(50_000)),
            opt("invert", OptionValue::Bool(true)),
        ],
    )
    .unwrap();
    assert_eq!(out.transactions[0].text.as_deref(), Some("Hi"));
}

#[test]
fn i2c_on_two_analog_channels_decodes_whatever_the_channel_numbers() {
    let (scl, sda) = i2c(0x3C, &[0x40, 0xA5]);
    // Hardware channels 1 and 2 of the capture: SCL on 2, SDA on 1.
    let capture = analog(&[sda, scl], vec![1, 2], 0.1, 3.3, 0.25);
    let out = decode(
        &capture,
        DecoderId::I2c,
        &[map(ChannelRole::Scl, 2), map(ChannelRole::Sda, 1)],
        &[],
    )
    .unwrap();
    assert_eq!(out.transactions.len(), 1, "{out:?}");
    let t = &out.transactions[0];
    assert_eq!(
        (t.address, t.read, t.acked),
        (Some(0x3C), Some(false), Some(true))
    );
    assert_eq!(t.data, "40 A5");
    let note = out.note.unwrap();
    assert!(
        note.contains("CH3 threshold") && note.contains("CH2 threshold"),
        "{note}"
    );
}

#[test]
fn spi_on_three_analog_channels_decodes() {
    let (clk, mosi, cs) = spi(&[0xA5, 0x3C]);
    let capture = analog(&[clk, mosi, cs], vec![0, 1, 2], 0.0, 3.3, 0.2);
    let out = decode(
        &capture,
        DecoderId::Spi,
        &[
            map(ChannelRole::Clk, 0),
            map(ChannelRole::Mosi, 1),
            map(ChannelRole::Cs, 2),
        ],
        &[],
    )
    .unwrap();
    assert_eq!(out.transactions.len(), 1, "{out:?}");
    assert_eq!(out.transactions[0].data, "A5 3C");
}

#[test]
fn a_differential_line_decodes_with_minus() {
    // UART on a pair: CH1 swings 2..3 V and CH2 the other way around the
    // same 2.5 V common mode; the difference is a clean +-1 V signal.
    let tx = uart(b"OK");
    let inverse: Vec<bool> = tx.iter().map(|b| !b).collect();
    let n = tx.len();
    let mut seed = 5;
    let mut raw = Vec::new();
    let code = |v: f64| ((v - f64::from(OFFSET)) / f64::from(SCALE)).round() as u8;
    for i in 0..n {
        let p = if tx[i] { 3.0 } else { 2.0 } + 0.05 * noise(&mut seed);
        let m = if inverse[i] { 3.0 } else { 2.0 } + 0.05 * noise(&mut seed);
        raw.push(code(p));
        raw.push(code(m));
    }
    let capture = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0, 1], Encoding::U8Interleaved),
        data: CaptureData::Analog {
            raw,
            sample_bits: 8,
            channel_scale: vec![SCALE; 2],
            channel_offset: vec![OFFSET; 2],
        },
    };
    let options = [opt("baud", OptionValue::Int(50_000))];
    let rx = ChannelMapping {
        role: ChannelRole::Rx,
        channel: 0,
        minus: Some(1),
    };
    let out = decode(&capture, DecoderId::Uart, &[rx], &options).unwrap();
    assert_eq!(out.transactions[0].text.as_deref(), Some("OK"));
    assert!(out.note.unwrap().contains("CH1-CH2 threshold"));
    // Naming a channel the capture lacks as `minus` is refused like any
    // other missing channel.
    let missing = ChannelMapping {
        minus: Some(5),
        ..rx
    };
    let err = decode(&capture, DecoderId::Uart, &[missing], &options).unwrap_err();
    assert_eq!(err.code, "channel_not_found");
    let same = ChannelMapping {
        minus: Some(0),
        ..rx
    };
    let err = decode(&capture, DecoderId::Uart, &[same], &options).unwrap_err();
    assert_eq!(err.code, "invalid_decoder_config");
}

#[test]
fn threshold_overrides_are_stripped_before_the_decoder_sees_them() {
    let capture = analog(&[uart(b"AWIC")], vec![0], 0.2, 3.1, 0.0);
    let options = [
        opt("baud", OptionValue::Int(50_000)),
        opt("threshold_mv", OptionValue::Int(1000)),
        opt("hysteresis_mv", OptionValue::Int(200)),
    ];
    let out = decode(
        &capture,
        DecoderId::Uart,
        &[map(ChannelRole::Rx, 0)],
        &options,
    )
    .unwrap();
    assert_eq!(out.transactions[0].text.as_deref(), Some("AWIC"));
    let note = out.note.unwrap();
    assert!(
        note.contains("threshold 1.000 V (given), hysteresis 0.200 V"),
        "{note}"
    );
    // A threshold above the signal never sees the line leave idle... which
    // here is the high level, so the whole capture reads as one idle line.
    let options = [
        opt("baud", OptionValue::Int(50_000)),
        opt("threshold_mv", OptionValue::Int(4000)),
    ];
    let out = decode(
        &capture,
        DecoderId::Uart,
        &[map(ChannelRole::Rx, 0)],
        &options,
    )
    .unwrap();
    assert!(out.transactions.is_empty());
    // I²C does not know the options either, and is not told.
    let (scl, sda) = i2c(0x50, &[1]);
    let capture = analog(&[scl, sda], vec![0, 1], 0.0, 3.3, 0.0);
    let options = [opt("threshold_mv", OptionValue::Int(1650))];
    let channels = [map(ChannelRole::Scl, 0), map(ChannelRole::Sda, 1)];
    assert_eq!(
        decode(&capture, DecoderId::I2c, &channels, &options)
            .unwrap()
            .transactions
            .len(),
        1
    );
}

#[test]
fn bad_threshold_options_are_named_errors() {
    let capture = analog(&[uart(b"A")], vec![0], 0.2, 3.1, 0.0);
    let rx = [map(ChannelRole::Rx, 0)];
    let text = [opt("threshold_mv", OptionValue::Text("auto".into()))];
    let err = decode(&capture, DecoderId::Uart, &rx, &text).unwrap_err();
    assert_eq!(err.code, "invalid_decoder_config");
    assert!(err.msg.contains("threshold_mv"), "{}", err.msg);
    let negative = [opt("hysteresis_mv", OptionValue::Int(-5))];
    assert!(decode(&capture, DecoderId::Uart, &rx, &negative).is_err());
}

#[test]
fn a_flat_channel_decodes_to_nothing_and_says_why() {
    let capture = analog(&[vec![false; 2000]], vec![0], 0.37, 0.37, 0.02);
    let out = decode(
        &capture,
        DecoderId::Uart,
        &[map(ChannelRole::Rx, 0)],
        &[opt("baud", OptionValue::Int(50_000))],
    )
    .unwrap();
    assert!(out.transactions.is_empty());
    assert!(out.note.unwrap().contains("CH1 flat at"));
}

#[test]
fn a_channel_the_analog_capture_lacks_is_refused() {
    let capture = analog(&[uart(b"A")], vec![0], 0.2, 3.1, 0.0);
    let err = decode(&capture, DecoderId::Uart, &[map(ChannelRole::Rx, 3)], &[]).unwrap_err();
    assert_eq!(err.code, "channel_not_found");
}

#[test]
fn minus_and_analog_options_are_refused_on_a_digital_capture() {
    let packed: Vec<u8> = uart(b"A").iter().map(|&b| u8::from(b)).collect();
    let capture = Capture {
        id: CaptureId::new(),
        meta: meta(vec![0], Encoding::PackedDigital),
        data: CaptureData::Digital {
            transitions: awic_core::capture_store::derive_transitions(&packed),
            packed,
        },
    };
    let rx = map(ChannelRole::Rx, 0);
    let ok = decode(&capture, DecoderId::Uart, &[rx], &[]).unwrap();
    assert_eq!(ok.note, None);
    let diff = ChannelMapping {
        minus: Some(1),
        ..rx
    };
    let err = decode(&capture, DecoderId::Uart, &[diff], &[]).unwrap_err();
    assert_eq!(err.code, "invalid_decoder_config");
    assert!(
        err.msg.contains("minus") && err.msg.contains("digital"),
        "{}",
        err.msg
    );
    for id in ["threshold_mv", "hysteresis_mv"] {
        let err = decode(
            &capture,
            DecoderId::Uart,
            &[rx],
            &[opt(id, OptionValue::Int(1000))],
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_decoder_config");
        assert!(
            err.msg.contains(id) && err.msg.contains("digital"),
            "{}",
            err.msg
        );
    }
}

#[test]
fn a_failed_auto_rate_names_the_hardware_line_not_its_bit() {
    // A few slow edges on CH2 only: too little for a rate. The message must
    // say CH2 (and the threshold), not "channel 0", the bit it was cut to.
    let line: Vec<bool> = (0..4000).map(|i| (i / 1000) % 2 == 1).collect();
    let flat = vec![false; 4000];
    let capture = analog(&[flat, line], vec![0, 1], 0.2, 3.1, 0.0);
    let err = decode(
        &capture,
        DecoderId::Uart,
        &[map(ChannelRole::Rx, 1)],
        &[opt("baud", OptionValue::Text("auto".into()))],
    )
    .unwrap_err();
    assert_eq!(err.code, "baud_not_detected");
    assert!(err.msg.contains("on CH2:"), "{}", err.msg);
    assert!(err.msg.contains("analog: CH2 threshold"), "{}", err.msg);
}
