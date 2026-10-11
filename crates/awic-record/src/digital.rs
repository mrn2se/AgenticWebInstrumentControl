//! Digital storage: run-length encoded blocks (SPECIFICATION.md §2.8.1).
//!
//! The packed sample stream (one byte per sample, bit *n* = D*n*) is stored
//! as runs `(value, length)`, grouped into blocks of up to
//! [`BLOCK_MAX_RUNS`] runs. Cost is proportional to edges, not samples: an
//! idle channel costs nothing per second, and the pathological
//! change-every-sample case falls back to packed bytes per block so it never
//! costs more than 1x.

use std::collections::VecDeque;

use crate::error::{RecordError, Result};
use crate::summary::DigitalBucket;
use crate::varint::{read_uvarint, write_uvarint};

/// Blocks close after this many runs (SPEC §2.8.1).
pub const BLOCK_MAX_RUNS: usize = 4096;

/// One decoded, contiguous run: `value` held for `len` samples starting at
/// `start`. The unit `awic-pattern` (T14) iterates to search history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub start: u64,
    pub len: u64,
    pub value: u8,
}

enum BlockData {
    /// `(value: u8, length: LEB128)` pairs, in order.
    Runs(Vec<u8>),
    /// Packed fallback: one raw byte per sample, chosen when it is smaller
    /// than the run encoding (SPEC §2.8.1).
    Packed(Vec<u8>),
}

struct DigitalBlock {
    first_sample: u64,
    sample_count: u64,
    data: BlockData,
}

impl DigitalBlock {
    fn approx_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match &self.data {
                BlockData::Runs(b) => b.len(),
                BlockData::Packed(b) => b.len(),
            }
    }

    /// Decode this block back into `(value, length)` pairs, in order.
    fn runs(&self) -> Vec<(u8, u64)> {
        match &self.data {
            BlockData::Runs(bytes) => decode_run_bytes(bytes),
            BlockData::Packed(bytes) => rle_encode_bytes(bytes),
        }
    }
}

fn encode_run_bytes(runs: &[(u8, u64)]) -> Vec<u8> {
    let mut buf = Vec::new();
    for &(v, len) in runs {
        buf.push(v);
        write_uvarint(&mut buf, len);
    }
    buf
}

fn decode_run_bytes(bytes: &[u8]) -> Vec<(u8, u64)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let v = bytes[pos];
        pos += 1;
        let len =
            read_uvarint(bytes, &mut pos).expect("well-formed run block (internal invariant)");
        out.push((v, len));
    }
    out
}

fn rle_encode_bytes(bytes: &[u8]) -> Vec<(u8, u64)> {
    let mut out = Vec::new();
    let mut it = bytes.iter();
    if let Some(&first) = it.next() {
        let mut cur = (first, 1u64);
        for &b in it {
            if b == cur.0 {
                cur.1 += 1;
            } else {
                out.push(cur);
                cur = (b, 1);
            }
        }
        out.push(cur);
    }
    out
}

/// Choose the smaller of the run encoding and the packed fallback for a
/// finished block's worth of runs.
fn finalize_block_data(runs: &[(u8, u64)]) -> (u64, BlockData) {
    let sample_count: u64 = runs.iter().map(|&(_, l)| l).sum();
    let encoded = encode_run_bytes(runs);
    if (encoded.len() as u64) <= sample_count {
        (sample_count, BlockData::Runs(encoded))
    } else {
        let mut packed = Vec::with_capacity(sample_count as usize);
        for &(v, len) in runs {
            packed.resize(packed.len() + len as usize, v);
        }
        (sample_count, BlockData::Packed(packed))
    }
}

/// Digital run-length store for one recording's packed byte stream.
pub struct DigitalStore {
    blocks: VecDeque<DigitalBlock>,
    /// Runs accumulated for the block currently being built (< `BLOCK_MAX_RUNS`).
    open_runs: Vec<(u8, u64)>,
    open_block_first_sample: u64,
    /// The in-progress run, not yet pushed to `open_runs` — the "still-open
    /// tail" (SPEC §2.8.1); may grow across many `append_packed` calls.
    /// Split into scalar fields rather than `Option<(u8, u64)>` so the
    /// hot ingest loop (SPEC §2.8 accept: >=10x real time at 24 MS/s) stays
    /// branch-simple instead of matching an enum discriminant every byte.
    cur_run_active: bool,
    cur_run_value: u8,
    cur_run_len: u64,
    first_sample: u64,
    head_sample: u64,
    /// Sum of `approx_bytes` over `blocks`, kept as blocks close and are
    /// evicted: `bytes()` runs after every ingested chunk (the server's
    /// memory budget), so it must not walk a deque that grows for hours.
    block_bytes: usize,
}

