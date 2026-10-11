//! Bounded, decimated single-channel reads of a stored capture
//! (SPECIFICATION.md §8.3): the agent-facing counterpart of
//! [`crate::measure_capture`] — same hardware-channel addressing (translated
//! once, exactly as `measure_capture` does), but instead of a scalar
//! statistic it returns at most `max_points` `(min, max)` buckets, capped
//! server-side so a bulk read can never leave through this path (§8.3's
//! "no tool returns bulk sample data").
//!
//! Digital channels reuse the same `(min, max)` shape as analog: a bucket's
//! AND becomes `min` (0.0 or 1.0) and its OR becomes `max`, so a bucket that
//! toggled reads as `(0.0, 1.0)` — the same "envelope" an analog glitch
//! gets from [`crate::minmax_decimate`]. This keeps `read_capture` one
//! shape for both capture kinds rather than two wire variants.

use crate::capture_store::{Capture, CaptureData};
use crate::decimate::{bitwise_decimate, minmax_decimate};
use crate::error::{CoreError, Result};

/// One channel of a capture, decimated to at most `max_points` buckets.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadCaptureResult {
    /// `"V"` for analog; empty for digital logic levels, matching
    /// [`crate::ChannelCalibration::unit`]'s convention.
    pub unit: String,
    /// Per-bucket minimum, same length as [`Self::max`].
    pub min: Vec<f32>,
    /// Per-bucket maximum, same length as [`Self::min`].
    pub max: Vec<f32>,
}

/// Decimates hardware `channel` of `capture` to at most `max_points`
/// `(min, max)` buckets. `channel` is a **hardware** channel number, exactly
/// as [`crate::measure_capture`] takes it — not a position in the capture's
/// interleave.
pub fn read_capture_channel(
    capture: &Capture,
    channel: u8,
    max_points: usize,
) -> Result<ReadCaptureResult> {
    read_capture_window(capture, channel, max_points, 0, usize::MAX)
}

/// [`read_capture_channel`] over samples `[start, end)` only (clamped to the
/// capture), for zooming into part of a capture without taking a new one.
pub fn read_capture_window(
    capture: &Capture,
    channel: u8,
    max_points: usize,
    start: usize,
    end: usize,
) -> Result<ReadCaptureResult> {
    let window = |len: usize| start.min(len)..end.min(len).max(start.min(len));
    match &capture.data {
        CaptureData::Analog {
            raw,
            sample_bits,
            channel_scale,
            channel_offset,
        } => {
            let acquired = &capture.meta.channels;
            let Some(position) = acquired.iter().position(|&c| c == channel) else {
                return Err(CoreError::ChannelNotFound {
                    capture: capture.id.to_string(),
                    channel,
                    valid: format!("this capture holds hardware channels {acquired:?}"),
                });
            };
            let stride = capture.meta.channel_count.max(1) as usize;
            let scale = channel_scale.get(position).copied().unwrap_or(1.0);
            let offset = channel_offset.get(position).copied().unwrap_or(0.0);

            let (min, max) = if *sample_bits <= 8 {
                let samples: Vec<u8> = raw.iter().skip(position).step_by(stride).copied().collect();
                let pairs = minmax_decimate(&samples[window(samples.len())], max_points);
                to_physical(pairs, scale, offset)
            } else {
                let all: Vec<i16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| i16::from_le_bytes(*c))
                    .collect();
                let samples: Vec<i16> = all.into_iter().skip(position).step_by(stride).collect();
                let pairs = minmax_decimate(&samples[window(samples.len())], max_points);
                to_physical(pairs, scale, offset)
            };
            Ok(ReadCaptureResult {
                unit: "V".into(),
                min,
                max,
            })
        }
        CaptureData::Digital { packed, .. } => {
            if channel > 7 {
                return Err(CoreError::ChannelNotFound {
                    capture: capture.id.to_string(),
                    channel,
                    valid: "digital channels are bits 0-7 of the packed byte".into(),
                });
            }
            let pairs = bitwise_decimate(&packed[window(packed.len())], max_points);
            let min = pairs.iter().map(|&(and, _)| bit(and, channel)).collect();
            let max = pairs.iter().map(|&(_, or)| bit(or, channel)).collect();
            Ok(ReadCaptureResult {
                unit: String::new(),
                min,
                max,
            })
        }
    }
}

fn bit(byte: u8, channel: u8) -> f32 {
    (((byte >> channel) & 1) != 0) as u8 as f32
}

