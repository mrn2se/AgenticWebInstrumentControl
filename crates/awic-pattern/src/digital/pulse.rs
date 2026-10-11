//! `D1 high >2ms <5ms` / `D1 low >=10ms` (SPEC §2.9.1).
//!
//! Both bounds given needs the pulse's closing edge to know its exact
//! width. A lower bound alone is a "level held": it is decidable as soon as
//! the held duration reaches the bound, even while the run is still open.

use super::support::RunMerger;
use crate::pattern::Cmp;
use crate::types::{Match, Run};

pub struct PulseMatcher {
    high: bool,
    min: Option<(Cmp, f64)>,
    max: Option<(Cmp, f64)>,
    merger: RunMerger,
    /// Start sample of the open run we already reported a "held" match
    /// for, so a later call (once it finally closes) does not report it
    /// again.
    already_reported: Option<u64>,
    /// Set by [`Self::mark_first_run_open_left`]: the very first run this
    /// matcher will ever see is truncated at its left edge (SPEC §2.9) —
    /// its `start` is the search window's boundary, not
    /// necessarily the level's true onset, so a pulse "starting" there
    /// cannot be reported (there is no way to know it did not begin
    /// earlier, unlike a bit-sequence's leading cells, a level pattern has
    /// no certain edge to align to instead — the onset *is* what "sample"
    /// means here). `first_run_start` fixes which run that is, the same
    /// self-invalidating way `BitSeqMatcher` does: run starts strictly
    /// increase, so a later run can never coincide with the original one.
    open_left_requested: bool,
    first_run_start: Option<u64>,
    /// Reused across calls so a run closing costs no heap allocation.
    closed_scratch: Vec<Run>,
}

impl PulseMatcher {
    pub fn new(bit: u8, high: bool, min: Option<(Cmp, f64)>, max: Option<(Cmp, f64)>) -> Self {
        Self {
            high,
            min,
            max,
            merger: RunMerger::new(1 << bit),
            already_reported: None,
            open_left_requested: false,
            first_run_start: None,
            closed_scratch: Vec::new(),
        }
    }

    /// See the field doc comment on `open_left_requested`. Call before the
    /// first [`Self::feed`].
    pub fn mark_first_run_open_left(&mut self) {
        self.open_left_requested = true;
    }

    fn is_open_left(&self, start: u64) -> bool {
        self.open_left_requested && self.first_run_start == Some(start)
    }

    fn is_target_level(&self, value: u8) -> bool {
        (value != 0) == self.high
    }

    fn check_closed(&mut self, r: Run, out: &mut Vec<Match>) {
        if self.already_reported == Some(r.start) {
            self.already_reported = None;
            return; // already reported while open (the held case); do not duplicate
        }
        if !self.is_target_level(r.value) {
            return;
        }
        if self.is_open_left(r.start) {
            return; // this level's true onset is unknown; do not report it
        }
        let min_ok = self.min.map(|(c, t)| c.holds(r.len, t)).unwrap_or(true);
        let max_ok = self.max.map(|(c, t)| c.holds(r.len, t)).unwrap_or(true);
        if min_ok && max_ok {
            out.push(Match {
                sample: r.start,
                decided_at: r.start + r.len,
            });
        }
    }

    /// A lower bound with no upper bound can fire while the run is still
    /// open, as soon as its known duration reaches the bound.
    fn check_open(&mut self, r: Run, out: &mut Vec<Match>) {
        if self.max.is_some() || !self.is_target_level(r.value) {
            return;
        }
        let Some((cmp, threshold)) = self.min else {
            return;
        };
        if self.already_reported == Some(r.start) {
            return;
        }
        if self.is_open_left(r.start) {
            return; // this level's true onset is unknown; do not report it
        }
        let need = cmp.min_len_satisfying(threshold);
        if r.len >= need {
            out.push(Match {
                sample: r.start,
                decided_at: r.start + need,
            });
            self.already_reported = Some(r.start);
        }
    }

    pub fn feed(&mut self, runs: &[Run]) -> Vec<Match> {
        if self.first_run_start.is_none() {
            if let Some(first) = runs.first() {
                self.first_run_start = Some(first.start);
            }
        }
        let mut out = Vec::new();
        let mut closed = std::mem::take(&mut self.closed_scratch);
        closed.clear();
        self.merger.feed(runs, &mut closed);
        for r in closed.drain(..) {
            self.check_closed(r, &mut out);
        }
        self.closed_scratch = closed;
        if let Some(open) = self.merger.open() {
            self.check_open(open, &mut out);
        }
        out
    }

    /// Ends the stream: the still-open run will never grow further, so
    /// evaluate it as closed (used by [`crate::search_runs`]).
    pub fn finish(&mut self) -> Vec<Match> {
        let mut out = Vec::new();
        if let Some(r) = self.merger.finish() {
            self.check_closed(r, &mut out);
        }
        out
    }
}