impl DigitalStore {
    pub fn new() -> Self {
        DigitalStore {
            blocks: VecDeque::new(),
            open_runs: Vec::new(),
            open_block_first_sample: 0,
            cur_run_active: false,
            cur_run_value: 0,
            cur_run_len: 0,
            first_sample: 0,
            head_sample: 0,
            block_bytes: 0,
        }
    }

    pub fn first_sample(&self) -> u64 {
        self.first_sample
    }

    pub fn head_sample(&self) -> u64 {
        self.head_sample
    }

    pub fn append_packed(&mut self, data: &[u8]) {
        let mut active = self.cur_run_active;
        let mut value = self.cur_run_value;
        let mut len = self.cur_run_len;
        for &b in data {
            if active && value == b {
                len += 1;
                continue;
            }
            if active {
                self.push_run((value, len));
            }
            active = true;
            value = b;
            len = 1;
        }
        self.cur_run_active = active;
        self.cur_run_value = value;
        self.cur_run_len = len;
        self.head_sample += data.len() as u64;
    }

    fn cur_run(&self) -> Option<(u8, u64)> {
        self.cur_run_active
            .then_some((self.cur_run_value, self.cur_run_len))
    }

    /// Cold relative to the byte-compare loop in `append_packed`: real
    /// signals hold a value for many samples, so this runs once per edge,
    /// not once per sample.
    #[cold]
    fn push_run(&mut self, run: (u8, u64)) {
        if self.open_runs.len() >= BLOCK_MAX_RUNS {
            self.close_open_block();
        }
        self.open_runs.push(run);
    }

