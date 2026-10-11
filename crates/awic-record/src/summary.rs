//! The multi-resolution summary tree (SPECIFICATION.md §2.8.3).
//!
//! A pyramid of buckets over sample index: level 0 spans `2^k` samples (the
//! power of two nearest `rate_hz / 1000`, minimum 64); each higher level
//! merges 8 buckets of the level below. The tree is appended to incrementally
//! as data closes a full level-0 bucket, so its top is always current, and it
//! answers "what happened in [t0,t1)" in time proportional to the number of
//! buckets touched, not the number of samples.
//!
//! This module is generic over the bucket payload ([`Bucket`]) so digital and
//! analog recordings share one tree implementation; [`DigitalBucket`] and
//! [`AnalogBucket`] are the two concrete payloads.

use std::collections::VecDeque;

/// A summary payload that can be merged with its immediate right-hand
/// neighbour to form the summary of the concatenation of both ranges.
///
/// Merges are always of two *adjacent, contiguous* ranges taken in sample
/// order (`left` immediately precedes `right`) — required for
/// [`DigitalBucket`], whose edge count depends on whether the boundary
/// sample pair itself is a transition.
pub trait Bucket: Clone {
    fn merge(left: &Self, right: &Self) -> Self;
    /// Approximate heap bytes owned by this bucket, for [`bytes`] accounting.
    fn approx_bytes(&self) -> usize;
}

/// Level-0 bucket span: the power of two nearest `rate_hz / 1000`
/// (i.e. covering roughly 1 ms), floored at 64 samples and capped at
/// 2^16 = 65536 (SPEC §2.8.3) — comfortably within the `u32` counts the
/// sparse digital tree stores (`DigitalSummaryTree`), and generous enough
/// that the cap never binds below multi-hundred-MS/s rates.
pub fn level0_span(rate_hz: u64) -> u64 {
    let target = (rate_hz as f64 / 1000.0).max(1.0);
    let k = target.log2().round().max(0.0) as u32;
    (1u64 << k).clamp(64, 65536)
}

/// Whether all 8 siblings of the group ending at index `last` (`last - 7 ..=
/// last`) are still retained on a level whose oldest retained index is
/// `front_index`. Eviction trims the front of every level, so on a long
/// recording under the memory budget the oldest siblings of a group may be
/// gone by the time its last one closes — typically when the tree grows a
/// new top level, whose first bucket would cover everything since sample 0.
fn group_retained(front_index: u64, last: u64) -> bool {
    front_index + 7 <= last
}

/// A parent whose children were partly evicted is never built: it would
/// summarize data that no longer exists. Its level instead starts after it,
/// so the indices of the parents that do get built stay contiguous. The level
/// is necessarily empty here — every older parent covers older, already
/// evicted children, so `evict_before` dropped it too.
fn skip_parent(front_index: &mut u64, level_empty: bool, parent: u64) {
    debug_assert!(level_empty, "a retained parent precedes a skipped one");
    if level_empty {
        *front_index = (*front_index).max(parent + 1);
    }
}

struct Level<B> {
    /// Contiguous, ascending bucket indices; `front_index` is the index of
    /// `buckets[0]`. No gaps: eviction only trims the front, closing only
    /// appends to the back.
    buckets: VecDeque<B>,
    front_index: u64,
}

impl<B> Level<B> {
    fn new(front_index: u64) -> Self {
        Level {
            buckets: VecDeque::new(),
            front_index,
        }
    }
}

/// Multi-resolution summary tree over one signal (one recording's worth of
/// digital or analog data).
pub struct SummaryTree<B: Bucket> {
    bucket0_span: u64,
    levels: Vec<Level<B>>,
    /// Number of level-0 buckets closed so far, i.e. the index of the next
    /// one to close. Absolute and monotonic — unaffected by eviction, which
    /// only trims retained history, not how much has been summarized.
    next_index0: u64,
    /// Retained bytes, kept as buckets are pushed and evicted — see
    /// `DigitalStore::block_bytes` for why `bytes()` must be O(1).
    bytes: usize,
}

impl<B: Bucket> SummaryTree<B> {
    pub fn new(bucket0_span: u64) -> Self {
        assert!(bucket0_span.is_power_of_two() && bucket0_span > 0);
        SummaryTree {
            bucket0_span,
            bytes: 0,
            levels: Vec::new(),
            next_index0: 0,
        }
    }

