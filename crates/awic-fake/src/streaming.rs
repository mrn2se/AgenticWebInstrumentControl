//! Chunked, paced sample streaming shared by the analog and digital
//! instruments (SPECIFICATION.md §2.6, §3.2).
//!
//! Real hardware delivers samples continuously over bulk USB transfers, not
//! as one instant dump; a fake backend that just handed the whole capture to
//! the sink in one `SampleChunk` would never exercise a consumer's
//! backpressure handling. So this streams in bounded chunks and paces
//! between them to roughly track wall-clock time at the configured rate,
//! using `tokio::time::sleep` (cheap: one timer wakeup per chunk, not a busy
//! loop) and correcting for drift against a single start `Instant` rather
//! than accumulating per-chunk rounding error.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awic_core::{SampleChunk, SampleSink};
use awic_proto::frame::Encoding;
use bytes::Bytes;
use tokio::time::Instant;

/// Roughly how many chunks per second of streaming we aim for: fine enough
/// to exercise backpressure, coarse enough not to spend all our time on
/// timer wakeups and allocations.
const CHUNKS_PER_SECOND: u64 = 50;
const MIN_CHUNK_SAMPLES: usize = 256;
const MAX_CHUNK_SAMPLES: usize = 65_536;

fn chunk_len(rate_hz: u64, total_samples: u64) -> usize {
    let target = (rate_hz / CHUNKS_PER_SECOND).max(1) as usize;
    target
        .clamp(MIN_CHUNK_SAMPLES, MAX_CHUNK_SAMPLES)
        .min(total_samples.max(1) as usize)
}

/// Bytes needed per sample for a given encoding/channel-count combination —
/// only the two encodings this crate ever produces.
fn bytes_per_sample(encoding: Encoding, channel_count: u8) -> usize {
    match encoding {
        Encoding::U8Interleaved => channel_count.max(1) as usize,
        Encoding::PackedDigital => 1,
        other => panic!("awic-fake never produces {other:?} chunks"),
    }
}

/// What to stream — bundled so `stream_chunks` stays under clippy's
/// too-many-arguments limit as continuous acquisition (SPECIFICATION.md
/// §2.3.2) adds a field alongside the encoding and pacing it already needed.
pub(crate) struct StreamSpec {
    pub encoding: Encoding,
    pub channel_count: u8,
    /// Samples (per channel) to stream. Ignored when `continuous` is true.
    pub total_samples: u64,
    pub rate_hz: u64,
    /// SPECIFICATION.md §2.3.2: stream until `cancel` is set instead of
    /// stopping after `total_samples` — mirrors the native drivers, which
    /// stream until `Instrument::stop()` rather than for a fixed count.
    pub continuous: bool,
}

