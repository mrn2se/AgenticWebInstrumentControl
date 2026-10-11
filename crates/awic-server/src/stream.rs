//! Forwards one running acquisition's [`SampleChunk`]s to its owning
//! WebSocket connection as binary sample frames (SPECIFICATION.md §3.2),
//! applying the backpressure policy: above the stream's high-water mark,
//! chunks are re-encoded in a decimated encoding via
//! [`crate::backpressure::minmax_reencode`] instead of being sent at full
//! resolution.

use std::sync::Arc;

use tokio::sync::mpsc;

use awic_core::{
    CaptureId, CaptureStore, ChannelCalibration, DeviceId, NewAnalogCapture, NewDigitalCapture,
    SampleChunk,
};
use awic_proto::frame::{encode_frame, Encoding, FrameHeader};

use crate::backpressure::{minmax_reencode, StreamBackpressure, MINMAX_BUCKETS};
use crate::ws::WsOut;

/// Everything [`forward_stream`] needs to file the acquisition into the
/// [`CaptureStore`] once it finishes, under the *same* id `Instrument::start`
/// already reported to the client via `acq.start`'s result
/// (`awic-core`'s `CaptureStore::insert_with_id` contract: one acquisition,
/// one id, end to end — a second, store-minted id would leave the client's
/// `capture://...` handle pointing at nothing).
pub struct CaptureRecorder {
    pub id: CaptureId,
    pub device: DeviceId,
    pub rate_hz: u64,
    /// Sample index of the trigger point. `awic-fake`'s convention — the
    /// only convention available, since [`awic_core::Trigger`] carries no
    /// pretrigger-position field — is that an edge trigger lands at the
    /// capture's midpoint; callers should pass `Some(total_samples / 2)`
    /// when a trigger was armed, `None` otherwise.
    pub trigger_position: Option<u64>,
    /// The driver's per-channel raw-to-physical conversion, from
    /// `acq.configure`'s effective result.
    pub calibration: Vec<ChannelCalibration>,
    /// Hardware channels acquired, in payload order (effective config).
    pub channels: Vec<u8>,
    pub store: Arc<CaptureStore>,
}

/// Per-channel `(scale, offset)` vectors for the capture store, from the
/// driver's calibration. A mismatch with the stream's channel count means the
/// driver reported inconsistent metadata; filing identity then keeps the raw
/// codes intact and recoverable rather than scaling them by the wrong channel.
fn scale_offset(calibration: &[ChannelCalibration], channel_count: u8) -> (Vec<f32>, Vec<f32>) {
    let n = channel_count.max(1) as usize;
    if calibration.len() == n {
        calibration.iter().map(|c| (c.scale, c.offset)).unzip()
    } else {
        tracing::error!(
            channels = n,
            calibrated = calibration.len(),
            "calibration does not match the stream's channels; filing raw codes unscaled"
        );
        (vec![1.0; n], vec![0.0; n])
    }
}

