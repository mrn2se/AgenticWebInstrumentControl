//! The decoder against a real recording: a CANable2 as the only node on a
//! 500 kbit/s bus, scoped on the oscilloscope at 16 MS/s (testdata/bench, the
//! `can_500k_noack_scope` fixture). Nobody acknowledges, so the CANable2
//! sends ID 0x123, DLC 8, `DE AD BE EF 01 02 03 04` over and over.
//!
//! The recording is analog — u8 codes interleaved CH1/CH2 — and the bus only
//! swings about four codes, so it is thresholded here by hand (the line idles
//! high at code ~141, dominant is ~137) into the transitions a decoder takes.

use awic_decode::can::{CanConfig, CanDecoder, Polarity};
use awic_decode::{Annotation, AnnotationKind, Decoder, Transition, LEVEL_FRAME};

/// Midway between the dominant (~137) and recessive (~141..142) codes.
const THRESHOLD: f64 = 139.5;
const RATE_HZ: u64 = 16_000_000;
/// Complete frames in the 32768-sample window: the capture opens in the
/// middle of a frame, and seven more follow, a period being ~131.7 bits
/// (about 4220 samples) and the last one ending before the capture does.
const FRAMES_IN_FIXTURE: usize = 7;
const DATA: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];

fn bench_transitions() -> (Vec<Transition>, u64) {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/bench/can_500k_noack_scope.bin"
    );
    let raw = std::fs::read(path).expect("bench fixture");
    let ch0: Vec<u8> = raw.iter().step_by(2).copied().collect();
    let mut transitions = Vec::new();
    let mut prev = None;
    for (i, &code) in ch0.iter().enumerate() {
        let level = f64::from(code) >= THRESHOLD;
        if prev != Some(level) {
            transitions.push(Transition {
                sample_index: i as u64,
                value: u8::from(level),
            });
            prev = Some(level);
        }
    }
    (transitions, ch0.len() as u64)
}

/// Decodes the fixture at `sample_point` percent and returns the frame
/// annotations and everything else at frame level.
fn decode(sample_point: u8) -> (Vec<Annotation>, Vec<Annotation>) {
    let (transitions, samples) = bench_transitions();
    let mut decoder = CanDecoder::new(CanConfig {
        bit: 0,
        sample_rate_hz: RATE_HZ,
        bitrate: 500_000,
        sample_point,
        polarity: Polarity::DominantLow,
    })
    .unwrap();
    // The capture opens in the middle of a frame.
    decoder.seed(transitions[0].value);
    let mut annotations = decoder.feed(&transitions, samples);
    annotations.extend(decoder.finish(samples));
    // That opening tail is reported as such, and only it.
    let opening = annotations.remove(0);
    assert_eq!(
        (opening.start_sample, opening.level, &opening.kind),
        (0, LEVEL_FRAME, &AnnotationKind::Incomplete),
        "{opening:?}"
    );
    assert!(opening
        .text
        .starts_with("bus activity before the first idle"));
    annotations
        .into_iter()
        .filter(|a| a.level == LEVEL_FRAME)
        .partition(|a| matches!(a.kind, AnnotationKind::CanFrame { .. }))
}

fn frame_fields(a: &Annotation) -> (u32, bool, bool, u8, &[u8], bool, bool) {
    let AnnotationKind::CanFrame {
        id,
        extended,
        remote,
        dlc,
        data,
        crc_ok,
        acked,
    } = &a.kind
    else {
        unreachable!()
    };
    (*id, *extended, *remote, *dlc, data, *crc_ok, *acked)
}

/// Sampled mid-bit, every frame decodes: ID 0x123, DLC 8, the bench data, a
/// good CRC, and an ACK slot nobody pulled dominant.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn decodes_every_unacknowledged_retransmission() {
    let (frames, others) = decode(50);
    assert_eq!(frames.len(), FRAMES_IN_FIXTURE);
    for f in &frames {
        let (id, extended, remote, dlc, data, crc_ok, acked) = frame_fields(f);
        assert_eq!(
            (id, extended, remote, dlc),
            (0x123, false, false, 8),
            "{f:?}"
        );
        assert_eq!(data, DATA);
        assert!(crc_ok && !acked, "{f:?}");
        assert_eq!(f.text, "0x123 [8] DE AD BE EF 01 02 03 04 no ACK");
    }
    assert!(others.is_empty(), "{others:#?}");
}

/// At the default 75 % sample point two of the seven frames are caught by
/// timing slips in the recording itself: in one, the line comes back to
/// recessive 0.3 bit (about 10 samples) before the end of a five-bit dominant
/// run and everything after it is that much early; in another a dominant bit
/// is half as long as it should be. Both are more than the quarter bit
/// between sample point and bit end. The decoder must not hand such a frame
/// over as good: it reports a CRC error, never a wrong payload with a good CRC.
#[test]
#[ignore = "needs testdata/bench, which is not in the repository"]
fn a_timing_slip_in_the_recording_is_a_crc_error_not_bad_data() {
    let (frames, others) = decode(75);
    assert_eq!(frames.len(), FRAMES_IN_FIXTURE);
    let good = frames
        .iter()
        .filter(|f| {
            let (id, _, _, dlc, data, crc_ok, acked) = frame_fields(f);
            if crc_ok {
                assert_eq!(
                    (id, dlc, data, acked),
                    (0x123, 8, &DATA[..], false),
                    "{f:?}"
                );
            }
            crc_ok
        })
        .count();
    assert_eq!(good, 5);
    assert!(others.is_empty(), "{others:#?}");
}
