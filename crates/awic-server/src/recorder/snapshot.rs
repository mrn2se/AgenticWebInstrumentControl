//! `rec.snapshot` (SPECIFICATION.md §2.8.5): decompresses `[t0_s, t1_s)` of a
//! recording into an ordinary capture, carrying the recording's calibration
//! and channel list so `capture.measure`, decoders and export work on it
//! with no special case. `Recording::extract_packed`/`extract_analog`
//! already enforce the 64 Mi sample cap; this module just wires their output
//! into the `CaptureStore`.

use awic_core::{CaptureId, NewAnalogCapture, NewDigitalCapture};
use awic_proto::RecSnapshotResult;

use super::{RecOpError, RecorderManager, RecordingEntry, RecordingKindLocal};

pub fn snapshot(
    manager: &RecorderManager,
    entry: &RecordingEntry,
    t0_s: f64,
    t1_s: f64,
) -> Result<RecSnapshotResult, RecOpError> {
    let rate = entry.rate_hz.max(1) as f64;
    let start = (t0_s * rate).round().max(0.0) as u64;
    let end = (t1_s * rate).round().max(0.0) as u64;

    let capture_id = CaptureId::new();
    let channels: Vec<u8> = entry.channels.iter().map(|c| c.channel).collect();

    match entry.kind {
        RecordingKindLocal::Digital => {
            let packed = entry
                .recording
                .read()
                .unwrap()
                .extract_packed(start, end)
                .map_err(RecOpError::from)?;
            manager.capture_store.insert_digital(NewDigitalCapture {
                channels,
                id: capture_id,
                device: entry.device_id.clone(),
                rate_hz: entry.rate_hz,
                channel_count: entry.channels.len() as u8,
                packed,
                t0_ns: 0,
                trigger_position: None,
            });
        }
        RecordingKindLocal::Analog {
            channel_count,
            sample_bits,
        } => {
            let raw = entry
                .recording
                .read()
                .unwrap()
                .extract_analog(start, end)
                .map_err(RecOpError::from)?;
            let (channel_scale, channel_offset): (Vec<f32>, Vec<f32>) = entry
                .channels
                .iter()
                .map(|c| (c.calibration.scale, c.calibration.offset))
                .unzip();
            manager.capture_store.insert_analog(NewAnalogCapture {
                channels,
                id: capture_id,
                device: entry.device_id.clone(),
                rate_hz: entry.rate_hz,
                channel_count,
                sample_bits,
                channel_scale,
                channel_offset,
                raw,
                t0_ns: 0,
                trigger_position: None,
            });
        }
    }

    Ok(RecSnapshotResult {
        capture: capture_id.to_string(),
    })
}
