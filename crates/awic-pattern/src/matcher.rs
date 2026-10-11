//! The incremental matcher and the history-search convenience wrapper
//! (SPEC §2.9.3).

use std::ops::Range;

use crate::analog::AnalogMatcher;
use crate::digital::{BitSeqMatcher, ClockedMatcher, EdgeMatcher, PulseMatcher, StateMatcher};
use crate::pattern::Pattern;
use crate::types::{Match, Run};

/// A pattern being matched incrementally against a growing recording
/// (SPEC §2.9.3). One matcher is created per search or `rec.wait`; it holds
/// all state needed to resume across appended chunks.
pub enum Matcher {
    BitSeq(BitSeqMatcher),
    Clocked(ClockedMatcher),
    Edge(EdgeMatcher),
    Pulse(PulseMatcher),
    State(StateMatcher),
    Analog(AnalogMatcher),
}

impl Matcher {
    pub fn new(pattern: &Pattern) -> Self {
        match pattern {
            Pattern::BitSeq(p) => Matcher::BitSeq(BitSeqMatcher::new(
                p.bit,
                p.cell_samples,
                p.tol_cells,
                p.bits.clone(),
            )),
            Pattern::Clocked(p) => Matcher::Clocked(ClockedMatcher::new(
                p.data_bit,
                p.clk_bit,
                p.clk_dir,
                p.bits.clone(),
            )),
            Pattern::Edge(p) => Matcher::Edge(EdgeMatcher::new(p.bit, p.dir)),
            Pattern::Pulse(p) => Matcher::Pulse(PulseMatcher::new(p.bit, p.high, p.min, p.max)),
            Pattern::State(p) => Matcher::State(StateMatcher::new(p.mask, p.expected)),
            Pattern::Analog(p) => Matcher::Analog(AnalogMatcher::new(*p)),
        }
    }

    /// Feeds newly-known digital runs. `runs` must continue exactly where
    /// the previous call left off (its first run may repeat the previous
    /// call's last run, extended, if that run was still open); adjacent
    /// runs may carry equal values (a storage chunk/bucket boundary
    /// artifact) — the matcher merges them itself.
    ///
    /// `known_until` is accepted for symmetry with the recorder's ingest
    /// contract (SPEC §2.9.3) — a caller does not need to know which kind
    /// of pattern it is feeding — but every digital matcher here already
    /// derives "known so far" from each run's own `start`/`len`, so it is
    /// otherwise unused. A no-op (returns no matches) on an analog
    /// pattern: feed samples to it via [`Self::feed_analog`] instead.
    pub fn feed_runs(&mut self, runs: &[Run], known_until: u64) -> Vec<Match> {
        let _ = known_until;
        match self {
            Matcher::BitSeq(m) => m.feed(runs),
            Matcher::Clocked(m) => m.feed(runs),
            Matcher::Edge(m) => m.feed(runs),
            Matcher::Pulse(m) => m.feed(runs),
            Matcher::State(m) => m.feed(runs),
            Matcher::Analog(_) => Vec::new(),
        }
    }

    /// Feeds newly-known analog samples (`codes`, channel-interleaved raw
    /// codes starting at `first_sample`). A no-op on a digital pattern.
    pub fn feed_analog(&mut self, first_sample: u64, codes: &[u8]) -> Vec<Match> {
        match self {
            Matcher::Analog(m) => m.feed_analog(first_sample, codes),
            _ => Vec::new(),
        }
    }

    /// Storage-pruning hook (SPEC §2.8.3) for an analog pattern: `true` if
    /// the caller may skip decoding `[start, start+len)` because
    /// `min_code..=max_code` (a summary bucket's range) cannot contain a
    /// debounced crossing. Always `false` for a digital pattern — prune
    /// those with [`crate::Pattern::mask`] instead.
    pub fn try_skip_analog(&mut self, start: u64, len: u64, min_code: i32, max_code: i32) -> bool {
        match self {
            Matcher::Analog(m) => m.try_skip_analog(start, len, min_code, max_code),
            _ => false,
        }
    }

    /// Ends the stream: resolves anything left pending as if no more data
    /// will ever arrive (a pulse's still-open run is evaluated as closed,
    /// an in-progress bit-sequence segment is given a hard end). Used by
    /// [`search_runs`] because a history search covers a fixed range; a
    /// caller feeding a `Matcher` live should also call this once the
    /// recording it is reading stops (SPEC §2.9.3), so a pattern whose
    /// last decision depends on the final run closing is still resolved.
    pub fn finish(&mut self) -> Vec<Match> {
        match self {
            Matcher::BitSeq(m) => m.finish(),
            Matcher::Clocked(m) => m.finish(),
            Matcher::Edge(m) => m.finish(),
            Matcher::Pulse(m) => m.finish(),
            Matcher::State(m) => m.finish(),
            Matcher::Analog(_) => Vec::new(),
        }
    }