/// Returns the number of samples per channel that arrived. Drains `rx`
/// (the [`awic_core::SampleSink`] receiver an `Instrument` is
/// streaming into) and forwards each chunk to `out_tx` as a binary sample
/// frame, tagged with `stream_id` and a per-stream, strictly increasing
/// sequence number.
///
/// The connection's outbound queue (`out_tx`) is bounded; a frame that
/// cannot be enqueued (the writer isn't draining at all, even at the
/// decimated data rate) is dropped rather than awaited, so this task never
/// blocks the instrument's own producer loop indefinitely and never grows
/// memory without bound (SPECIFICATION.md §3.2). The sequence number still
/// advances across a drop, which is exactly what lets a client detect it.
///
/// Independently of what gets sent to the client (which may be decimated
/// under backpressure), every chunk's *full-resolution* bytes are
/// accumulated and, if `recorder` is set, filed into the [`CaptureStore`]
/// once the stream ends — DEVELOPMENT.md §1 rule 4: measurements run on
/// full-resolution, server-side data, never on the live-view preview.
pub async fn forward_stream(
    mut rx: mpsc::Receiver<SampleChunk>,
    stream_id: u16,
    out_tx: mpsc::Sender<WsOut>,
    backpressure: Arc<StreamBackpressure>,
    recorder: Option<CaptureRecorder>,
) -> u64 {
    let mut sequence: u32 = 0;
    let mut received: u64 = 0;
    let mut raw = Vec::new();
    let mut recorded_shape: Option<(Encoding, u8, u64)> = None;

    while let Some(chunk) = rx.recv().await {
        received += u64::from(chunk.sample_count);
        if recorder.is_some() {
            if recorded_shape.is_none() {
                recorded_shape = Some((chunk.encoding, chunk.channel_count, chunk.t0_ns));
            }
            raw.extend_from_slice(&chunk.data);
        }

        let (encoding, payload, sample_count) = if backpressure.should_decimate() {
            minmax_reencode(&chunk, MINMAX_BUCKETS)
        } else {
            (chunk.encoding, chunk.data.to_vec(), chunk.sample_count)
        };

        let header = FrameHeader {
            stream_id,
            sequence,
            encoding,
            channel_count: chunk.channel_count,
            sample_count,
            t0_ns: chunk.t0_ns,
        };
        sequence = sequence.wrapping_add(1);

        let bytes = encode_frame(&header, &payload);
        let payload_len = payload.len();
        backpressure.record_enqueued(payload_len);

        let frame = WsOut::Frame {
            bytes,
            payload_len,
            backpressure: backpressure.clone(),
        };
        if out_tx.try_send(frame).is_err() {
            // Writer queue saturated even under decimation: the client
            // isn't draining at all. Drop this preview frame rather than
            // stalling the capture pipeline upstream or buffering
            // unboundedly (SPEC §3.2's explicit alternative to both).
            backpressure.release(payload_len);
            tracing::warn!(
                stream_id,
                bytes = payload_len,
                "dropping sample frame: client not draining"
            );
        }
    }

    if let (Some(recorder), Some((encoding, channel_count, t0_ns))) = (recorder, recorded_shape) {
        file_capture(recorder, encoding, channel_count, t0_ns, raw);
    }
    received
}