    pub fn bucket0_span(&self) -> u64 {
        self.bucket0_span
    }

    /// Sample span covered by one bucket at `level` (0 = finest).
    pub fn span_at(&self, level: usize) -> u64 {
        self.bucket0_span * 8u64.pow(level as u32)
    }

    /// Absolute sample index up to which level-0 buckets have been closed.
    pub fn closed_head(&self) -> u64 {
        self.next_index0 * self.bucket0_span
    }

    /// Close every level-0 bucket now fully covered by `head_sample`
    /// (absolute samples appended so far), computing each via `exact`
    /// (`start, end -> bucket`, called once per level-0 span). Cascades into
    /// higher levels whenever 8 siblings complete.
    pub fn close_ready<F>(&mut self, head_sample: u64, mut exact: F)
    where
        F: FnMut(u64, u64) -> B,
    {
        while (self.next_index0 + 1) * self.bucket0_span <= head_sample {
            let start = self.next_index0 * self.bucket0_span;
            let end = start + self.bucket0_span;
            let bucket = exact(start, end);
            self.push_level0(bucket);
            self.next_index0 += 1;
        }
    }

    /// Pushes a level-0 bucket the caller has already computed exactly
    /// (its span must be `bucket0_span`, covering
    /// `[closed_head(), closed_head() + bucket0_span)`), skipping the
    /// `exact` callback `close_ready` would otherwise make. For a caller
    /// that can produce the bucket incrementally while it appends data
    /// (a fused running min/max/sum, say) rather than by re-scanning
    /// already-stored data afterwards — the two are equivalent, this just
    /// avoids a second full pass over every sample (SPEC accept: analog
    /// ingest must not pay for the tree with a per-sample rescan).
    pub fn push_ready(&mut self, bucket: B) {
        self.push_level0(bucket);
        self.next_index0 += 1;
    }

    fn ensure_level(&mut self, level: usize) {
        while self.levels.len() <= level {
            self.levels.push(Level::new(0));
        }
    }

    fn push_level0(&mut self, bucket: B) {
        self.ensure_level(0);
        self.bytes += bucket.approx_bytes() + std::mem::size_of::<B>();
        self.levels[0].buckets.push_back(bucket);

        // Cascade: whenever `level`'s just-appended bucket completes a group
        // of 8, merge that group into the parent level and repeat.
        let mut idx = self.next_index0;
        let mut level = 0;
        while (idx + 1).is_multiple_of(8) {
            if !group_retained(self.levels[level].front_index, idx) {
                self.ensure_level(level + 1);
                let parent = &mut self.levels[level + 1];
                skip_parent(&mut parent.front_index, parent.buckets.is_empty(), idx / 8);
                break;
            }
            let len = self.levels[level].buckets.len();
            let merged = self.levels[level]
                .buckets
                .range(len - 8..)
                .cloned()
                .reduce(|a, b| B::merge(&a, &b))
                .expect("8 elements");
            idx /= 8;
            level += 1;
            self.ensure_level(level);
            self.bytes += merged.approx_bytes() + std::mem::size_of::<B>();
            self.levels[level].buckets.push_back(merged);
        }
    }

    fn get(&self, level: usize, idx: u64) -> Option<&B> {
        let lvl = self.levels.get(level)?;
        if idx < lvl.front_index {
            return None;
        }
        lvl.buckets.get((idx - lvl.front_index) as usize)
    }

    /// Exact summary of `[start, end)`, both multiples of `bucket0_span`,
    /// built from whichever tree buckets are available, coarsest first.
    /// Returns `None` if any part of the range isn't covered by a closed,
    /// retained bucket (evicted, or not yet closed) — the caller falls back
    /// to computing that part from raw data.
    pub fn sum_range(&self, start: u64, end: u64) -> Option<B> {
        debug_assert_eq!(start % self.bucket0_span, 0);
        debug_assert_eq!(end % self.bucket0_span, 0);
        if start >= end {
            return None;
        }
        let mut cur = start;
        let mut acc: Option<B> = None;
        for level in (0..self.levels.len()).rev() {
            let span = self.span_at(level);
            while cur + span <= end && cur.is_multiple_of(span) {
                let idx = cur / span;
                match self.get(level, idx) {
                    Some(b) => {
                        acc = Some(match acc {
                            Some(a) => B::merge(&a, b),
                            None => b.clone(),
                        });
                        cur += span;
                    }
                    None => break,
                }
            }
        }
        if cur == end {
            acc
        } else {
            None
        }
    }

