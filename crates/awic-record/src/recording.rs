//! `Recording`: the top-level type of `awic-record` (SPECIFICATION.md §2.8).
//!
//! Ties one [`crate::digital::DigitalStore`] or [`crate::analog::AnalogStore`]
//! to its [`crate::summary::SummaryTree`] and answers every query the server
//! (T18) and `awic-pattern` (T14) need: runs, masked runs, extraction,
//! summaries and views, plus eviction.

use crate::analog::AnalogStore;
use crate::digital::{clip_range, DigitalStore, Run};
use crate::error::{RecordError, Result, MAX_EXTRACT_SAMPLES};
use crate::summary::{AnalogBucket, Bucket, DigitalBucket, DigitalSummaryTree, SummaryTree};
use crate::view::{choose_item_span, ViewData, ViewKind};

/// What a recording holds (SPEC §2.8: "one recording per device, one
/// timebase per recording — a new rate, channel set or kind is a new
/// recording").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingKind {
    /// Packed byte stream, one byte per sample, bit *n* = channel D*n*.
    Digital,
    /// Interleaved raw codes, `sample_bits` wide (8 or 16).
    Analog { channel_count: u8, sample_bits: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingSpec {
    pub kind: RecordingKind,
    pub rate_hz: u64,
}

enum Storage {
    Digital(DigitalStore),
    Analog(AnalogStore),
}

enum Tree {
    Digital(DigitalSummaryTree),
    Analog(SummaryTree<AnalogBucket>),
}

/// Exact summary of an arbitrary span (§2.8.3), the result of `summarize`.
#[derive(Debug, Clone)]
pub enum Summary {
    Digital(DigitalBucket),
    Analog(AnalogBucket),
}

/// A bounded, compressed, in-memory, searchable history of one continuous
/// acquisition (SPEC §2.8). Pure data structure: no I/O, no tokio — the
/// server (T18) owns ingest and eviction scheduling and calls into this.
pub struct Recording {
    spec: RecordingSpec,
    storage: Storage,
    tree: Tree,
}

fn align_up(v: u64, span: u64) -> u64 {
    v.div_ceil(span) * span
}

impl Recording {
    pub fn new(spec: RecordingSpec) -> Self {
        let span0 = crate::summary::level0_span(spec.rate_hz);
        let (storage, tree) = match spec.kind {
            RecordingKind::Digital => (
                Storage::Digital(DigitalStore::new()),
                Tree::Digital(DigitalSummaryTree::new(span0)),
            ),
            RecordingKind::Analog {
                channel_count,
                sample_bits,
            } => {
                let mut store = AnalogStore::new(channel_count as usize, sample_bits);
                // Lets the store fold each bucket's min/max/sum into its own
                // de-interleave loop instead of `append_analog` below
                // re-scanning the same samples afterwards.
                store.set_level0_span(span0);
                (
                    Storage::Analog(store),
                    Tree::Analog(SummaryTree::new(span0)),
                )
            }
        };
        Recording {
            spec,
            storage,
            tree,
        }
    }

    pub fn spec(&self) -> RecordingSpec {
        self.spec
    }

    // ---- append ---------------------------------------------------------

    pub fn append_packed(&mut self, data: &[u8]) -> Result<()> {
        let (store, tree) = match (&mut self.storage, &mut self.tree) {
            (Storage::Digital(s), Tree::Digital(t)) => (s, t),
            _ => {
                return Err(RecordError::WrongKind {
                    expected: "digital",
                    actual: "analog",
                })
            }
        };
        store.append_packed(data);
        let head = store.head_sample();
        tree.close_ready(head, |s, e| store.exact_bucket(s, e));
        Ok(())
    }

    pub fn append_analog(&mut self, data: &[u8]) -> Result<()> {
        let (store, tree) = match (&mut self.storage, &mut self.tree) {
            (Storage::Analog(s), Tree::Analog(t)) => (s, t),
            _ => {
                return Err(RecordError::WrongKind {
                    expected: "analog",
                    actual: "digital",
                })
            }
        };
        store.append_analog(data)?;
        // The store already computed each completed bucket's min/max/sum
        // while de-interleaving (see `AnalogStore::append_analog`); push
        // them straight into the tree rather than asking it to re-derive
        // them via `exact()` (which would rescan the same samples).
        for bucket in store.drain_ready_buckets() {
            tree.push_ready(bucket);
        }
        Ok(())
    }

    // ---- accessors --------------------------------------------------------

    pub fn first_sample(&self) -> u64 {
        match &self.storage {
            Storage::Digital(s) => s.first_sample(),
            Storage::Analog(s) => s.first_sample(),
        }
    }

    /// Exclusive end of the retained span: the sample index that would be
    /// appended next.
    pub fn head_sample(&self) -> u64 {
        match &self.storage {
            Storage::Digital(s) => s.head_sample(),
            Storage::Analog(s) => s.head_sample(),
        }
    }

    /// Approximate heap bytes actually used: stored blocks plus the summary
    /// tree.
    pub fn bytes(&self) -> usize {
        let tree_bytes = match &self.tree {
            Tree::Digital(t) => t.bytes(),
            Tree::Analog(t) => t.bytes(),
        };
        let storage_bytes = match &self.storage {
            Storage::Digital(s) => s.bytes(),
            Storage::Analog(s) => s.bytes(),
        };
        storage_bytes + tree_bytes
    }

    /// What `[start, end)` would cost stored packed/raw (uncompressed),
    /// for the compression ratio `rec.list` reports.
    pub fn raw_bytes(&self, start: u64, end: u64) -> u64 {
        let n = end.saturating_sub(start);
        match self.spec.kind {
            RecordingKind::Digital => n,
            RecordingKind::Analog {
                channel_count,
                sample_bits,
            } => n * channel_count as u64 * if sample_bits == 16 { 2 } else { 1 },
        }
    }

    // ---- eviction ---------------------------------------------------------

    /// Drops the oldest block (and the summary buckets entirely before the
    /// new `first_sample`). Returns approximate bytes freed; 0 if there is
    /// no finalized block left to drop.
    pub fn evict_oldest_block(&mut self) -> usize {
        let freed_storage = match &mut self.storage {
            Storage::Digital(s) => s.evict_oldest_block(),
            Storage::Analog(s) => s.evict_oldest_block(),
        };
        if freed_storage == 0 {
            return 0;
        }
        let first_sample = self.first_sample();
        let freed_tree = match &mut self.tree {
            Tree::Digital(t) => t.evict_before(first_sample),
            Tree::Analog(t) => t.evict_before(first_sample),
        };
        freed_storage + freed_tree
    }

    // ---- digital queries ----------------------------------------------------

    pub fn runs(&self, start: u64, end: u64) -> Result<Vec<Run>> {
        let store = self.digital_store()?;
        let (start, end) = clip_range(start, end, store.first_sample(), store.head_sample())?;
        Ok(store.runs(start, end))
    }

    /// Runs of `value & mask`, adjacent equal runs merged, using the summary
    /// tree to skip whole buckets whose masked bits are constant (SPEC
    /// §2.8.3) — the fast path pattern search uses.
    pub fn runs_masked(&self, start: u64, end: u64, mask: u8) -> Result<Vec<Run>> {
        let store = self.digital_store()?;
        let Tree::Digital(tree) = &self.tree else {
            unreachable!()
        };
        let (start, end) = clip_range(start, end, store.first_sample(), store.head_sample())?;
        let span0 = tree.bucket0_span();

        let mut out: Vec<Run> = Vec::new();
        let push = |out: &mut Vec<Run>, s: u64, l: u64, v: u8| {
            if l == 0 {
                return;
            }
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
        };

        let mut cur = start;
        while cur < end {
            let shortcut = tree
                .aligned_and_or_desc(cur, end)
                .into_iter()
                .find(|&(_, and, or)| (and & mask) == (or & mask));
            if let Some((bucket_end, and, _or)) = shortcut {
                push(&mut out, cur, bucket_end - cur, and & mask);
                cur = bucket_end;
                continue;
            }
            let next = align_up(cur + 1, span0).min(end);
            for run in store.runs(cur, next) {
                push(&mut out, run.start, run.len, run.value & mask);
            }
            cur = next;
        }
        Ok(out)
    }

    pub fn extract_packed(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let store = self.digital_store()?;
        let (start, end) = clip_range(start, end, store.first_sample(), store.head_sample())?;
        check_extract_size(end - start)?;
        Ok(store.extract_packed(start, end))
    }

    pub fn extract_analog(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let store = self.analog_store()?;
        let (start, end) = clip_range(start, end, store.first_sample(), store.head_sample())?;
        check_extract_size(end - start)?;
        Ok(store.extract_analog(start, end))
    }

    // ---- summarize ----------------------------------------------------------

    pub fn summarize(&self, start: u64, end: u64) -> Result<Summary> {
        match (&self.storage, &self.tree) {
            (Storage::Digital(store), Tree::Digital(tree)) => {
                let (start, end) =
                    clip_range(start, end, store.first_sample(), store.head_sample())?;
                Ok(Summary::Digital(summarize_digital_span(
                    tree,
                    start,
                    end,
                    |s, e| store.exact_bucket(s, e),
                )))
            }
            (Storage::Analog(store), Tree::Analog(tree)) => {
                let (start, end) =
                    clip_range(start, end, store.first_sample(), store.head_sample())?;
                Ok(Summary::Analog(summarize_span(tree, start, end, |s, e| {
                    store.exact_bucket(s, e)
                })))
            }
            _ => unreachable!("storage and tree kinds always match"),
        }
    }

    // ---- view -----------------------------------------------------------

    /// The representation choice of SPEC §3.4.
    pub fn view(&self, start: u64, end: u64, columns: u32) -> Result<ViewData> {
        match &self.storage {
            Storage::Digital(store) => {
                let (start, end) =
                    clip_range(start, end, store.first_sample(), store.head_sample())?;
                self.view_digital(store, start, end, columns)
            }
            Storage::Analog(store) => {
                let (start, end) =
                    clip_range(start, end, store.first_sample(), store.head_sample())?;
                self.view_analog(store, start, end, columns)
            }
        }
    }

    fn view_digital(
        &self,
        store: &DigitalStore,
        start: u64,
        end: u64,
        columns: u32,
    ) -> Result<ViewData> {
        let runs = store.runs(start, end);
        if runs.len() as u64 <= 4 * columns as u64 {
            let mut payload = Vec::new();
            for r in &runs {
                payload.push(r.value);
                crate::varint::write_uvarint(&mut payload, r.len);
            }
            return Ok(ViewData {
                kind: ViewKind::DigitalRuns,
                first_sample: start,
                samples_per_item: 1,
                item_count: runs.len() as u64,
                channel_count: 8,
                sample_bits: 8,
                payload,
            });
        }

        let Tree::Digital(tree) = &self.tree else {
            unreachable!()
        };
        let (first_sample, span, item_count) = choose_item_span(start, end, columns);
        let mut payload = Vec::with_capacity(item_count as usize * 2);
        for i in 0..item_count {
            let item_start = first_sample + i * span;
            let item_end = (item_start + span).min(end);
            let item_start = item_start.max(start);
            let bucket =
                summarize_digital_span(tree, item_start, item_end, |s, e| store.exact_bucket(s, e));
            payload.push(bucket.and);
            payload.push(bucket.or);
        }
        Ok(ViewData {
            kind: ViewKind::DigitalSummary,
            first_sample,
            samples_per_item: span,
            item_count,
            channel_count: 8,
            sample_bits: 8,
            payload,
        })
    }

    fn view_analog(
        &self,
        store: &AnalogStore,
        start: u64,
        end: u64,
        columns: u32,
    ) -> Result<ViewData> {
        let n = end - start;
        let channel_count = store.channel_count() as u8;
        let sample_bits = store.sample_bits();
        if n <= 2 * columns as u64 {
            return Ok(ViewData {
                kind: ViewKind::AnalogRaw,
                first_sample: start,
                samples_per_item: 1,
                item_count: n,
                channel_count,
                sample_bits,
                payload: store.extract_analog(start, end),
            });
        }

        let Tree::Analog(tree) = &self.tree else {
            unreachable!()
        };
        let (first_sample, span, item_count) = choose_item_span(start, end, columns);
        let write_code = |buf: &mut Vec<u8>, v: i32| {
            if sample_bits == 16 {
                buf.extend_from_slice(&(v as i16).to_le_bytes());
            } else {
                buf.push(v as u8);
            }
        };
        let mut buckets: Vec<AnalogBucket> = Vec::with_capacity(item_count as usize);
        for i in 0..item_count {
            let item_start = (first_sample + i * span).max(start);
            let item_end = (first_sample + (i + 1) * span).min(end);
            buckets.push(summarize_span(tree, item_start, item_end, |s, e| {
                store.exact_bucket(s, e)
            }));
        }
        // channel-major payload: all items for channel 0, then channel 1, ...
        let mut payload = Vec::new();
        for ch in 0..channel_count as usize {
            for b in &buckets {
                write_code(&mut payload, b.channels[ch].min);
                write_code(&mut payload, b.channels[ch].max);
            }
        }
        Ok(ViewData {
            kind: ViewKind::AnalogSummary,
            first_sample,
            samples_per_item: span,
            item_count,
            channel_count,
            sample_bits,
            payload,
        })
    }

    // ---- kind guards ------------------------------------------------------

    fn digital_store(&self) -> Result<&DigitalStore> {
        match &self.storage {
            Storage::Digital(s) => Ok(s),
            Storage::Analog(_) => Err(RecordError::WrongKind {
                expected: "digital",
                actual: "analog",
            }),
        }
    }

    fn analog_store(&self) -> Result<&AnalogStore> {
        match &self.storage {
            Storage::Analog(s) => Ok(s),
            Storage::Digital(_) => Err(RecordError::WrongKind {
                expected: "analog",
                actual: "digital",
            }),
        }
    }
}

fn check_extract_size(samples: u64) -> Result<()> {
    if samples > MAX_EXTRACT_SAMPLES {
        Err(RecordError::ExtractTooLarge { samples })
    } else {
        Ok(())
    }
}

/// Exact summary of `[start, end)`: full tree buckets for the aligned
/// interior, exact (raw/run-derived) computation for the partial edges
/// (SPEC §2.8.3).
fn summarize_span<B: Bucket>(
    tree: &SummaryTree<B>,
    start: u64,
    end: u64,
    exact: impl Fn(u64, u64) -> B,
) -> B {
    let span0 = tree.bucket0_span();
    let mid_start = align_up(start, span0).min(end);
    let mid_end_raw = end - end % span0;
    let mid_end = mid_end_raw.max(mid_start).min(end);

    let mut parts: Vec<B> = Vec::new();
    if start < mid_start {
        parts.push(exact(start, mid_start));
    }
    if mid_start < mid_end {
        let bucket = tree
            .sum_range(mid_start, mid_end)
            .unwrap_or_else(|| exact(mid_start, mid_end));
        parts.push(bucket);
    }
    if mid_end < end {
        parts.push(exact(mid_end, end));
    }
    parts
        .into_iter()
        .reduce(|a, b| B::merge(&a, &b))
        .unwrap_or_else(|| exact(start, end))
}

/// Same algorithm as [`summarize_span`], for [`DigitalSummaryTree`] — which
/// isn't a `SummaryTree<B: Bucket>` (it stores a compressed representation
/// internally; see its doc comment), so it can't share that generic
/// function, but the interior/edges decomposition is identical.
fn summarize_digital_span(
    tree: &DigitalSummaryTree,
    start: u64,
    end: u64,
    exact: impl Fn(u64, u64) -> DigitalBucket,
) -> DigitalBucket {
    let span0 = tree.bucket0_span();
    let mid_start = align_up(start, span0).min(end);
    let mid_end_raw = end - end % span0;
    let mid_end = mid_end_raw.max(mid_start).min(end);

    let mut parts: Vec<DigitalBucket> = Vec::new();
    if start < mid_start {
        parts.push(exact(start, mid_start));
    }
    if mid_start < mid_end {
        let bucket = tree
            .sum_range(mid_start, mid_end)
            .unwrap_or_else(|| exact(mid_start, mid_end));
        parts.push(bucket);
    }
    if mid_end < end {
        parts.push(exact(mid_end, end));
    }
    parts
        .into_iter()
        .reduce(|a, b| DigitalBucket::merge(&a, &b))
        .unwrap_or_else(|| exact(start, end))
}
