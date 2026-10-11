//! Analog storage: lossless codec blocks (SPECIFICATION.md §2.8.2).
//!
//! Raw ADC codes — never volts — stored per channel in blocks of 64 Ki
//! samples. Each block picks whichever of two codecs is smaller:
//!
//! | Id | Codec |
//! |---|---|
//! | 0 | stored raw |
//! | 1 | delta, zigzag, bit-packed at a width covering most deltas, with an escape list for outliers |
//!
//! The open tail block stays uncompressed until it fills. A small
//! move-to-front cache of decoded blocks per channel makes repeated nearby
//! queries (a scrolling view) cheap.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::error::{RecordError, Result};
use crate::summary::{AnalogBucket, AnalogChannelStat};
use crate::varint::{read_uvarint, write_uvarint};

/// Samples per analog block: 64 Ki (SPEC §2.8.2).
pub const BLOCK_SAMPLES: usize = 64 * 1024;

const CACHE_CAPACITY: usize = 4;

#[inline(always)]
fn zigzag_encode(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

#[inline(always)]
fn zigzag_decode(z: u32) -> i32 {
    ((z >> 1) as i32) ^ -((z & 1) as i32)
}

struct BitWriter {
    buf: Vec<u8>,
    cur: u64,
    nbits: u32,
}

impl BitWriter {
    fn with_capacity(bytes: usize) -> Self {
        BitWriter {
            buf: Vec::with_capacity(bytes),
            cur: 0,
            nbits: 0,
        }
    }

    /// Writes `width` (<= 24, see [`choose_width_from_histogram`]) bits of
    /// `value`, flushing a whole 4-byte word at a time instead of one byte
    /// per call — `nbits` never exceeds 31 bits before a call, and a call
    /// adds at most 24, so it crosses the 32-bit flush threshold at most
    /// once, no loop needed. Called once per delta in a 64 Ki-sample block,
    /// so cutting ~5x the `Vec` appends here matters (SPEC accept:
    /// >=100 Msamples/s release for u8 2-channel analog).
    #[inline(always)]
    fn write(&mut self, value: u64, width: u32) {
        debug_assert!(width <= 24);
        self.cur |= value << self.nbits;
        self.nbits += width;
        if self.nbits >= 32 {
            self.buf.extend_from_slice(&(self.cur as u32).to_le_bytes());
            self.cur >>= 32;
            self.nbits -= 32;
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut buf = self.buf;
        let remaining_bytes = self.nbits.div_ceil(8) as usize;
        buf.extend_from_slice(&self.cur.to_le_bytes()[..remaining_bytes]);
        buf
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
    cur: u64,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        BitReader {
            buf,
            pos: 0,
            cur: 0,
            nbits: 0,
        }
    }

    fn read(&mut self, width: u32) -> u64 {
        while self.nbits < width {
            let byte = self.buf.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.cur |= (byte as u64) << self.nbits;
            self.nbits += 8;
        }
        let mask = (1u64 << width) - 1;
        let v = self.cur & mask;
        self.cur >>= width;
        self.nbits -= width;
        v
    }
}

enum Codec {
    /// Native-width bytes, in sample order.
    Raw(Vec<u8>),
    /// `base:i32 LE, width:u8, escape_count:LEB128, escapes:[LEB128 zigzag],
    /// packed:[width-bit zigzag deltas, sentinel = escape]`.
    Delta(Vec<u8>),
}

fn encode_raw(codes: &[i32], sample_bits: u8) -> Vec<u8> {
    if sample_bits == 16 {
        let mut buf = Vec::with_capacity(codes.len() * 2);
        for &c in codes {
            buf.extend_from_slice(&(c as i16).to_le_bytes());
        }
        buf
    } else {
        // Hoisted out of the loop: branching on `sample_bits` (loop
        // invariant) on every element was pure overhead.
        codes.iter().map(|&c| c as u8).collect()
    }
}

fn decode_raw(bytes: &[u8], sample_bits: u8) -> Vec<i32> {
    if sample_bits == 16 {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c) as i32)
            .collect()
    } else {
        bytes.iter().map(|&b| b as i32).collect()
    }
}