    /// Buckets that start exactly at `start` and fit within `max_end`, one
    /// per level that has one, coarsest first. Used by `runs_masked`'s
    /// bucket-skip fast path, which wants the largest available shortcut but
    /// falls back to a smaller one if the largest isn't useful (e.g. its
    /// masked bits aren't constant).
    pub fn aligned_buckets_desc(&self, start: u64, max_end: u64) -> Vec<(u64, &B)> {
        let mut out = Vec::new();
        for level in (0..self.levels.len()).rev() {
            let span = self.span_at(level);
            if span == 0 {
                continue;
            }
            if start.is_multiple_of(span) && start + span <= max_end {
                if let Some(b) = self.get(level, start / span) {
                    out.push((start + span, b));
                }
            }
        }
        out
    }

    /// Drop every bucket, at every level, entirely before `first_sample`.
    /// Returns the approximate bytes freed.
    pub fn evict_before(&mut self, first_sample: u64) -> usize {
        let mut freed = 0usize;
        for level in 0..self.levels.len() {
            let span = self.span_at(level);
            loop {
                let bucket_start = self.levels[level].front_index * span;
                let should_drop =
                    bucket_start + span <= first_sample && !self.levels[level].buckets.is_empty();
                if !should_drop {
                    break;
                }
                if let Some(b) = self.levels[level].buckets.pop_front() {
                    freed += b.approx_bytes() + std::mem::size_of::<B>();
                    self.levels[level].front_index += 1;
                }
            }
        }
        self.bytes -= freed;
        freed
    }

    /// Approximate heap bytes retained across all levels.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Per-bucket summary for a digital (8-lane packed) recording (SPEC §2.8.3).
#[derive(Debug, Clone, Copy)]
pub struct DigitalBucket {
    pub and: u8,
    pub or: u8,
    /// Samples with bit `n` set, per channel.
    pub high_count: [u64; 8],
    /// Transitions of bit `n`, per channel — counts run boundaries *inside*
    /// this range plus, when merging two adjacent buckets, the boundary
    /// between them, so it never double- or under-counts.
    pub edge_count: [u64; 8],
    pub count: u64,
    pub first_value: u8,
    pub last_value: u8,
}

impl DigitalBucket {
    /// The summary of a single sample.
    pub fn of_sample(value: u8) -> Self {
        let mut high_count = [0u64; 8];
        for (ch, slot) in high_count.iter_mut().enumerate() {
            if value & (1 << ch) != 0 {
                *slot = 1;
            }
        }
        DigitalBucket {
            and: value,
            or: value,
            high_count,
            edge_count: [0; 8],
            count: 1,
            first_value: value,
            last_value: value,
        }
    }

    /// Fraction of samples with bit `channel` set, in `[0.0, 1.0]`.
    pub fn high_fraction(&self, channel: u8) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.high_count[channel as usize] as f64 / self.count as f64
        }
    }

    /// Merges two adjacent, contiguous ranges (`left` immediately precedes
    /// `right`) into the summary of their concatenation. An inherent
    /// method, not [`Bucket::merge`]: the digital *tree* stores a
    /// compressed representation ([`DigitalSummaryTree`]), not this type
    /// directly, but call sites that build an exact `DigitalBucket` from
    /// runs (`DigitalStore::exact_bucket`) still fold pieces together with
    /// exactly this arithmetic.
    pub fn merge(left: &Self, right: &Self) -> Self {
        let mut high_count = [0u64; 8];
        let mut edge_count = [0u64; 8];
        for ch in 0..8 {
            high_count[ch] = left.high_count[ch] + right.high_count[ch];
            let boundary = ((left.last_value ^ right.first_value) >> ch) & 1;
            edge_count[ch] = left.edge_count[ch] + right.edge_count[ch] + boundary as u64;
        }
        DigitalBucket {
            and: left.and & right.and,
            or: left.or | right.or,
            high_count,
            edge_count,
            count: left.count + right.count,
            first_value: left.first_value,
            last_value: right.last_value,
        }
    }
}

