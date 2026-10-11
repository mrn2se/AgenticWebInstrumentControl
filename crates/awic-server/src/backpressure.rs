//! Per-stream backpressure and the min/max re-encoding it switches to
//! (SPECIFICATION.md §3.2).
//!
//! The wire protocol has no explicit ack frame, so "unacked bytes" is
//! operationalized here as *bytes handed to this connection's writer but not
//! yet confirmed sent* — see [`StreamBackpressure`]. [`crate::stream`] is the
//! only caller: it decides, per outgoing chunk, whether to send at full
//! resolution or call [`minmax_reencode`] first.

use std::sync::atomic::{AtomicUsize, Ordering};

use awic_core::{bitwise_decimate, minmax_decimate, SampleChunk};
use awic_proto::frame::Encoding;

/// Number of `(min, max)` buckets produced per channel once a stream has
/// switched to the decimated encoding — a "screen columns" default. SPEC
/// §3.2 does not pin an exact figure; this is a documented implementation
/// choice, not a value derived from the spec.
pub const MINMAX_BUCKETS: usize = 512;

/// Bytes of enqueued-but-unacked payload above which a stream switches to
/// a decimated encoding (SPEC §3.2). Not specified numerically in the
/// spec; 1 MiB is small relative to a single full-resolution capture chunk
/// but large enough not to trip on a momentary scheduling hiccup. Documented
/// implementation choice.
pub const DEFAULT_HIGH_WATER_BYTES: usize = 1 << 20;

/// Tracks one stream's outstanding (enqueued but not yet flushed) payload
/// bytes and decides, from that alone, whether the stream should be sent at
/// full resolution or decimated (SPEC §3.2).
///
/// There is no transport-level ack in this protocol, so "unacked" means: the
/// byte count handed to [`Self::record_enqueued`] since connection start,
/// minus what [`Self::release`] has reported as flushed (or dropped — see
/// [`crate::stream::forward_stream`]) since. That is the best available
/// proxy for "how much data is the client's read side behind by," and it is
/// exactly the quantity SPEC §3.2 asks the server to track.
#[derive(Debug)]
pub struct StreamBackpressure {
    unacked_bytes: AtomicUsize,
    high_water_mark: usize,
}

impl StreamBackpressure {
    pub fn new(high_water_mark: usize) -> Self {
        Self {
            unacked_bytes: AtomicUsize::new(0),
            high_water_mark,
        }
    }

    pub fn with_default_high_water_mark() -> Self {
        Self::new(DEFAULT_HIGH_WATER_BYTES)
    }

    /// Call when `bytes` of sample payload have been handed to the
    /// connection's writer.
    pub fn record_enqueued(&self, bytes: usize) {
        self.unacked_bytes.fetch_add(bytes, Ordering::SeqCst);
    }

    /// Call once those `bytes` are no longer outstanding: either the writer
    /// actually flushed them to the socket, or they were dropped instead of
    /// being sent at all (SPEC §3.2 forbids unbounded buffering, not a
    /// bounded drop of an already-decimated preview frame as a last
    /// resort — see [`crate::stream::forward_stream`]).
    pub fn release(&self, bytes: usize) {
        // Saturating: a stream that never enqueued anything (or already
        // released it) must not underflow the counter.
        self.unacked_bytes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                Some(cur.saturating_sub(bytes))
            })
            .ok();
    }

    pub fn unacked(&self) -> usize {
        self.unacked_bytes.load(Ordering::SeqCst)
    }

    /// Whether the stream is currently above its high-water mark and should
    /// be sent decimated rather than full resolution.
    pub fn should_decimate(&self) -> bool {
        self.unacked() >= self.high_water_mark
    }
}