/// Streams `spec.total_samples` samples (per channel) into `sink`, in
/// chunks, pacing to roughly match `spec.rate_hz`. `emit_sample(n, buf)`
/// appends the raw bytes for output sample index `n` (0-based, *not* yet
/// trigger-shifted — callers bake any shift into their closure) to `buf`.
///
/// Stops early, without error, if `cancel` is set (SPECIFICATION.md's
/// `Instrument::stop`) or the sink's receiver has gone away.
pub(crate) async fn stream_chunks(
    sink: &SampleSink,
    spec: StreamSpec,
    cancel: &AtomicBool,
    mut emit_sample: impl FnMut(i64, &mut Vec<u8>),
) {
    let StreamSpec {
        encoding,
        channel_count,
        total_samples,
        rate_hz,
        continuous,
    } = spec;
    let len = chunk_len(rate_hz, if continuous { u64::MAX } else { total_samples });
    let bps = bytes_per_sample(encoding, channel_count);
    let start = Instant::now();
    let mut sent: u64 = 0;

    while continuous || sent < total_samples {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let this_len = if continuous {
            len
        } else {
            len.min((total_samples - sent) as usize)
        };
        let mut buf = Vec::with_capacity(this_len * bps);
        for i in 0..this_len {
            emit_sample(sent as i64 + i as i64, &mut buf);
        }

        let t0_ns = (u128::from(sent) * 1_000_000_000u128 / u128::from(rate_hz.max(1))) as u64;
        let chunk = SampleChunk {
            encoding,
            channel_count,
            sample_count: this_len as u32,
            t0_ns,
            data: Bytes::from(buf),
        };
        if sink.send(chunk).await.is_err() {
            // Receiver dropped: capture was torn down downstream.
            break;
        }
        sent += this_len as u64;

        let expected = Duration::from_secs_f64(sent as f64 / rate_hz.max(1) as f64);
        let actual = start.elapsed();
        if let Some(remaining) = expected.checked_sub(actual) {
            if remaining > Duration::from_millis(1) {
                tokio::time::sleep(remaining).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    #[tokio::test]
    async fn streams_exactly_the_requested_sample_count() {
        let (sink, mut rx) = SampleSink::channel(64);
        let cancel = AtomicBool::new(false);
        let total = 10_000u64;
        let rate = 1_000_000u64;

        let handle = tokio::spawn(async move {
            stream_chunks(
                &sink,
                StreamSpec {
                    encoding: Encoding::U8Interleaved,
                    channel_count: 1,
                    total_samples: total,
                    rate_hz: rate,
                    continuous: false,
                },
                &cancel,
                |n, buf| {
                    buf.push((n % 256) as u8);
                },
            )
            .await;
        });

        let mut received = 0u64;
        while let Some(chunk) = rx.recv().await {
            received += chunk.sample_count as u64;
        }
        handle.await.unwrap();
        assert_eq!(received, total);
    }

    #[tokio::test]
    async fn cancel_flag_stops_streaming_early() {
        let (sink, mut rx) = SampleSink::channel(1);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_task = cancel.clone();
        // A slow rate so the loop has time to observe the cancel flag
        // between chunks instead of finishing before we ever set it.
        let total = 1_000_000u64;
        let rate = 1_000u64;

        let handle = tokio::spawn(async move {
            stream_chunks(
                &sink,
                StreamSpec {
                    encoding: Encoding::U8Interleaved,
                    channel_count: 1,
                    total_samples: total,
                    rate_hz: rate,
                    continuous: false,
                },
                &cancel_task,
                |_, buf| {
                    buf.push(0);
                },
            )
            .await;
        });

        // Let one chunk through, then cancel.
        let _ = rx.recv().await;
        cancel.store(true, Ordering::Relaxed);
        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .expect("stream_chunks did not stop promptly after cancel")
            .unwrap();
    }

    #[test]
    fn chunk_len_stays_within_bounds() {
        assert_eq!(chunk_len(1_000_000, 10), 10);
        assert!(chunk_len(48_000_000, u64::MAX) <= MAX_CHUNK_SAMPLES);
        assert_eq!(chunk_len(10, u64::MAX), MIN_CHUNK_SAMPLES);
    }

    /// SPECIFICATION.md §2.3.2: `continuous` ignores `total_samples` (0 is
    /// valid) and streams until `cancel` is set, rather than stopping after
    /// a fixed count.
    #[tokio::test]
    async fn continuous_ignores_total_samples_and_streams_until_cancelled() {
        let (sink, mut rx) = SampleSink::channel(1);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_task = cancel.clone();
        let rate = 1_000u64;

        let handle = tokio::spawn(async move {
            stream_chunks(
                &sink,
                StreamSpec {
                    encoding: Encoding::U8Interleaved,
                    channel_count: 1,
                    total_samples: 0, // ignored when continuous.
                    rate_hz: rate,
                    continuous: true,
                },
                &cancel_task,
                |_, buf| {
                    buf.push(0);
                },
            )
            .await;
        });

        // Several chunks arrive well past what `total_samples: 0` would
        // ever have produced if it were honoured.
        let mut received = 0u64;
        for _ in 0..3 {
            let chunk = rx.recv().await.expect("stream ended before cancel");
            received += chunk.sample_count as u64;
        }
        assert!(received > 0, "continuous streaming produced no samples");

        cancel.store(true, Ordering::Relaxed);
        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .expect("stream_chunks did not stop promptly after cancel")
            .unwrap();
    }
}