/// Compact per-bucket metadata for [`DigitalSummaryTree`]: `and`/`or` (from
/// which the toggled-channel mask `and ^ or` and each constant channel's
/// level, `and`'s bit, are derived) plus the bucket's first/last sample
/// values, needed for the boundary-edge count when two buckets merge.
/// `count` is never stored: every bucket in the tree spans exactly
/// `span_at(level)` samples, always (buckets only close once full), so it's
/// recovered from context instead of paying for it 10^5+ times over.
#[derive(Debug, Clone, Copy)]
struct SparseMeta {
    and: u8,
    or: u8,
    first_value: u8,
    last_value: u8,
}

fn narrow_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// Expands a stored bucket back into a [`DigitalBucket`] with the given
/// `span` as its sample count. `inline` is the (high, edge) count for the
/// lowest-numbered toggled channel (if any); `overflow` holds the rest,
/// `(channel, high, edge)` triples, for the rare bucket with more than one
/// toggled channel.
fn decompress_digital(
    meta: SparseMeta,
    inline: (u32, u32),
    overflow: Option<&[(u8, u32, u32)]>,
    span: u64,
) -> DigitalBucket {
    let toggled = meta.and ^ meta.or;
    let mut high = [0u64; 8];
    let mut edge = [0u64; 8];
    if let Some(ch0) = (0..8u8).find(|&ch| toggled & (1 << ch) != 0) {
        high[ch0 as usize] = inline.0 as u64;
        edge[ch0 as usize] = inline.1 as u64;
    }
    for &(ch, h, e) in overflow.unwrap_or(&[]) {
        high[ch as usize] = h as u64;
        edge[ch as usize] = e as u64;
    }
    for ch in 0..8u8 {
        if toggled & (1 << ch) == 0 && meta.and & (1 << ch) != 0 {
            high[ch as usize] = span; // constant high for the whole bucket
        }
    }
    DigitalBucket {
        and: meta.and,
        or: meta.or,
        high_count: high,
        edge_count: edge,
        count: span,
        first_value: meta.first_value,
        last_value: meta.last_value,
    }
}

fn narrow_u16(v: u32) -> u16 {
    v.min(u16::MAX as u32) as u16
}

/// Either `u16` or `u32` inline (high_count, edge_count) pairs, chosen per
/// level at creation time from whether that level's span fits `u16` (level
/// 0 and usually level 1 do — a handful of samples per ms — halving their
/// cost, which dominates the tree's total; higher levels have far fewer
/// buckets, so `u32` there costs little in absolute terms).
enum CountStore {
    Narrow(VecDeque<(u16, u16)>),
    Wide(VecDeque<(u32, u32)>),
}

impl CountStore {
    fn new(span: u64) -> Self {
        if span <= u16::MAX as u64 {
            CountStore::Narrow(VecDeque::new())
        } else {
            CountStore::Wide(VecDeque::new())
        }
    }

    fn push_back(&mut self, value: (u32, u32)) {
        match self {
            CountStore::Narrow(d) => d.push_back((narrow_u16(value.0), narrow_u16(value.1))),
            CountStore::Wide(d) => d.push_back(value),
        }
    }

    fn get(&self, local: usize) -> Option<(u32, u32)> {
        match self {
            CountStore::Narrow(d) => d.get(local).map(|&(h, e)| (h as u32, e as u32)),
            CountStore::Wide(d) => d.get(local).copied(),
        }
    }

    fn pop_front(&mut self) -> Option<(u32, u32)> {
        match self {
            CountStore::Narrow(d) => d.pop_front().map(|(h, e)| (h as u32, e as u32)),
            CountStore::Wide(d) => d.pop_front(),
        }
    }

    fn bytes_per_entry(&self) -> usize {
        match self {
            CountStore::Narrow(_) => std::mem::size_of::<(u16, u16)>(),
            CountStore::Wide(_) => std::mem::size_of::<(u32, u32)>(),
        }
    }
}

struct SparseLevel {
    metas: VecDeque<SparseMeta>,
    /// One (high_count, edge_count) slot per bucket, always present, for
    /// the lowest-numbered toggled channel — the overwhelmingly common
    /// case is 0 or 1 toggled channels per bucket. Unused (all-constant)
    /// buckets pay for this one empty slot; a bucket with more than one
    /// toggled channel spills the rest into `overflow`.
    counts: CountStore,
    /// Rare: channels beyond the first toggled one, keyed by absolute
    /// bucket index at this level.
    overflow: std::collections::BTreeMap<u64, Vec<(u8, u32, u32)>>,
    front_index: u64,
}

impl SparseLevel {
    fn new(span: u64) -> Self {
        SparseLevel {
            metas: VecDeque::new(),
            counts: CountStore::new(span),
            overflow: std::collections::BTreeMap::new(),
            front_index: 0,
        }
    }
}