    fn close_open_block(&mut self) {
        if self.open_runs.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.open_runs);
        let (sample_count, data) = finalize_block_data(&runs);
        let block = DigitalBlock {
            first_sample: self.open_block_first_sample,
            sample_count,
            data,
        };
        self.block_bytes += block.approx_bytes();
        self.blocks.push_back(block);
        self.open_block_first_sample += sample_count;
    }

    /// Approximate heap bytes used by stored data (not including the summary
    /// tree, which the [`crate::recording::Recording`] adds separately).
    pub fn bytes(&self) -> usize {
        self.block_bytes + self.open_runs.len() * std::mem::size_of::<(u8, u64)>()
    }

    /// Index of the first block that could overlap `start` (its end sample
    /// is `> start`), found by binary search over the block deque (sorted,
    /// ascending, by construction) rather than a linear scan — O(log n)
    /// seek even with 10^5+ blocks in a long, edge-dense recording.
    fn seek_block(&self, start: u64) -> usize {
        let mut lo = 0usize;
        let mut hi = self.blocks.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let block = &self.blocks[mid];
            if block.first_sample + block.sample_count <= start {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Calls `f(start, len, value)` once per contiguous run-piece
    /// overlapping `[start, end)`, in order. Pieces may split at storage
    /// boundaries (block edges); callers that need merged runs coalesce
    /// adjacent equal-value pieces themselves.
    fn for_each_piece<F: FnMut(u64, u64, u8)>(&self, start: u64, end: u64, mut f: F) {
        let start_idx = self.seek_block(start);
        for block in self.blocks.iter().skip(start_idx) {
            let block_end = block.first_sample + block.sample_count;
            if block_end <= start {
                continue;
            }
            if block.first_sample >= end {
                return;
            }
            let mut cur = block.first_sample;
            for (v, len) in block.runs() {
                let piece_end = cur + len;
                let lo = cur.max(start);
                let hi = piece_end.min(end);
                if lo < hi {
                    f(lo, hi - lo, v);
                }
                cur = piece_end;
            }
        }

        let mut cur = self.open_block_first_sample;
        for &(v, len) in &self.open_runs {
            let piece_end = cur + len;
            let lo = cur.max(start);
            let hi = piece_end.min(end);
            if lo < hi {
                f(lo, hi - lo, v);
            }
            cur = piece_end;
        }
        if let Some((v, len)) = self.cur_run() {
            let piece_end = cur + len;
            let lo = cur.max(start);
            let hi = piece_end.min(end);
            if lo < hi {
                f(lo, hi - lo, v);
            }
        }
    }

    /// Contiguous runs over `[start, end)`, clipped to that range.
    /// Ranges are validated by [`crate::recording::Recording`]; `start`/`end`
    /// here are assumed already clipped to `[first_sample, head_sample]`.
    pub fn runs(&self, start: u64, end: u64) -> Vec<Run> {
        let mut out: Vec<Run> = Vec::new();
        self.for_each_piece(start, end, |s, l, v| {
            if let Some(last) = out.last_mut() {
                if last.value == v && last.start + last.len == s {
                    last.len += l;
                    return;
                }
            }
            out.push(Run {
                start: s,
                len: l,
                value: v,
            });
        });
        out
    }

    pub fn extract_packed(&self, start: u64, end: u64) -> Vec<u8> {
        let mut buf = vec![0u8; (end - start) as usize];
        self.for_each_piece(start, end, |s, l, v| {
            let off = (s - start) as usize;
            buf[off..off + l as usize].fill(v);
        });
        buf
    }

    /// Exact digital summary of `[start, end)`, built from stored runs —
    /// used both to close level-0 summary buckets and to cover the partial
    /// edges of a `summarize()` query that the tree can't (SPEC §2.8.3).
    pub fn exact_bucket(&self, start: u64, end: u64) -> DigitalBucket {
        let mut acc: Option<DigitalBucket> = None;
        self.for_each_piece(start, end, |_s, l, v| {
            let mut high_count = [0u64; 8];
            for ch in 0..8u8 {
                if v & (1 << ch) != 0 {
                    high_count[ch as usize] = l;
                }
            }
            let piece = DigitalBucket {
                and: v,
                or: v,
                high_count,
                edge_count: [0; 8],
                count: l,
                first_value: v,
                last_value: v,
            };
            acc = Some(match acc.take() {
                Some(a) => DigitalBucket::merge(&a, &piece),
                None => piece,
            });
        });
        acc.unwrap_or(DigitalBucket {
            and: 0,
            or: 0,
            high_count: [0; 8],
            edge_count: [0; 8],
            count: 0,
            first_value: 0,
            last_value: 0,
        })
    }

    /// Drops the oldest finalized block, advancing `first_sample` to its
    /// end. Returns the approximate bytes freed, or 0 if there is no
    /// finalized block to drop (only the open tail remains).
    pub fn evict_oldest_block(&mut self) -> usize {
        match self.blocks.pop_front() {
            Some(block) => {
                self.first_sample = block.first_sample + block.sample_count;
                self.block_bytes -= block.approx_bytes();
                block.approx_bytes()
            }
            None => 0,
        }
    }
}

impl Default for DigitalStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates and clips `[start, end)` against `[first_sample, head_sample)`.
/// Shared by digital and analog queries (SPEC §2.8: half-open ranges,
/// clipped to the retained span, an error when the start has been evicted).
pub(crate) fn clip_range(
    start: u64,
    end: u64,
    first_sample: u64,
    head_sample: u64,
) -> Result<(u64, u64)> {
    if start >= end {
        return Err(RecordError::EmptyRange { start, end });
    }
    if start < first_sample {
        return Err(RecordError::Evicted {
            start,
            first_sample,
            head_sample,
        });
    }
    let end = end.min(head_sample);
    if start >= end {
        return Err(RecordError::EmptyRange { start, end });
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_extract_round_trip() {
        let mut store = DigitalStore::new();
        let data = [0u8, 0, 1, 1, 1, 0, 2, 2, 2, 2];
        store.append_packed(&data);
        assert_eq!(store.head_sample(), 10);
        assert_eq!(store.extract_packed(0, 10), data);
    }

    #[test]
    fn runs_merges_across_append_boundary() {
        let mut store = DigitalStore::new();
        store.append_packed(&[5, 5, 5]);
        store.append_packed(&[5, 5, 7]); // run of 5 spans the two appends
        let runs = store.runs(0, 6);
        assert_eq!(
            runs,
            vec![
                Run {
                    start: 0,
                    len: 5,
                    value: 5
                },
                Run {
                    start: 5,
                    len: 1,
                    value: 7
                },
            ]
        );
    }

    #[test]
    fn block_closes_after_max_runs_and_extract_still_exact() {
        let mut store = DigitalStore::new();
        // Alternate every sample so every sample is its own run: forces the
        // packed fallback and multiple block closes.
        let n = BLOCK_MAX_RUNS * 3 + 17;
        let data: Vec<u8> = (0..n).map(|i| (i % 2) as u8).collect();
        store.append_packed(&data);
        assert_eq!(store.extract_packed(0, n as u64), data);
        assert!(store.blocks.len() >= 3);
    }

    #[test]
    fn evict_oldest_block_advances_first_sample() {
        let mut store = DigitalStore::new();
        let n = BLOCK_MAX_RUNS * 2 + 5;
        let data: Vec<u8> = (0..n).map(|i| (i % 2) as u8).collect();
        store.append_packed(&data);
        let before = store.first_sample();
        let freed = store.evict_oldest_block();
        assert!(freed > 0);
        assert!(store.first_sample() > before);
        // Data before the new first_sample is gone from extract's contract;
        // data after it must still be exact.
        let fs = store.first_sample();
        let hs = store.head_sample();
        assert_eq!(store.extract_packed(fs, hs), data[fs as usize..hs as usize]);
    }
}