/// Min, max and sum of `codes` in one pass (SPEC §2.8.3 analog bucket).
fn min_max_sum(codes: &[i32]) -> AnalogChannelStat {
    let mut min = i32::MAX;
    let mut max = i32::MIN;
    let mut sum = 0i64;
    for &c in codes {
        if c < min {
            min = c;
        }
        if c > max {
            max = c;
        }
        sum += c as i64;
    }
    if codes.is_empty() {
        min = 0;
        max = 0;
    }
    AnalogChannelStat { min, max, sum }
}

/// Bit width covering the given percentile of a histogram of "bits needed"
/// per zigzag-encoded delta; the rest escape. Clamped well below 32 so
/// `(1 << width) - 1` never overflows and stays far above any real 8/16-bit
/// delta magnitude (<= 18 bits).
fn choose_width_from_histogram(hist: &[usize; 33], total: usize) -> u32 {
    if total == 0 {
        return 1;
    }
    let target = ((total as f64) * 0.95).ceil() as usize;
    let mut cum = 0;
    let mut width = 1u32;
    for (b, &count) in hist.iter().enumerate() {
        cum += count;
        if cum >= target {
            width = b as u32;
            break;
        }
    }
    width.clamp(1, 24)
}

/// Points sampled to *estimate* the delta bit-width distribution — the
/// pack pass below still visits every delta exactly once (unavoidable,
/// since every one must be written), but width selection only affects the
/// compression ratio, not correctness (any delta that doesn't fit escapes
/// regardless of which width was picked), so it's safe to estimate this
/// from a bounded sample instead of an O(n) histogram over the whole block.
const WIDTH_ESTIMATE_SAMPLES: usize = 512;

/// Estimates the covering bit-width from up to [`WIDTH_ESTIMATE_SAMPLES`]
/// evenly spaced adjacent-sample deltas, in O(min(n, samples)) rather than
/// O(n) — the second full O(n) histogram pass this replaced was as
/// expensive as the bit-packing pass itself (SPEC accept: >=100 Msamples/s
/// release for u8 2-channel analog).
fn estimate_width(codes: &[i32]) -> u32 {
    let deltas_len = codes.len() - 1;
    if deltas_len == 0 {
        return 1;
    }
    let step = deltas_len.div_ceil(WIDTH_ESTIMATE_SAMPLES).max(1);
    let mut hist = [0usize; 33];
    let mut count = 0usize;
    let mut i = 0usize;
    while i < deltas_len {
        let z = zigzag_encode(codes[i + 1] - codes[i]);
        hist[(32 - z.leading_zeros()) as usize] += 1;
        count += 1;
        i += step;
    }
    choose_width_from_histogram(&hist, count)
}