/// Digital-specialized summary tree (SPEC §2.8.3, M12 memory budget): same
/// pyramid-of-8 structure as [`SummaryTree`], but stores each bucket
/// compressed. A channel that's constant through a bucket (`and == or` for
/// that bit — true of every unused lane, always) costs nothing beyond the
/// shared 4-byte `and`/`or`/`first`/`last` meta; a bucket with only the one
/// toggled channel typical of a real signal (a UART line, a CAL square
/// wave) costs one more `(u32, u32)` pair. A naive per-channel `[u64; 8]`
/// pair of arrays, by contrast, costs ~150 bytes *regardless* — for a
/// 10-minute 1 MS/s recording of a 1 kHz square wave on one digital
/// channel (~586 K level-0 buckets), that was ~90 MB; this is well under
/// 10 MB (SPEC §2.3.2 / M12 exit criterion).
pub struct DigitalSummaryTree {
    bucket0_span: u64,
    levels: Vec<SparseLevel>,
    next_index0: u64,
    /// Retained bytes, kept as buckets are pushed and evicted (O(1)
    /// `bytes()`, see `SummaryTree::bytes`).
    bytes: usize,
}

impl DigitalSummaryTree {
    pub fn new(bucket0_span: u64) -> Self {
        assert!(bucket0_span.is_power_of_two() && bucket0_span > 0);
        DigitalSummaryTree {
            bucket0_span,
            levels: Vec::new(),
            next_index0: 0,
            bytes: 0,
        }
    }

    pub fn bucket0_span(&self) -> u64 {
        self.bucket0_span
    }

    pub fn span_at(&self, level: usize) -> u64 {
        self.bucket0_span * 8u64.pow(level as u32)
    }

    pub fn closed_head(&self) -> u64 {
        self.next_index0 * self.bucket0_span
    }

    /// Close every level-0 bucket now fully covered by `head_sample`,
    /// computing each via `exact` — same contract as
    /// [`SummaryTree::close_ready`].
    pub fn close_ready<F>(&mut self, head_sample: u64, mut exact: F)
    where
        F: FnMut(u64, u64) -> DigitalBucket,
    {
        while (self.next_index0 + 1) * self.bucket0_span <= head_sample {
            let start = self.next_index0 * self.bucket0_span;
            let end = start + self.bucket0_span;
            self.push_ready(exact(start, end));
        }
    }

    /// Pushes a level-0 bucket the caller already computed exactly —
    /// see [`SummaryTree::push_ready`].
    pub fn push_ready(&mut self, bucket: DigitalBucket) {
        self.push_level(0, bucket);
        self.next_index0 += 1;
    }

    fn ensure_level(&mut self, level: usize) {
        while self.levels.len() <= level {
            let span = self.span_at(self.levels.len());
            self.levels.push(SparseLevel::new(span));
        }
    }

    fn push_level(&mut self, level: usize, bucket: DigitalBucket) {
        self.ensure_level(level);
        let idx = self.levels[level].front_index + self.levels[level].metas.len() as u64;
        self.push_compressed(level, &bucket, idx);

        if (idx + 1).is_multiple_of(8) {
            if !group_retained(self.levels[level].front_index, idx) {
                self.ensure_level(level + 1);
                let parent = &mut self.levels[level + 1];
                skip_parent(&mut parent.front_index, parent.metas.is_empty(), idx / 8);
                return;
            }
            let len = self.levels[level].metas.len();
            let mut acc: Option<DigitalBucket> = None;
            for local in (len - 8)..len {
                let b = self.decompress_at(level, local);
                acc = Some(match acc {
                    Some(a) => DigitalBucket::merge(&a, &b),
                    None => b,
                });
            }
            self.push_level(level + 1, acc.expect("8 buckets"));
        }
    }