    /// Bit-sequence patterns only: how many sub-half-cell runs have been
    /// absorbed as glitches so far (SPEC §2.9.2 rule 2, §2.9.3 `glitches`).
    pub fn glitches_absorbed(&self) -> u64 {
        match self {
            Matcher::BitSeq(m) => m.glitches_absorbed(),
            _ => 0,
        }
    }

    /// Marks that the very first digital run this matcher will ever see is
    /// truncated at its left edge (SPEC §2.9): its `start`
    /// is where the caller's search began — the recording's retained start
    /// after an eviction, or a recording's very first run — not necessarily
    /// where the run truly began, and its `len` is only a lower bound. Call
    /// once, before the first [`Self::feed_runs`].
    ///
    /// Only bit-sequence and level/pulse patterns need this: an edge, state
    /// or clocked-data pattern only ever reports a transition *into* a run
    /// from a previously-observed one, and the very first run never has a
    /// "previous" — so it can never itself anchor a spurious match (see each
    /// matcher's own doc comment). A no-op for those, and for an analog
    /// pattern (which matches over exact samples, not runs of unknown
    /// history, so has no equivalent ambiguity).
    pub fn mark_first_run_open_left(&mut self) {
        match self {
            Matcher::BitSeq(m) => m.mark_first_run_open_left(),
            Matcher::Pulse(m) => m.mark_first_run_open_left(),
            Matcher::Clocked(_) | Matcher::Edge(_) | Matcher::State(_) | Matcher::Analog(_) => {}
        }
    }
}

/// One history search's result (SPEC §2.9.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResult {
    pub matches: Vec<Match>,
    /// `true` if `max_matches` was reached before the range was exhausted.
    pub truncated: bool,
    pub glitches: u64,
}

/// Searches `runs` — a digital pattern's run history over `range`
/// (half-open, in samples) — for up to `max_matches` occurrences.
///
/// This is exactly [`Matcher::feed_runs`] fed from the start of `runs` in
/// batches, finished at the end (SPEC §2.9.3: `t0_s`/`t1_s` bound a fixed
/// range, not a live one), with output clipped to `range`. `runs` should
/// include enough lead-in before `range.start` for the pattern to establish
/// state (an edge or state pattern needs one prior run to know the level it
/// is transitioning from); a match that genuinely depends on data before
/// the very first run given cannot be found.
pub fn search_runs<'a>(
    pattern: &Pattern,
    runs: impl IntoIterator<Item = &'a Run>,
    range: Range<u64>,
    max_matches: usize,
) -> SearchResult {
    let mut matcher = Matcher::new(pattern);
    let mut result = SearchResult::default();

    // Runs are fed in batches, not one at a time: each `feed_runs` call
    // pays a fixed bookkeeping cost (see `BitSeqMatcher::feed`) on top of
    // its O(edges) work, and this crate's own `search_runs` is the only
    // caller that would otherwise pay that cost per run instead of per
    // batch — a real recorder hands over a batch of newly-known runs at
    // a time anyway (SPEC §2.9.3). The batch size is a plain throughput
    // knob, not a correctness one (proved by the chunking-invariance
    // property test): any grouping yields identical matches.
    const BATCH: usize = 65536;
    let mut batch: Vec<Run> = Vec::with_capacity(BATCH);
    let mut iter = runs.into_iter().peekable();

    'feed: while iter.peek().is_some() {
        batch.clear();
        batch.extend(iter.by_ref().take(BATCH).copied());
        let Some(last) = batch.last() else { break };
        let known_until = last.start + last.len;
        let found = matcher.feed_runs(&batch, known_until);
        for m in found {
            if m.sample < range.start || m.sample >= range.end {
                continue;
            }
            result.matches.push(m);
            if result.matches.len() >= max_matches {
                result.truncated = true;
                break 'feed;
            }
        }
    }
    if !result.truncated {
        for m in matcher.finish() {
            if m.sample < range.start || m.sample >= range.end {
                continue;
            }
            result.matches.push(m);
            if result.matches.len() >= max_matches {
                result.truncated = true;
                break;
            }
        }
    }
    result.glitches = matcher.glitches_absorbed();
    result
}