fn encode_delta(codes: &[i32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(codes.len() + 8);
    let base = codes[0];
    buf.extend_from_slice(&base.to_le_bytes());
    let deltas = &codes[1..];

    let width = estimate_width(codes);
    let sentinel = (1u32 << width) - 1;
    buf.push(width as u8);

    // Single full pass: bit-pack, with escapes' LEB128 bytes accumulated
    // inline (no separate `Vec<u32>` of escape values re-walked afterwards).
    let mut escape_bytes = Vec::new();
    let mut escape_count: u64 = 0;
    let mut bw = BitWriter::with_capacity(deltas.len() * width as usize / 8 + 1);
    let mut prev = base;
    for &c in deltas {
        let z = zigzag_encode(c - prev);
        prev = c;
        if z < sentinel {
            bw.write(z as u64, width);
        } else {
            bw.write(sentinel as u64, width);
            write_uvarint(&mut escape_bytes, z as u64);
            escape_count += 1;
        }
    }
    write_uvarint(&mut buf, escape_count);
    buf.extend_from_slice(&escape_bytes);
    buf.extend_from_slice(&bw.finish());
    buf
}

fn decode_delta(bytes: &[u8], sample_count: usize) -> Vec<i32> {
    let base = i32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
    let width = bytes[4] as u32;
    let mut pos = 5;
    let escape_count = read_uvarint(bytes, &mut pos).expect("escape count") as usize;
    let mut escapes = Vec::with_capacity(escape_count);
    for _ in 0..escape_count {
        escapes.push(read_uvarint(bytes, &mut pos).expect("escape value") as u32);
    }
    let sentinel = (1u32 << width) - 1;
    let mut br = BitReader::new(&bytes[pos..]);
    let mut out = Vec::with_capacity(sample_count);
    out.push(base);
    let mut prev = base;
    let mut esc_iter = escapes.into_iter();
    for _ in 1..sample_count {
        let v = br.read(width) as u32;
        let z = if v == sentinel {
            esc_iter.next().expect("one escape per sentinel")
        } else {
            v
        };
        prev += zigzag_decode(z);
        out.push(prev);
    }
    out
}

fn build_block_codec(codes: &[i32], sample_bits: u8) -> Codec {
    // Raw's size is `codes.len() * bytes_per_sample` regardless of the
    // data — no need to materialize it just to measure it. Building it is
    // only worthwhile when it's actually the smaller (and thus stored)
    // codec, which halves the work on the common path where delta wins.
    let raw_size = codes.len() * if sample_bits == 16 { 2 } else { 1 };
    let delta = encode_delta(codes);
    if delta.len() < raw_size {
        Codec::Delta(delta)
    } else {
        Codec::Raw(encode_raw(codes, sample_bits))
    }
}

struct AnalogBlock {
    first_sample: u64,
    sample_count: u32,
    codec: Codec,
}

impl AnalogBlock {
    fn approx_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match &self.codec {
                Codec::Raw(b) => b.len(),
                Codec::Delta(b) => b.len(),
            }
    }

    fn decode(&self, sample_bits: u8) -> Vec<i32> {
        match &self.codec {
            Codec::Raw(bytes) => decode_raw(bytes, sample_bits),
            Codec::Delta(bytes) => decode_delta(bytes, self.sample_count as usize),
        }
    }
}

struct ChannelStore {
    blocks: VecDeque<AnalogBlock>,
    open_tail: Vec<i32>,
    /// `Arc`/`std::sync::Mutex`, not `Rc`/`RefCell`: the server (T18) shares
    /// a whole `Recording` — this cache included — across tokio tasks and a
    /// dedicated ingest thread (SPEC §2.8, §3.4), so every type reachable
    /// from it must be `Send + Sync`. The cache is still only ever touched
    /// through `&self` methods with the lock held for a few instructions, so
    /// this costs nothing meaningful over the single-threaded `RefCell`.
    cache: Mutex<Vec<(u64, Arc<Vec<i32>>)>>,
}

impl ChannelStore {
    fn new() -> Self {
        ChannelStore {
            blocks: VecDeque::new(),
            open_tail: Vec::new(),
            cache: Mutex::new(Vec::new()),
        }
    }