    fn push_compressed(&mut self, level: usize, bucket: &DigitalBucket, idx: u64) {
        let toggled = bucket.and ^ bucket.or;
        let mut toggled_channels = (0..8u8).filter(|&ch| toggled & (1 << ch) != 0);
        let inline = match toggled_channels.next() {
            Some(ch0) => (
                narrow_u32(bucket.high_count[ch0 as usize]),
                narrow_u32(bucket.edge_count[ch0 as usize]),
            ),
            None => (0, 0),
        };
        let rest: Vec<(u8, u32, u32)> = toggled_channels
            .map(|ch| {
                (
                    ch,
                    narrow_u32(bucket.high_count[ch as usize]),
                    narrow_u32(bucket.edge_count[ch as usize]),
                )
            })
            .collect();

        let lvl = &mut self.levels[level];
        lvl.metas.push_back(SparseMeta {
            and: bucket.and,
            or: bucket.or,
            first_value: bucket.first_value,
            last_value: bucket.last_value,
        });
        lvl.counts.push_back(inline);
        self.bytes += std::mem::size_of::<SparseMeta>() + lvl.counts.bytes_per_entry();
        if !rest.is_empty() {
            self.bytes += overflow_bytes(&rest);
            lvl.overflow.insert(idx, rest);
        }
    }

    fn decompress_at(&self, level: usize, local: usize) -> DigitalBucket {
        let lvl = &self.levels[level];
        let meta = lvl.metas[local];
        let inline = lvl.counts.get(local).expect("consistent index");
        let abs_idx = lvl.front_index + local as u64;
        let overflow = lvl.overflow.get(&abs_idx).map(|v| v.as_slice());
        decompress_digital(meta, inline, overflow, self.span_at(level))
    }

    fn get(&self, level: usize, idx: u64) -> Option<DigitalBucket> {
        let lvl = self.levels.get(level)?;
        if idx < lvl.front_index {
            return None;
        }
        let local = (idx - lvl.front_index) as usize;
        if local >= lvl.metas.len() {
            return None;
        }
        Some(self.decompress_at(level, local))
    }

    /// Exact summary of `[start, end)`, both multiples of `bucket0_span` —
    /// same contract as [`SummaryTree::sum_range`].
    pub fn sum_range(&self, start: u64, end: u64) -> Option<DigitalBucket> {
        debug_assert_eq!(start % self.bucket0_span, 0);
        debug_assert_eq!(end % self.bucket0_span, 0);
        if start >= end {
            return None;
        }
        let mut cur = start;
        let mut acc: Option<DigitalBucket> = None;
        for level in (0..self.levels.len()).rev() {
            let span = self.span_at(level);
            while cur + span <= end && cur.is_multiple_of(span) {
                let idx = cur / span;
                match self.get(level, idx) {
                    Some(b) => {
                        acc = Some(match acc {
                            Some(a) => DigitalBucket::merge(&a, &b),
                            None => b,
                        });
                        cur += span;
                    }
                    None => break,
                }
            }
        }
        if cur == end {
            acc
        } else {
            None
        }
    }

    /// Buckets (as their cheap-to-copy `and`/`or` meta) that start exactly
    /// at `start` and fit within `max_end`, coarsest first — same contract
    /// as [`SummaryTree::aligned_buckets_desc`], used by `runs_masked`'s
    /// bucket-skip fast path (only `and`/`or` are needed there, so this
    /// returns the meta directly rather than a fully decompressed bucket).
    fn aligned_buckets_desc(&self, start: u64, max_end: u64) -> Vec<(u64, SparseMeta)> {
        let mut out = Vec::new();
        for level in (0..self.levels.len()).rev() {
            let span = self.span_at(level);
            if start.is_multiple_of(span) && start + span <= max_end {
                if let Some(lvl) = self.levels.get(level) {
                    if start / span >= lvl.front_index {
                        let local = (start / span - lvl.front_index) as usize;
                        if let Some(&m) = lvl.metas.get(local) {
                            out.push((start + span, m));
                        }
                    }
                }
            }
        }
        out
    }

    /// Digital-bucket-and/or lookalike used by `runs_masked`: same shape
    /// as the fields `DigitalBucket` exposes, so callers don't need to
    /// know this tree stores something else internally.
    pub fn aligned_and_or_desc(&self, start: u64, max_end: u64) -> Vec<(u64, u8, u8)> {
        self.aligned_buckets_desc(start, max_end)
            .into_iter()
            .map(|(end, m)| (end, m.and, m.or))
            .collect()
    }