/// Re-encodes `chunk` in its decimated encoding (`MinMaxU8`, `MinMaxI16`
/// or `BitwiseDigital`, chosen by the source encoding): for each channel (in
/// order), up to `buckets` `(min, max)` pairs covering that channel's
/// samples in the chunk, sample width matching the source encoding (1 byte
/// for `U8Interleaved`/`PackedDigital`, little-endian `i16` for `I16`).
///
/// This channel-major payload layout is this implementation's choice: SPEC
/// §3.2 fixes the 24-byte frame header precisely but does not specify the
/// internal byte layout of encoding=3's payload for multi-channel data.
///
/// Returns the encoded payload and the number of buckets actually produced
/// per channel (the frame header's `sample_count`, post-decimation).
pub fn minmax_reencode(chunk: &SampleChunk, buckets: usize) -> (Encoding, Vec<u8>, u32) {
    match chunk.encoding {
        // Already decimated: pass through unchanged, keeping its encoding.
        e @ (Encoding::MinMaxU8 | Encoding::MinMaxI16 | Encoding::BitwiseDigital) => {
            (e, chunk.data.to_vec(), chunk.sample_count)
        }
        Encoding::PackedDigital => {
            // One byte per sample packs every digital channel into a bitmask.
            // Bitmasks are not numbers: decimate with AND/OR per bucket, which
            // keeps each channel's level and preserves glitches. Numeric
            // min/max here would corrupt every channel (see bitwise_decimate).
            let pairs = bitwise_decimate(chunk.data.as_ref(), buckets);
            let mut out = Vec::with_capacity(pairs.len() * 2);
            for (lo, hi) in &pairs {
                out.push(*lo);
                out.push(*hi);
            }
            (Encoding::BitwiseDigital, out, pairs.len() as u32)
        }
        Encoding::I16 => {
            let channels = chunk.channel_count.max(1) as usize;
            let samples: Vec<i16> = chunk
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes(*c))
                .collect();
            let mut out = Vec::new();
            let mut produced = 0usize;
            for ch in 0..channels {
                let channel_samples: Vec<i16> =
                    samples.iter().copied().skip(ch).step_by(channels).collect();
                let pairs = minmax_decimate(&channel_samples, buckets);
                produced = produced.max(pairs.len());
                for (lo, hi) in pairs {
                    out.extend_from_slice(&lo.to_le_bytes());
                    out.extend_from_slice(&hi.to_le_bytes());
                }
            }
            (Encoding::MinMaxI16, out, produced as u32)
        }
        Encoding::U8Interleaved => {
            let channels = chunk.channel_count.max(1) as usize;
            let mut out = Vec::new();
            let mut produced = 0usize;
            for ch in 0..channels {
                let channel_samples: Vec<u8> = chunk
                    .data
                    .iter()
                    .copied()
                    .skip(ch)
                    .step_by(channels)
                    .collect();
                let pairs = minmax_decimate(&channel_samples, buckets);
                produced = produced.max(pairs.len());
                for (lo, hi) in pairs {
                    out.push(lo);
                    out.push(hi);
                }
            }
            (Encoding::MinMaxU8, out, produced as u32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn starts_below_high_water_mark() {
        let bp = StreamBackpressure::new(100);
        assert!(!bp.should_decimate());
    }

    #[test]
    fn crosses_high_water_mark_once_enough_bytes_are_enqueued() {
        let bp = StreamBackpressure::new(100);
        bp.record_enqueued(50);
        assert!(!bp.should_decimate());
        bp.record_enqueued(50);
        assert!(bp.should_decimate());
    }

    #[test]
    fn releasing_bytes_drops_back_below_the_mark() {
        let bp = StreamBackpressure::new(100);
        bp.record_enqueued(150);
        assert!(bp.should_decimate());
        bp.release(100);
        assert!(!bp.should_decimate());
        assert_eq!(bp.unacked(), 50);
    }

    #[test]
    fn release_never_underflows() {
        let bp = StreamBackpressure::new(100);
        bp.release(10);
        assert_eq!(bp.unacked(), 0);
    }

    fn chunk(
        encoding: Encoding,
        channel_count: u8,
        data: Vec<u8>,
        sample_count: u32,
    ) -> SampleChunk {
        SampleChunk {
            encoding,
            channel_count,
            sample_count,
            t0_ns: 0,
            data: Bytes::from(data),
        }
    }

    #[test]
    fn minmax_reencode_u8_preserves_a_spike_per_channel() {
        // Two interleaved channels, 8 samples each. Channel 0 has a spike.
        let mut interleaved = Vec::new();
        for i in 0..8u8 {
            let ch0 = if i == 5 { 255 } else { 10 };
            interleaved.push(ch0);
            interleaved.push(20); // channel 1 flat
        }
        let c = chunk(Encoding::U8Interleaved, 2, interleaved, 8);
        let (encoding, payload, buckets) = minmax_reencode(&c, 4);
        assert_eq!(encoding, Encoding::MinMaxU8);
        // 2 channels * 4 buckets * 2 bytes (min,max) = 16 bytes.
        assert_eq!(payload.len(), 16);
        assert_eq!(buckets, 4);
        // Channel 0's pairs occupy the first 8 bytes; one of the highs must be 255.
        assert!(payload[0..8].contains(&255));
        // Channel 1 is flat at 20 throughout.
        assert!(payload[8..16].iter().all(|&b| b == 20));
    }

    #[test]
    fn minmax_reencode_i16_round_trips_extremes() {
        let mut data = Vec::new();
        for v in [-100i16, 5, 5, 5, 5, 5, 5, 300] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let c = chunk(Encoding::I16, 1, data, 8);
        let (encoding, payload, buckets) = minmax_reencode(&c, 2);
        assert_eq!(encoding, Encoding::MinMaxI16);
        assert_eq!(buckets, 2);
        assert_eq!(payload.len(), 2 * 4); // 2 buckets * (min i16 + max i16)
        let lo0 = i16::from_le_bytes([payload[0], payload[1]]);
        let hi0 = i16::from_le_bytes([payload[2], payload[3]]);
        assert_eq!((lo0, hi0), (-100, 5));
        let lo1 = i16::from_le_bytes([payload[4], payload[5]]);
        let hi1 = i16::from_le_bytes([payload[6], payload[7]]);
        assert_eq!((lo1, hi1), (5, 300));
    }

    #[test]
    fn digital_is_reencoded_as_bitwise_not_numeric_minmax() {
        // D0 high then a D7 glitch: numeric min/max would yield (0x01, 0x80).
        let c = chunk(Encoding::PackedDigital, 8, vec![0x01, 0x80], 2);
        let (encoding, payload, buckets) = minmax_reencode(&c, 1);
        assert_eq!(encoding, Encoding::BitwiseDigital);
        assert_eq!(buckets, 1);
        assert_eq!(payload, vec![0x00, 0x81], "(AND, OR): D0 and D7 toggled");
    }

    #[test]
    fn minmax_reencode_already_decimated_is_a_pass_through() {
        let c = chunk(Encoding::MinMaxU8, 1, vec![1, 2, 3, 4], 2);
        let (encoding, payload, buckets) = minmax_reencode(&c, 512);
        assert_eq!(
            encoding,
            Encoding::MinMaxU8,
            "pass-through keeps its encoding"
        );
        assert_eq!(payload, vec![1, 2, 3, 4]);
        assert_eq!(buckets, 2);
    }
}