fn to_physical<T>(pairs: Vec<(T, T)>, scale: f32, offset: f32) -> (Vec<f32>, Vec<f32>)
where
    T: Copy,
    f32: From<T>,
{
    pairs
        .into_iter()
        .map(|(lo, hi)| {
            (
                f32::from(lo) * scale + offset,
                f32::from(hi) * scale + offset,
            )
        })
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_store::{CaptureId, CaptureMeta};
    use crate::device_id::DeviceId;
    use awic_proto::frame::Encoding;

    fn dev() -> DeviceId {
        DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e)
    }

    fn analog_capture(
        channels: Vec<u8>,
        raw: Vec<u8>,
        scale: Vec<f32>,
        offset: Vec<f32>,
    ) -> Capture {
        let channel_count = channels.len() as u8;
        Capture {
            id: CaptureId::new(),
            meta: CaptureMeta {
                channels,
                device: dev(),
                rate_hz: 1_000,
                channel_count,
                encoding: Encoding::U8Interleaved,
                t0_ns: 0,
                trigger_position: None,
            },
            data: CaptureData::Analog {
                raw,
                sample_bits: 8,
                channel_scale: scale,
                channel_offset: offset,
            },
        }
    }

    fn digital_capture(packed: Vec<u8>) -> Capture {
        let transitions = crate::capture_store::derive_transitions(&packed);
        Capture {
            id: CaptureId::new(),
            meta: CaptureMeta {
                channels: (0..8).collect(),
                device: dev(),
                rate_hz: 1_000_000,
                channel_count: 8,
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

    #[test]
    fn extracts_only_the_requested_channel_from_an_interleaved_capture() {
        // Two channels interleaved: ch0 is always 0, ch1 has one spike to 200.
        let mut raw = Vec::new();
        for i in 0..50u8 {
            raw.push(0);
            raw.push(if i == 25 { 200 } else { 10 });
        }
        let capture = analog_capture(vec![0, 1], raw, vec![1.0, 1.0], vec![0.0, 0.0]);

        let ch0 = read_capture_channel(&capture, 0, 10).unwrap();
        assert!(
            ch0.max.iter().all(|&v| v == 0.0),
            "channel 0 must never see channel 1's spike: {:?}",
            ch0.max
        );

        let ch1 = read_capture_channel(&capture, 1, 10).unwrap();
        assert!(ch1.max.contains(&200.0));
    }

    #[test]
    fn applies_calibration_to_get_volts() {
        let capture = analog_capture(vec![0], vec![0, 128, 255], vec![0.04], vec![-5.0]);
        let result = read_capture_channel(&capture, 0, 10).unwrap();
        assert_eq!(result.unit, "V");
        assert!((result.min[0] - (-5.0)).abs() < 1e-4);
    }

    #[test]
    fn unknown_hardware_channel_is_a_named_error() {
        let capture = analog_capture(vec![0, 1], vec![0, 0], vec![1.0, 1.0], vec![0.0, 0.0]);
        let err = read_capture_channel(&capture, 5, 10).unwrap_err();
        assert!(matches!(err, CoreError::ChannelNotFound { .. }));
    }

    #[test]
    fn digital_toggled_bucket_reads_as_zero_to_one() {
        // D0 low, then high, within one bucket -> toggled.
        let packed = vec![0x00u8, 0x01];
        let result = read_capture_channel(&digital_capture(packed), 0, 1).unwrap();
        assert_eq!(result.unit, "");
        assert_eq!(result.min, vec![0.0]);
        assert_eq!(result.max, vec![1.0]);
    }

    #[test]
    fn digital_channel_above_seven_is_refused() {
        let err = read_capture_channel(&digital_capture(vec![0, 1]), 8, 10).unwrap_err();
        assert!(matches!(err, CoreError::ChannelNotFound { .. }));
    }

    #[test]
    fn a_window_reads_only_its_samples() {
        // D0 high only in samples 40..60 of 100.
        let packed: Vec<u8> = (0..100).map(|i| u8::from((40..60).contains(&i))).collect();
        let capture = digital_capture(packed);
        let inside = read_capture_window(&capture, 0, 4, 40, 60).unwrap();
        assert_eq!(inside.min, vec![1.0; 4]);
        let before = read_capture_window(&capture, 0, 4, 0, 40).unwrap();
        assert_eq!(before.max, vec![0.0; 4]);
        // Out-of-range bounds clamp instead of panicking.
        let past = read_capture_window(&capture, 0, 4, 90, 1_000).unwrap();
        assert_eq!(past.max.len(), 4);
    }

    #[test]
    fn caps_at_max_points() {
        let raw = vec![0u8; 1000];
        let capture = analog_capture(vec![0], raw, vec![1.0], vec![0.0]);
        let result = read_capture_channel(&capture, 0, 16).unwrap();
        assert!(result.min.len() <= 16);
        assert_eq!(result.min.len(), result.max.len());
    }
}