    /// Drop every bucket, at every level, entirely before `first_sample` —
    /// same contract as [`SummaryTree::evict_before`].
    pub fn evict_before(&mut self, first_sample: u64) -> usize {
        let mut freed = 0usize;
        for level in 0..self.levels.len() {
            let span = self.span_at(level);
            loop {
                let lvl = &mut self.levels[level];
                let bucket_start = lvl.front_index * span;
                if lvl.metas.is_empty() || bucket_start + span > first_sample {
                    break;
                }
                lvl.metas.pop_front();
                let count_bytes = lvl.counts.bytes_per_entry();
                lvl.counts.pop_front();
                if let Some(v) = lvl.overflow.remove(&lvl.front_index) {
                    freed += overflow_bytes(&v);
                }
                freed += std::mem::size_of::<SparseMeta>() + count_bytes;
                lvl.front_index += 1;
            }
        }
        self.bytes -= freed;
        freed
    }

    /// Approximate heap bytes retained across all levels.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// One overflow entry's cost: its channel counts plus an approximate
/// `BTreeMap` per-entry overhead.
fn overflow_bytes(rest: &[(u8, u32, u32)]) -> usize {
    std::mem::size_of_val(rest) + 48
}

/// Per-channel min/max/sum for one analog bucket (SPEC §2.8.3).
#[derive(Debug, Clone, Copy)]
pub struct AnalogChannelStat {
    pub min: i32,
    pub max: i32,
    pub sum: i64,
}

#[derive(Debug, Clone)]
pub struct AnalogBucket {
    pub count: u64,
    pub channels: Vec<AnalogChannelStat>,
}

impl AnalogBucket {
    pub fn of_sample(codes: &[i32]) -> Self {
        AnalogBucket {
            count: 1,
            channels: codes
                .iter()
                .map(|&c| AnalogChannelStat {
                    min: c,
                    max: c,
                    sum: c as i64,
                })
                .collect(),
        }
    }

    pub fn mean(&self, channel: usize) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.channels[channel].sum as f64 / self.count as f64
        }
    }
}

impl Bucket for AnalogBucket {
    fn merge(left: &Self, right: &Self) -> Self {
        debug_assert_eq!(left.channels.len(), right.channels.len());
        AnalogBucket {
            count: left.count + right.count,
            channels: left
                .channels
                .iter()
                .zip(right.channels.iter())
                .map(|(a, b)| AnalogChannelStat {
                    min: a.min.min(b.min),
                    max: a.max.max(b.max),
                    sum: a.sum + b.sum,
                })
                .collect(),
        }
    }