    /// Index of the first block that could overlap `start`, by binary
    /// search over the (sorted, ascending) block deque — O(log n) seek even
    /// with 10^5+ blocks in a long recording (views query at up to 60 fps).
    fn seek_block(&self, start: u64) -> usize {
        let mut lo = 0usize;
        let mut hi = self.blocks.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let block = &self.blocks[mid];
            if block.first_sample + block.sample_count as u64 <= start {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn decode_cached(&self, block: &AnalogBlock, sample_bits: u8) -> Arc<Vec<i32>> {
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(pos) = cache.iter().position(|(fs, _)| *fs == block.first_sample) {
                let entry = cache.remove(pos);
                let codes = entry.1.clone();
                cache.push(entry);
                return codes;
            }
        }
        let codes = Arc::new(block.decode(sample_bits));
        let mut cache = self.cache.lock().unwrap();
        cache.push((block.first_sample, codes.clone()));
        if cache.len() > CACHE_CAPACITY {
            cache.remove(0);
        }
        codes
    }
}

/// Analog codec store for one recording's interleaved channels.
pub struct AnalogStore {
    channel_count: usize,
    sample_bits: u8,
    channels: Vec<ChannelStore>,
    open_block_first_sample: u64,
    first_sample: u64,
    head_sample: u64,
    /// The summary tree's level-0 bucket span, so `append_analog` can
    /// compute each bucket's min/max/sum *while* de-interleaving instead of
    /// the caller re-scanning the same samples afterwards — that second
    /// full pass, once per `level0_span` samples, was as expensive as
    /// ingest itself (SPEC accept: >=100 Msamples/s release, u8 2-channel).
    /// `u64::MAX` (the default, see [`AnalogStore::new`]) disables this:
    /// no bucket ever completes, which is correct (if useless) for tests
    /// that only exercise storage, not the tree.
    level0_span: u64,
    /// Running (min, max, sum) per channel since the last completed bucket.
    pending_stats: Vec<(i32, i32, i64)>,
    pending_since: u64,
    /// Buckets completed during the most recent `append_analog` call,
    /// drained by [`crate::recording::Recording`] into the summary tree.
    ready_buckets: Vec<AnalogBucket>,
    /// Sum of `approx_bytes` over every channel's closed blocks, kept
    /// incrementally for the same reason as `DigitalStore::block_bytes`.
    block_bytes: usize,
}

impl AnalogStore {
    pub fn new(channel_count: usize, sample_bits: u8) -> Self {
        assert!(
            channel_count > 0,
            "analog recording needs at least one channel"
        );
        assert!(
            sample_bits == 8 || sample_bits == 16,
            "sample_bits must be 8 or 16"
        );
        AnalogStore {
            channel_count,
            sample_bits,
            channels: (0..channel_count).map(|_| ChannelStore::new()).collect(),
            open_block_first_sample: 0,
            first_sample: 0,
            head_sample: 0,
            level0_span: u64::MAX,
            pending_stats: vec![(i32::MAX, i32::MIN, 0); channel_count],
            pending_since: 0,
            ready_buckets: Vec::new(),
            block_bytes: 0,
        }
    }

    /// Sets the summary tree's level-0 span so `append_analog` can fuse
    /// bucket completion into its de-interleave loop. Must be called
    /// before the first `append_analog` (typically right after `new`, by
    /// [`crate::recording::Recording::new`]); a later call is ignored once
    /// any bucket has already started accumulating differently.
    pub fn set_level0_span(&mut self, span: u64) {
        debug_assert!(span > 0);
        if self.pending_since == 0 {
            self.level0_span = span;
        }
    }

    /// Drains the buckets completed by the most recent `append_analog`
    /// call, in order, for the caller to push into the summary tree.
    pub fn drain_ready_buckets(&mut self) -> Vec<AnalogBucket> {
        std::mem::take(&mut self.ready_buckets)
    }

    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    pub fn sample_bits(&self) -> u8 {
        self.sample_bits
    }

    pub fn bytes_per_sample(&self) -> usize {
        if self.sample_bits == 16 {
            2
        } else {
            1
        }
    }

    /// Bytes per interleaved frame (all channels, one sample each).
    pub fn frame_bytes(&self) -> usize {
        self.channel_count * self.bytes_per_sample()
    }

    pub fn first_sample(&self) -> u64 {
        self.first_sample
    }

    pub fn head_sample(&self) -> u64 {
        self.head_sample
    }