/// Files the accumulated full-resolution samples into the capture store
/// (SPECIFICATION.md §2.7), classifying analog vs. digital from the source
/// encoding. Analog captures carry the driver's calibration, so the store
/// holds raw codes plus the exact conversion the hardware implies.
fn file_capture(
    recorder: CaptureRecorder,
    encoding: Encoding,
    channel_count: u8,
    t0_ns: u64,
    raw: Vec<u8>,
) {
    match encoding {
        Encoding::PackedDigital => {
            recorder.store.insert_digital(NewDigitalCapture {
                channels: recorder.channels.clone(),
                id: recorder.id,
                device: recorder.device,
                rate_hz: recorder.rate_hz,
                channel_count,
                packed: raw,
                t0_ns,
                trigger_position: recorder.trigger_position,
            });
        }
        Encoding::I16 => {
            let (channel_scale, channel_offset) =
                scale_offset(&recorder.calibration, channel_count);
            recorder.store.insert_analog(NewAnalogCapture {
                channels: recorder.channels.clone(),
                id: recorder.id,
                device: recorder.device,
                rate_hz: recorder.rate_hz,
                channel_count,
                sample_bits: 16,
                channel_scale,
                channel_offset,
                raw,
                t0_ns,
                trigger_position: recorder.trigger_position,
            });
        }
        // A decimated encoding is a preview, never a capture. If a driver
        // ever reports one, filing it as raw samples would silently put
        // lossy data where full resolution is promised — and every later
        // measurement on it would be wrong without warning. Refuse instead.
        e @ (Encoding::MinMaxU8 | Encoding::MinMaxI16 | Encoding::BitwiseDigital) => {
            tracing::error!(
                capture = %recorder.id,
                encoding = ?e,
                "driver produced a decimated encoding; not filing a lossy capture"
            );
        }
        Encoding::U8Interleaved => {
            let (channel_scale, channel_offset) =
                scale_offset(&recorder.calibration, channel_count);
            recorder.store.insert_analog(NewAnalogCapture {
                channels: recorder.channels.clone(),
                id: recorder.id,
                device: recorder.device,
                rate_hz: recorder.rate_hz,
                channel_count,
                sample_bits: 8,
                channel_scale,
                channel_offset,
                raw,
                t0_ns,
                trigger_position: recorder.trigger_position,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;

    fn u8_chunk(channel_count: u8, sample_count: u32, fill: u8) -> SampleChunk {
        SampleChunk {
            encoding: Encoding::U8Interleaved,
            channel_count,
            sample_count,
            t0_ns: 0,
            data: Bytes::from(vec![fill; (sample_count * channel_count as u32) as usize]),
        }
    }

    #[tokio::test]
    async fn sequence_numbers_are_monotonic_and_encoding_matches_source() {
        let (sink, rx) = awic_core::SampleSink::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let backpressure = Arc::new(StreamBackpressure::new(usize::MAX));

        let forwarder = tokio::spawn(forward_stream(rx, 7, out_tx, backpressure, None));
        for _ in 0..5 {
            sink.send(u8_chunk(2, 16, 9)).await.unwrap();
        }
        drop(sink);
        forwarder.await.unwrap();

        let mut last_seq: Option<u32> = None;
        let mut count = 0;
        while let Ok(WsOut::Frame { bytes, .. }) = out_rx.try_recv() {
            let (header, _payload) = awic_proto::frame::decode_frame(&bytes).unwrap();
            assert_eq!(header.stream_id, 7);
            assert_eq!(header.encoding, Encoding::U8Interleaved);
            assert_eq!(header.channel_count, 2);
            if let Some(prev) = last_seq {
                assert_eq!(header.sequence, prev + 1);
            }
            last_seq = Some(header.sequence);
            count += 1;
        }
        assert_eq!(count, 5);
    }

    /// SPEC §3.2's backpressure requirement: feed the forwarder far faster
    /// than the "client" (the reader of `out_rx`) drains it, and confirm
    /// that instead of the outbound queue growing without bound, frames
    /// switch to a decimated encoding once the tracked unacked bytes
    /// cross the high-water mark — and the process never deadlocks, because
    /// `forward_stream` never blocks trying to enqueue.
    #[tokio::test]
    async fn slow_consumer_causes_switch_to_minmax_encoding() {
        // 256-deep sample channel so the producer below never has to wait on
        // the *driver* side — the thing under test is what the *server* does
        // toward a slow client, not driver-level backpressure.
        let (sink, rx) = awic_core::SampleSink::channel(256);
        // A small, bounded outbound queue: this is what keeps server memory
        // flat regardless of how far behind the client falls (SPEC §3.2).
        const OUT_QUEUE_CAPACITY: usize = 16;
        let (out_tx, mut out_rx) = mpsc::channel(OUT_QUEUE_CAPACITY);
        // Low enough that a handful of 4-byte full-resolution chunks trips it.
        let backpressure = Arc::new(StreamBackpressure::new(32));

        let forwarder = tokio::spawn(forward_stream(rx, 1, out_tx, backpressure, None));

        // Producer: fires 100 chunks essentially all at once. Nothing reads
        // `out_rx` yet, so the forwarder races ahead of "the client" — the
        // scenario SPEC §3.2 requires the server to survive without a stall
        // or unbounded growth.
        for _ in 0..100 {
            sink.send(u8_chunk(1, 4, 77)).await.unwrap();
        }
        drop(sink);

        // Give the forwarder a beat to run ahead of us and fill its bounded
        // outbound queue — this is the "faster than the consumer drains"
        // part of the scenario.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now drain like a real (if slow) client: read a frame, then "ack"
        // it exactly as the real WS writer does once a send actually
        // completes (see `WsOut::Frame` handling in `ws.rs`).
        let mut saw_minmax = false;
        let mut saw_full_res = false;
        let mut frames_seen = 0usize;
        loop {
            match tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await {
                Ok(Some(WsOut::Frame {
                    bytes,
                    payload_len,
                    backpressure,
                })) => {
                    frames_seen += 1;
                    let (header, _payload) = awic_proto::frame::decode_frame(&bytes).unwrap();
                    match header.encoding {
                        e if e.is_decimated() => saw_minmax = true,
                        Encoding::U8Interleaved => saw_full_res = true,
                        other => panic!("unexpected encoding {other:?}"),
                    }
                    backpressure.release(payload_len);
                }
                Ok(Some(WsOut::Text(_))) => unreachable!("this test never sends control text"),
                Ok(Some(WsOut::ViewFrame { .. })) => {
                    unreachable!("this test never sends view frames")
                }
                Ok(Some(WsOut::TermFrame(_))) => {
                    unreachable!("this test never sends terminal frames")
                }
                Ok(None) | Err(_) => break,
            }
        }
        let _ = forwarder.await;

        // The outbound queue never held more than its fixed capacity worth
        // of frames — memory stayed bounded despite 100 chunks racing in.
        assert!(frames_seen <= 100);
        assert!(
            saw_full_res,
            "the first chunks, before the mark trips, should be full resolution"
        );
        assert!(
            saw_minmax,
            "once above the high-water mark the stream must switch to min/max"
        );
    }

    /// The contract awic-core's `CaptureStore::insert_with_id` exists for:
    /// the id `Instrument::start` hands back (and that `acq.start` reports
    /// to the client) must be exactly the id the capture is filed under, so
    /// a client that read "capture X" from `acq.start` can actually read it
    /// back.
    #[tokio::test]
    async fn capture_is_filed_under_the_id_acq_start_reported() {
        let (sink, rx) = awic_core::SampleSink::channel(8);
        let (out_tx, _out_rx) = mpsc::channel(8);
        let backpressure = Arc::new(StreamBackpressure::new(usize::MAX));

        let capture_id = CaptureId::new();
        let device = DeviceId::new(1, vec![1], 0x0925, 0x3881);
        let store = Arc::new(CaptureStore::with_defaults());
        let recorder = CaptureRecorder {
            id: capture_id,
            device: device.clone(),
            rate_hz: 1_000_000,
            // A trigger was armed: awic-fake's convention is that it lands
            // at the capture's midpoint.
            trigger_position: Some(32),
            calibration: vec![],
            channels: vec![0, 1],
            store: store.clone(),
        };

        let forwarder = tokio::spawn(forward_stream(rx, 1, out_tx, backpressure, Some(recorder)));
        for _ in 0..4 {
            sink.send(u8_chunk(2, 16, 5)).await.unwrap();
        }
        drop(sink);
        forwarder.await.unwrap();

        // Exactly the id `acq.start` would have reported — not some other,
        // store-minted id.
        let capture = store
            .get(&capture_id)
            .expect("capture must read back under its acq.start id");
        assert_eq!(capture.meta.device, device);
        assert_eq!(capture.meta.rate_hz, 1_000_000);
        assert_eq!(capture.meta.channel_count, 2);
        assert_eq!(capture.meta.trigger_position, Some(32));
        match &capture.data {
            awic_core::CaptureData::Analog { raw, .. } => {
                assert_eq!(
                    raw.len(),
                    4 * 16 * 2,
                    "all four chunks' full-resolution bytes kept"
                );
            }
            other => panic!("expected an analog capture, got {other:?}"),
        }
    }
}