    fn approx_bytes(&self) -> usize {
        self.channels.len() * std::mem::size_of::<AnalogChannelStat>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level0_span_examples() {
        assert_eq!(level0_span(1_000_000), 1024); // nearest pow2 to 1000
        assert_eq!(level0_span(1_000), 64); // nearest pow2 to 1 is 1, floored to 64
        assert_eq!(level0_span(24_000_000), 32768); // nearest pow2 to 24000
    }

    fn sample_bucket(v: u8) -> DigitalBucket {
        DigitalBucket::of_sample(v)
    }

    #[test]
    fn digital_merge_counts_boundary_edge_once() {
        let a = sample_bucket(0x01);
        let b = sample_bucket(0x00);
        let merged = DigitalBucket::merge(&a, &b);
        assert_eq!(merged.edge_count[0], 1);
        assert_eq!(merged.count, 2);
        assert_eq!(merged.first_value, 0x01);
        assert_eq!(merged.last_value, 0x00);
    }

    #[test]
    fn digital_merge_no_boundary_edge_when_equal() {
        let a = sample_bucket(0x01);
        let b = sample_bucket(0x01);
        let merged = DigitalBucket::merge(&a, &b);
        assert_eq!(merged.edge_count[0], 0);
    }

    #[test]
    fn tree_closes_and_answers_full_range() {
        let mut tree = DigitalSummaryTree::new(64);
        // Feed 64*16 = 1024 samples, alternating per bucket so AND != OR at the
        // sample level but is exact per whole-bucket construction.
        let mut buckets_built: Vec<DigitalBucket> = Vec::new();
        tree.close_ready(1024, |start, end| {
            let len = end - start;
            let val = if (start / 64) % 2 == 0 { 0x00 } else { 0x01 };
            let mut b = DigitalBucket::of_sample(val);
            b.count = len;
            b.high_count[0] = if val == 1 { len } else { 0 };
            buckets_built.push(b);
            b
        });
        let summary = tree.sum_range(0, 1024).expect("fully covered");
        assert_eq!(summary.count, 1024);
        assert_eq!(summary.high_count[0], 512);
    }

    #[test]
    fn eviction_drops_only_entirely_before_buckets() {
        let mut tree = DigitalSummaryTree::new(64);
        tree.close_ready(64 * 20, |start, end| {
            let mut b = DigitalBucket::of_sample(0);
            b.count = end - start;
            b
        });
        let freed = tree.evict_before(64 * 5 + 10); // 5 buckets entirely before, 6th straddled
        assert!(freed > 0);
        // Bucket 5 (samples [320,384)) straddles 330 and must remain absent
        // from level-0 lookups that would otherwise use it as if it started
        // at 320 (it doesn't, once first_sample has moved to 330).
        assert!(tree.get(0, 5).is_some()); // bucket data itself still present
        assert!(tree.get(0, 4).is_none()); // bucket 4 fully evicted
    }

    /// Level-0 bucket `i` of a 64-sample-span analog tree: every sample
    /// equals `i`, so any merged range's min/max/sum identify exactly which
    /// buckets went into it.
    fn analog_bucket(i: u64) -> AnalogBucket {
        AnalogBucket {
            count: 64,
            channels: vec![AnalogChannelStat {
                min: i as i32,
                max: i as i32,
                sum: i as i64 * 64,
            }],
        }
    }

    fn exact_analog(first: u64, last_exclusive: u64) -> (u64, i32, i32, i64) {
        let sum = (first..last_exclusive).map(|i| i as i64 * 64).sum();
        (
            (last_exclusive - first) * 64,
            first as i32,
            last_exclusive as i32 - 1,
            sum,
        )
    }

    fn stat(b: &AnalogBucket) -> (u64, i32, i32, i64) {
        let c = b.channels[0];
        (b.count, c.min, c.max, c.sum)
    }

    /// Live-test regression (2026-09-15): a 4 MS/s scope recording under the
    /// memory budget panicked the ingest thread — `range(len - 8..)` with
    /// `len == 7` — the first time the tree grew a new top level after
    /// eviction had begun, and the poisoned lock then wedged every recording.
    #[test]
    fn a_new_level_after_eviction_skips_the_partly_evicted_parent() {
        let mut tree: SummaryTree<AnalogBucket> = SummaryTree::new(64);
        for i in 0..7 {
            tree.push_ready(analog_bucket(i));
        }
        tree.evict_before(64 * 3);
        // Bucket 7 completes group 0..8, of which 0..3 are gone: no parent.
        for i in 7..128 {
            tree.push_ready(analog_bucket(i));
            if i % 16 == 0 {
                tree.evict_before(64 * 3);
            }
        }
        assert!(tree.get(1, 0).is_none(), "partly evicted parent was built");
        assert!(
            tree.get(2, 0).is_none(),
            "partly evicted grandparent was built"
        );
        // Every parent built afterwards sits at its own index.
        assert_eq!(stat(tree.get(1, 1).unwrap()), exact_analog(8, 16));
        assert_eq!(stat(tree.get(1, 15).unwrap()), exact_analog(120, 128));
        assert_eq!(stat(tree.get(2, 1).unwrap()), exact_analog(64, 128));
        assert_eq!(
            stat(&tree.sum_range(64 * 8, 64 * 128).unwrap()),
            exact_analog(8, 128)
        );
        assert_eq!(
            stat(&tree.sum_range(64 * 3, 64 * 128).unwrap()),
            exact_analog(3, 128)
        );
    }

    #[test]
    fn digital_tree_skips_a_partly_evicted_parent_too() {
        let mut tree = DigitalSummaryTree::new(64);
        let bucket = |i: u64| {
            let mut b = DigitalBucket::of_sample((i & 1) as u8);
            b.count = 64;
            b.high_count[0] = if i & 1 == 1 { 64 } else { 0 };
            b
        };
        for i in 0..7 {
            tree.push_ready(bucket(i));
        }
        tree.evict_before(64 * 3);
        for i in 7..128 {
            tree.push_ready(bucket(i));
        }
        assert!(tree.get(1, 0).is_none());
        assert!(tree.get(2, 0).is_none());
        let level1 = tree.get(1, 1).expect("parent of 8..16");
        assert_eq!((level1.count, level1.high_count[0]), (512, 256));
        let level2 = tree.get(2, 1).expect("parent of 64..128");
        assert_eq!((level2.count, level2.high_count[0]), (4096, 2048));
        let all = tree.sum_range(64 * 3, 64 * 128).expect("retained span");
        assert_eq!((all.count, all.high_count[0]), (125 * 64, 63 * 64));
    }
}