    pub fn append_analog(&mut self, data: &[u8]) -> Result<()> {
        let frame = self.frame_bytes();
        if !data.len().is_multiple_of(frame) {
            return Err(RecordError::MisalignedAppend {
                len: data.len(),
                bytes_per_sample: frame,
            });
        }
        let n = data.len() / frame;
        let channel_count = self.channel_count;

        // Walk the new samples in segments aligned to the summary tree's
        // level-0 span, de-interleaving and folding each segment's
        // min/max/sum into the running per-channel total in the same pass
        // — a bucket completes here, in-line, instead of the caller
        // re-scanning the same range again afterwards (SPEC accept:
        // >=100 Msamples/s release for u8 2-channel analog).
        let mut produced = 0usize;
        while produced < n {
            let remaining_in_bucket = (self.level0_span - self.pending_since) as usize;
            let seg_len = remaining_in_bucket.min(n - produced);
            let seg_frame_off = produced * frame;

            if self.sample_bits == 16 {
                for (ch, chan) in self.channels.iter_mut().enumerate() {
                    let old_len = chan.open_tail.len();
                    chan.open_tail.resize(old_len + seg_len, 0);
                    let out = &mut chan.open_tail[old_len..];
                    let mut off = seg_frame_off + ch * 2;
                    // min/max/sum computed in the same pass as the write,
                    // not a second read-back over `out` afterwards.
                    let (mut min, mut max, mut sum) = (i32::MAX, i32::MIN, 0i64);
                    for slot in out.iter_mut() {
                        let v = i16::from_le_bytes([data[off], data[off + 1]]) as i32;
                        *slot = v;
                        min = min.min(v);
                        max = max.max(v);
                        sum += v as i64;
                        off += frame;
                    }
                    let p = &mut self.pending_stats[ch];
                    p.0 = p.0.min(min);
                    p.1 = p.1.max(max);
                    p.2 += sum;
                }
            } else {
                for (ch, chan) in self.channels.iter_mut().enumerate() {
                    let old_len = chan.open_tail.len();
                    chan.open_tail.resize(old_len + seg_len, 0);
                    let out = &mut chan.open_tail[old_len..];
                    let src = &data[seg_frame_off + ch..];
                    let (mut min, mut max, mut sum) = (i32::MAX, i32::MIN, 0i64);
                    for (slot, &b) in out.iter_mut().zip(src.iter().step_by(channel_count)) {
                        let v = b as i32;
                        *slot = v;
                        min = min.min(v);
                        max = max.max(v);
                        sum += v as i64;
                    }
                    let p = &mut self.pending_stats[ch];
                    p.0 = p.0.min(min);
                    p.1 = p.1.max(max);
                    p.2 += sum;
                }
            }

            produced += seg_len;
            self.pending_since += seg_len as u64;
            if self.pending_since >= self.level0_span {
                let channels = self
                    .pending_stats
                    .iter()
                    .map(|&(min, max, sum)| AnalogChannelStat { min, max, sum })
                    .collect();
                self.ready_buckets.push(AnalogBucket {
                    count: self.pending_since,
                    channels,
                });
                self.pending_since = 0;
                self.pending_stats.fill((i32::MAX, i32::MIN, 0));
            }
        }

        self.head_sample += n as u64;
        self.close_full_blocks();
        Ok(())
    }

    fn close_full_blocks(&mut self) {
        while self.channels[0].open_tail.len() >= BLOCK_SAMPLES {
            for chan in &mut self.channels {
                let codes: Vec<i32> = chan.open_tail.drain(0..BLOCK_SAMPLES).collect();
                let codec = build_block_codec(&codes, self.sample_bits);
                let block = AnalogBlock {
                    first_sample: self.open_block_first_sample,
                    sample_count: BLOCK_SAMPLES as u32,
                    codec,
                };
                self.block_bytes += block.approx_bytes();
                chan.blocks.push_back(block);
            }
            self.open_block_first_sample += BLOCK_SAMPLES as u64;
        }
    }

    pub fn bytes(&self) -> usize {
        self.block_bytes
            + self
                .channels
                .iter()
                .map(|c| c.open_tail.len() * std::mem::size_of::<i32>())
                .sum::<usize>()
    }

    /// Raw codes for one channel over `[start, end)` (already clipped).
    pub fn channel_samples(&self, ch: usize, start: u64, end: u64) -> Vec<i32> {
        let mut out = Vec::with_capacity((end - start) as usize);
        let chan = &self.channels[ch];
        let start_idx = chan.seek_block(start);
        for block in chan.blocks.iter().skip(start_idx) {
            let block_end = block.first_sample + block.sample_count as u64;
            if block_end <= start {
                continue;
            }
            if block.first_sample >= end {
                return out;
            }
            let codes = chan.decode_cached(block, self.sample_bits);
            let lo = (start.max(block.first_sample) - block.first_sample) as usize;
            let hi = (end.min(block_end) - block.first_sample) as usize;
            out.extend_from_slice(&codes[lo..hi]);
        }
        if start < self.head_sample && self.open_block_first_sample < end {
            let tail_start = self.open_block_first_sample;
            let lo = (start.max(tail_start).saturating_sub(tail_start)) as usize;
            let hi = (end.min(self.head_sample).saturating_sub(tail_start)) as usize;
            if lo < hi {
                out.extend_from_slice(&chan.open_tail[lo..hi]);
            }
        }
        out
    }

    /// Exactly the appended bytes for `[start, end)`, interleaved.
    pub fn extract_analog(&self, start: u64, end: u64) -> Vec<u8> {
        let n = (end - start) as usize;
        let per_channel: Vec<Vec<i32>> = (0..self.channel_count)
            .map(|ch| self.channel_samples(ch, start, end))
            .collect();
        let bps = self.bytes_per_sample();
        let frame_bytes = self.frame_bytes();
        let mut buf = vec![0u8; n * frame_bytes];
        for (ch, codes) in per_channel.iter().enumerate() {
            for (i, &code) in codes.iter().enumerate() {
                let off = i * frame_bytes + ch * bps;
                if self.sample_bits == 16 {
                    buf[off..off + 2].copy_from_slice(&(code as i16).to_le_bytes());
                } else {
                    buf[off] = code as u8;
                }
            }
        }
        buf
    }

    /// Exact analog summary of `[start, end)`, from stored codes.
    pub fn exact_bucket(&self, start: u64, end: u64) -> AnalogBucket {
        let channels = (0..self.channel_count)
            .map(|ch| {
                // Single pass over the samples for min/max/sum together —
                // three separate iterator passes here would triple the
                // work done on every closed summary bucket, which on the
                // ingest path happens once per `level0_span` samples.
                if start < self.head_sample && start >= self.open_block_first_sample {
                    // Fast path: the whole range is in the still-open tail
                    // (the common case while ingesting), so scan the slice
                    // directly instead of allocating a fresh Vec first.
                    let tail_start = self.open_block_first_sample;
                    let lo = (start - tail_start) as usize;
                    let hi = (end.min(self.head_sample) - tail_start) as usize;
                    min_max_sum(&self.channels[ch].open_tail[lo..hi])
                } else {
                    let samples = self.channel_samples(ch, start, end);
                    min_max_sum(&samples)
                }
            })
            .collect();
        AnalogBucket {
            count: end - start,
            channels,
        }
    }

    /// Drops the oldest finalized block from every channel (they always
    /// finalize in lockstep). Returns approximate bytes freed.
    pub fn evict_oldest_block(&mut self) -> usize {
        let mut freed = 0usize;
        let mut new_first_sample = None;
        for chan in &mut self.channels {
            if let Some(block) = chan.blocks.pop_front() {
                new_first_sample = Some(block.first_sample + block.sample_count as u64);
                freed += block.approx_bytes();
                chan.cache.lock().unwrap().clear();
            }
        }
        if let Some(fs) = new_first_sample {
            self.first_sample = fs;
        }
        self.block_bytes -= freed;
        freed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes_to_bytes(codes: &[i32], sample_bits: u8) -> Vec<u8> {
        encode_raw(codes, sample_bits)
    }

    #[test]
    fn round_trip_constant() {
        let codes = vec![42i32; 1000];
        let mut store = AnalogStore::new(1, 8);
        store.append_analog(&codes_to_bytes(&codes, 8)).unwrap();
        let extracted = store.extract_analog(0, 1000);
        assert_eq!(extracted, codes_to_bytes(&codes, 8));
    }

    #[test]
    fn round_trip_full_scale_noise_u8() {
        let mut seed = 12345u32;
        let mut next = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 24) as u8
        };
        let codes: Vec<i32> = (0..BLOCK_SAMPLES * 2 + 37).map(|_| next() as i32).collect();
        let mut store = AnalogStore::new(1, 8);
        let bytes = codes_to_bytes(&codes, 8);
        store.append_analog(&bytes).unwrap();
        assert_eq!(store.extract_analog(0, codes.len() as u64), bytes);
    }

    #[test]
    fn round_trip_single_outlier() {
        // A full block so this actually exercises block codec selection
        // (mostly-zero deltas with one huge escape), not just the open tail.
        let mut codes = vec![100i32; BLOCK_SAMPLES];
        codes[BLOCK_SAMPLES / 2] = -128;
        let mut store = AnalogStore::new(1, 16);
        let bytes = codes_to_bytes(&codes, 16);
        store.append_analog(&bytes).unwrap();
        assert_eq!(store.extract_analog(0, codes.len() as u64), bytes);
    }

    #[test]
    fn round_trip_i16_extremes() {
        let pattern = [
            i32::from(i16::MIN),
            i32::from(i16::MAX),
            0,
            i32::from(i16::MIN),
            i32::from(i16::MAX),
        ];
        let codes: Vec<i32> = pattern
            .iter()
            .copied()
            .cycle()
            .take(BLOCK_SAMPLES)
            .collect();
        let mut store = AnalogStore::new(1, 16);
        let bytes = codes_to_bytes(&codes, 16);
        store.append_analog(&bytes).unwrap();
        assert_eq!(store.extract_analog(0, codes.len() as u64), bytes);
    }

    #[test]
    fn codec_delta_round_trips_directly() {
        for codes in [
            vec![7i32; 500],
            {
                let mut v = vec![50i32; 500];
                v[250] = 5000;
                v
            },
            (0..500i32).map(|i| i * 37 % 251).collect(),
        ] {
            let encoded = encode_delta(&codes);
            let decoded = decode_delta(&encoded, codes.len());
            assert_eq!(decoded, codes);
        }
    }

    #[test]
    fn misaligned_append_is_rejected() {
        let mut store = AnalogStore::new(2, 16); // 4 bytes/frame
        let err = store.append_analog(&[0u8; 5]).unwrap_err();
        assert!(matches!(err, RecordError::MisalignedAppend { .. }));
    }

    #[test]
    fn evict_advances_first_sample_and_extract_stays_exact() {
        let mut codes = Vec::new();
        let mut seed = 7u32;
        for i in 0..(BLOCK_SAMPLES * 3) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            codes.push(((seed >> 24) as i32 % 20) + (i as i32 / 5000)); // slow drift + noise
        }
        let mut store = AnalogStore::new(1, 8);
        store.append_analog(&codes_to_bytes(&codes, 8)).unwrap();
        let freed = store.evict_oldest_block();
        assert!(freed > 0);
        let fs = store.first_sample();
        let hs = store.head_sample();
        let expected = codes_to_bytes(&codes[fs as usize..hs as usize], 8);
        assert_eq!(store.extract_analog(fs, hs), expected);
    }
}
