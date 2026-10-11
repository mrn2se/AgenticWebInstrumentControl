//! `D0 f=1000 100010x101` (SPEC §2.9.1, §2.9.2): edge-anchored bit-sequence
//! matching, chosen by SPEC §15 S-01 option A.
//!
//! Pipeline, in order:
//! 1. [`super::support::RunMerger`] folds chunk-boundary artifacts (two
//!    adjacent runs the storage happened to split with equal value) into
//!    one run, and separates the still-growing tail from closed runs.
//! 2. [`GlitchFilter`] absorbs runs shorter than half a bit cell into their
//!    (same-valued, by alternation) neighbours before either is used —
//!    that requires holding the run *before* a candidate glitch until the
//!    run *after* it is known not to need the same treatment.
//! 3. [`scan_segment`] does the actual pattern search over the resulting
//!    cell-runs, restarting at every off-rate run (a barrier no match may
//!    span) and reporting a match as soon as it is decidable — including
//!    while its tail cells are covered only by the still-open run, once
//!    that run's duration guarantees them regardless of further growth.
//!
//! Candidates are tried only at run boundaries, never mid-run: matching is
//! edge-anchored, this keeps cost to O(edges) (SPEC §2.9.2), and it is
//! exactly what makes "inside one run longer than the pattern, only the
//! first position is reported" (rule 5) fall out for free — a long run has
//! only one boundary, hence only one candidate.

use super::support::RunMerger;
use crate::pattern::BitVal;
use crate::types::{Match, Run};

const EPS: f64 = 1e-6;

/// `x.round()` for `x >= 0`, without going through `f64::round`'s libm
/// call — this runs once per run touched (SPEC §2.9.2: cost is meant to be
/// O(edges)), so it must be cheap. Correct because `x` is never negative
/// here (a sample count divided by a positive cell width).
fn round_nonneg(x: f64) -> u64 {
    (x + 0.5) as u64 // `as` truncates toward zero, i.e. floors for x >= 0
}

/// `x.ceil()` for `x >= 0`, same reasoning as [`round_nonneg`].
fn ceil_nonneg(x: f64) -> u64 {
    let truncated = x as u64;
    if (truncated as f64) < x {
        truncated + 1
    } else {
        truncated
    }
}

fn cell_count(len: u64, cell_samples: f64) -> u64 {
    round_nonneg(len as f64 / cell_samples)
}

/// Smallest sample length that guarantees `cell_count` returns at least `m`
/// no matter how much further the run grows (task note: "d·f ≥ k − 0.5").
fn min_len_for_cells(m: u64, cell_samples: f64) -> u64 {
    let x = ((m as f64) - 0.5) * cell_samples - EPS;
    if x <= 0.0 {
        0
    } else {
        ceil_nonneg(x)
    }
}

fn is_glitch(len: u64, cell_samples: f64) -> bool {
    cell_count(len, cell_samples) == 0
}

fn is_off_rate(len: u64, cell_samples: f64, tol_cells: f64) -> bool {
    let n = cell_count(len, cell_samples);
    ((len as f64 / cell_samples) - n as f64).abs() > tol_cells + EPS
}

/// Absorbs sub-half-cell runs into their neighbours (SPEC §2.9.2 rule 2),
/// holding back exactly one run of lookback: a run is only safe to hand
/// out (finalize) once the run right after it is known *not* to be a
/// glitch that must merge into it.
#[derive(Default)]
struct GlitchFilter {
    held: Option<Run>,
    /// True right after a glitch was folded into `held`: the very next
    /// run, whatever it is, is that glitch's other neighbour and (by
    /// alternation) shares `held`'s value, so it merges unconditionally.
    absorbing: bool,
    glitches: u64,
}

impl GlitchFilter {
    fn ingest_closed(&mut self, r: Run, cell_samples: f64, out: &mut Vec<Run>) {
        if is_glitch(r.len, cell_samples) {
            self.glitches += 1;
            if let Some(h) = &mut self.held {
                h.len += r.len;
                self.absorbing = true;
            }
            // A glitch with nothing held yet (start of stream) has no
            // neighbour to extend; it is simply dropped.
            return;
        }
        if self.absorbing {
            if let Some(h) = &mut self.held {
                h.len = r.start + r.len - h.start;
            } else {
                self.held = Some(r);
            }
            self.absorbing = false;
            return;
        }
        if let Some(h) = self.held.take() {
            out.push(h);
        }
        self.held = Some(r);
    }

    /// The best current knowledge of the trailing, not-yet-committed run,
    /// for early-deciding a match into it (see module docs). `open` is the
    /// run [`RunMerger`] is still accumulating.
    fn effective_tail(&self, open: Option<Run>, cell_samples: f64) -> Option<Run> {
        let open = open?;
        if self.absorbing {
            let h = self.held.expect("absorbing implies held is Some");
            return Some(Run {
                start: h.start,
                len: h.len + open.len,
                value: h.value,
            });
        }
        if self.held.is_none() && open.len as f64 > 0.5 * cell_samples + EPS {
            return Some(open);
        }
        None // too short to be sure it is not a glitch, or `held` is still ambiguous
    }

    /// Ends the stream: nothing more can arrive to absorb into `held`.
    fn finish(&mut self) -> Option<Run> {
        self.absorbing = false;
        self.held.take()
    }
}

/// `table[pos][level as usize]` = how many consecutive pattern positions
/// starting at `pos` are all compatible with `level` — i.e. the length a
/// run entered at its natural start could safely contribute there before
/// *its* value would contradict the pattern. Precomputed once per pattern
/// (SPEC §2.9.2: search cost is O(edges), so this must not be
/// recomputed per run touched — with it, every candidate/continuation
/// check below is an O(1) lookup instead of an O(pattern length) scan).
struct KTable(Vec<[u32; 2]>);

impl KTable {
    fn new(bits: &[BitVal]) -> Self {
        let mut table = vec![[0u32; 2]; bits.len() + 1];
        for pos in (0..bits.len()).rev() {
            let next = table[pos + 1];
            table[pos] = [
                if bits[pos].matches(false) {
                    next[0] + 1
                } else {
                    0
                },
                if bits[pos].matches(true) {
                    next[1] + 1
                } else {
                    0
                },
            ];
        }
        Self(table)
    }

    fn get(&self, pos: usize, level: bool) -> u64 {
        self.0[pos][level as usize] as u64
    }
}

/// Scans `segment` (finalized, on-rate, alternating-value runs) from
/// `*cursor` for non-overlapping, earliest-first matches of `bits`,
/// extending the last one into `tail` if given. Stops at the first
/// undecided candidate, leaving `*cursor` there to retry with more data.
///
/// A run's cells are all identical, so a run longer than the pattern's own
/// leading same-value run cannot be entered at offset 0 when more pattern
/// follows: the surplus leading cells would still be *this* value when the
/// pattern needs a change. The fix is greedy and provably optimal (using
/// fewer cells never helps: by construction the very next pattern position
/// is guaranteed to match the *next* run's value, since runs alternate) —
/// enter the candidate run at the offset that leaves it exactly enough
/// cells to cover its safe leading run, i.e. at its *tail*.
///
/// `open_left_start` is `Some(w)` when the caller has marked the run
/// starting at sample `w` as truncated at its left edge (SPEC §2.9): its
/// `start` is where the search window begins, not where
/// the run truly began, and its `len` is only a lower bound. It applies
/// only to a candidate whose own `start` is literally `w` — which, because
/// run starts strictly increase along the stream, can only ever be the
/// original run itself, never a later one, so no separate bookkeeping is
/// needed to "forget" it once `segment[0]` moves on (trimmed, or a barrier
/// resets the segment): a later run's `start` is simply never `w`.
///
/// Entering that run at offset 0 (as the two branches below do for an
/// ordinary run, aligning to its known *start*) would report a match
/// anchored on a fictitious instant — the spurious "0.0000 s" match seen on
/// hardware. Aligning to the run's known *end* instead (its right edge is a
/// real, observed transition, truncation or not) sidesteps that — unless
/// doing so uses the run's entire visible length with no cells to spare, in
/// which case the computed anchor collapses back onto the fictitious start
/// and the candidate is refused outright, exactly mirroring how the
/// open-right tail run may only complete a match once its length
/// guarantees it regardless of further growth.
/// The matcher's fixed, per-search configuration — grouped so `scan_segment`
/// (called from several places with the same three values every time)
/// doesn't need eight separate parameters.
struct ScanParams<'a> {
    k: &'a KTable,
    pattern_len: usize,
    cell_samples: f64,
}

fn scan_segment(
    segment: &[Run],
    tail: Option<Run>,
    params: &ScanParams<'_>,
    cursor: &mut usize,
    out: &mut Vec<Match>,
    open_left_start: Option<u64>,
) {
    let &ScanParams {
        k,
        pattern_len,
        cell_samples,
    } = params;
    loop {
        let candidate_run = if *cursor < segment.len() {
            segment[*cursor]
        } else if *cursor == segment.len() {
            // No finalized run to start at yet, but the still-open tail
            // might itself be long enough to be, and complete, a candidate
            // on its own (SPEC task note: decided even while open, once its
            // duration guarantees the cells needed). It can never serve as
            // a *non-final* segment of a candidate here — there is by
            // definition no run after it yet — so if the pattern needs a
            // different value afterwards, we simply cannot know yet.
            let Some(t) = tail else { break };
            if Some(t.start) == open_left_start {
                // This still-open run is *also* the search's very first —
                // truncated on the left (unknown true start) and not yet
                // closed on the right (no certain end either). With neither
                // edge certain, there is no anchor to report a match at;
                // wait for it to close (segment[0] then gets the ordinary
                // `open_left_start` treatment above) or for more data.
                break;
            }
            let level0 = t.value != 0;
            let k0 = k.get(0, level0) as usize;
            if k0 < pattern_len {
                break; // more pattern needed than this run alone can ever give; wait
            }
            let min_len = min_len_for_cells(pattern_len as u64, cell_samples);
            if t.len < min_len {
                break; // not decided yet; retry once it grows or closes
            }
            out.push(Match {
                sample: t.start,
                decided_at: t.start + min_len,
            });
            *cursor = segment.len() + 1;
            continue;
        } else {
            break;
        };
        let level0 = candidate_run.value != 0;
        let n0 = cell_count(candidate_run.len, cell_samples);
        let k0 = k.get(0, level0);
        let take0 = k0.min(n0);
        if take0 == 0 {
            // Even one cell of this run contradicts the pattern's first
            // required bit: no candidate can start anywhere in it.
            *cursor += 1;
            continue;
        }
        let is_open_left_candidate = Some(candidate_run.start) == open_left_start;
        let (offset_cells, take0) = if is_open_left_candidate {
            // Always align to the run's certain right edge (see the doc
            // comment above) — never the two "use the run's start" branches
            // below, which assume `candidate_run.start` is real.
            let capped = take0.min(pattern_len as u64);
            (n0.saturating_sub(capped), capped)
        } else if take0 as usize >= pattern_len {
            (0u64, pattern_len as u64) // self-contained: use the run's start (rule 5)
        } else if take0 == n0 {
            (0u64, take0) // uses the whole run, nothing left over to skip
        } else {
            (n0 - take0, take0) // must align with the run's natural end
        };
        if is_open_left_candidate && offset_cells == 0 {
            // No slack: every visible cell of this run is needed, so the
            // computed anchor coincides exactly with its fictitious start.
            // This candidate cannot be placed anywhere certain — refuse it
            // (it is a lower bound, not a known run) and move on; a
            // genuine match starting later is still found normally.
            *cursor += 1;
            continue;
        }
        let candidate_sample =
            candidate_run.start + round_nonneg(offset_cells as f64 * cell_samples);

        let mut pos = take0 as usize;
        let mut j = *cursor + 1;
        let mut ok = true;
        let mut decided = pos >= pattern_len;
        // The earliest sample by which this run's contribution (`take0`
        // cells) is guaranteed, whatever it does afterwards — the same
        // reasoning as the tail's "decided early" case applies to *any*
        // run, not only a still-open one (needed for chunking invariance:
        // whether a run is already finalized by the time we get here must
        // not change when we say the match became certain).
        let mut decided_at = candidate_run.start + min_len_for_cells(take0, cell_samples);
        let mut used_tail = false;

        while pos < pattern_len {
            if j < segment.len() {
                let run = segment[j];
                let n = cell_count(run.len, cell_samples);
                let take = n.min((pattern_len - pos) as u64);
                let level = run.value != 0;
                if take > k.get(pos, level) {
                    ok = false;
                    decided = true;
                    break;
                }
                pos += take as usize;
                j += 1;
                if pos >= pattern_len {
                    // This run completes the match: certain as soon as it
                    // covers `take` cells, not only once it fully closes.
                    decided = true;
                    decided_at = run.start + min_len_for_cells(take, cell_samples);
                } else {
                    // More pattern follows, in a run we have not seen yet:
                    // that requires this run to have actually ended.
                    decided_at = run.start + run.len;
                }
            } else if let Some(t) = tail {
                let need = (pattern_len - pos) as u64;
                let level = t.value != 0;
                let k_tail = k.get(pos, level);
                if k_tail < need {
                    // This run's value cannot carry the whole remainder of
                    // the pattern; a run we have not seen yet would be
                    // needed after it. Not a failure — just not decidable
                    // until more data arrives (it might still be a glitch
                    // that ends up absorbed, or simply close and continue).
                    break;
                }
                let min_len = min_len_for_cells(need, cell_samples);
                if t.len >= min_len {
                    used_tail = true;
                    ok = true;
                    decided = true;
                    decided_at = t.start + min_len;
                }
                break;
            } else {
                // No more finalized runs and no tail at all: the range
                // simply ended (used by `finish`/one-shot search).
                ok = false;
                decided = true;
                break;
            }
        }

        if !decided {
            break; // retry this exact candidate once more data arrives
        }
        if ok {
            out.push(Match {
                sample: candidate_sample,
                decided_at,
            });
            *cursor = if used_tail { segment.len() + 1 } else { j };
        } else {
            *cursor += 1;
        }
    }
}

pub struct BitSeqMatcher {
    cell_samples: f64,
    tol_cells: f64,
    pattern_len: usize,
    k: KTable,
    merger: RunMerger,
    filter: GlitchFilter,
    /// Finalized, on-rate runs of the current segment, not yet fully
    /// consumed by a match decision.
    segment: Vec<Run>,
    cursor: usize,
    /// Set by [`Self::mark_first_run_open_left`]: the very first run this
    /// matcher will ever see is truncated at its left edge (SPEC §2.9), not
    /// a genuine run boundary.
    open_left_requested: bool,
    /// The `start` of the very first run ever fed, captured once (`None`
    /// until then) — the "window boundary" `open_left_requested` refers to.
    /// Comparing a later candidate's own `start` against this (rather than
    /// tracking "have we moved past it" separately) is self-invalidating:
    /// run starts strictly increase along a stream, so nothing after the
    /// original run can ever have this same `start` again, truncated or not
    /// (e.g. if the original run turned out to be a glitch and was dropped
    /// entirely — the run that replaces it in `segment` starts at a real,
    /// observed transition, never at the window boundary).
    first_run_start: Option<u64>,
    /// Reusable buffers for `RunMerger`/`GlitchFilter` output, so a run
    /// closing does not cost a heap allocation on top of the `O(edges)`
    /// work itself (SPEC §2.9.2) — swapped out via `mem::take` and back
    /// rather than borrowed directly, since they are used from inside a
    /// loop that also needs other `&mut self` methods (`admit`).
    closed_scratch: Vec<Run>,
    finalized_scratch: Vec<Run>,
}

impl BitSeqMatcher {
    pub fn new(bit: u8, cell_samples: f64, tol_cells: f64, bits: Vec<BitVal>) -> Self {
        Self {
            cell_samples,
            tol_cells,
            pattern_len: bits.len(),
            k: KTable::new(&bits),
            merger: RunMerger::new(1 << bit),
            filter: GlitchFilter::default(),
            segment: Vec::new(),
            cursor: 0,
            open_left_requested: false,
            first_run_start: None,
            closed_scratch: Vec::new(),
            finalized_scratch: Vec::new(),
        }
    }

    /// Marks that the very first run this matcher will ever see is a lower
    /// bound, not a known run: its reported `start` is where the caller's
    /// search window begins (or the recording's retained start after
    /// eviction), not necessarily where the run truly began (SPEC §2.9).
    /// Call before the first [`Self::feed`].
    pub fn mark_first_run_open_left(&mut self) {
        self.open_left_requested = true;
    }

    /// `Some(w)` while a candidate starting exactly at `w` should get the
    /// open-left treatment; `None` once no such run was requested, or
    /// before any run has been seen to fix `w` against.
    fn open_left_start(&self) -> Option<u64> {
        if self.open_left_requested {
            self.first_run_start
        } else {
            None
        }
    }

    pub fn glitches_absorbed(&self) -> u64 {
        self.filter.glitches
    }

    fn admit(&mut self, r: Run, out: &mut Vec<Match>) {
        if is_off_rate(r.len, self.cell_samples, self.tol_cells) {
            // A barrier: no match may span it (SPEC §2.9.2 rule 3). Resolve
            // everything pending in the current segment against a hard
            // end, then start a fresh, empty segment after it.
            let open_left_start = self.open_left_start();
            scan_segment(
                &self.segment,
                None,
                &ScanParams {
                    k: &self.k,
                    pattern_len: self.pattern_len,
                    cell_samples: self.cell_samples,
                },
                &mut self.cursor,
                out,
                open_left_start,
            );
            self.segment.clear();
            self.cursor = 0;
        } else {
            self.segment.push(r);
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
            let mut finalized = std::mem::take(&mut self.finalized_scratch);
            finalized.clear();
            self.filter
                .ingest_closed(r, self.cell_samples, &mut finalized);
            for f in finalized.drain(..) {
                self.admit(f, &mut out);
            }
            self.finalized_scratch = finalized;
        }
        self.closed_scratch = closed;
        // Early-finalize `held` once the open run guarantees it will not
        // retroactively merge into it (see `GlitchFilter::effective_tail`
        // for why the guard below is the same "not ambiguous" condition).
        let open = self.merger.open();
        if !self.filter.absorbing {
            if let (Some(h), Some(o)) = (self.filter.held, open) {
                if o.len as f64 > 0.5 * self.cell_samples + EPS {
                    self.filter.held = None;
                    self.admit(h, &mut out);
                }
            }
        }
        let tail = self.filter.effective_tail(open, self.cell_samples);
        let open_left_start = self.open_left_start();
        scan_segment(
            &self.segment,
            tail,
            &ScanParams {
                k: &self.k,
                pattern_len: self.pattern_len,
                cell_samples: self.cell_samples,
            },
            &mut self.cursor,
            &mut out,
            open_left_start,
        );
        self.trim();
        out
    }

    /// Ends the stream: forces the open run and any held run to close, so
    /// a trailing pattern that only the (formerly open) run could complete
    /// is resolved one way or the other. Used by [`crate::search_runs`].
    pub fn finish(&mut self) -> Vec<Match> {
        let mut out = Vec::new();
        if let Some(open) = self.merger.finish() {
            let mut finalized = Vec::new();
            self.filter
                .ingest_closed(open, self.cell_samples, &mut finalized);
            for f in finalized {
                self.admit(f, &mut out);
            }
        }
        if let Some(h) = self.filter.finish() {
            self.admit(h, &mut out);
        }
        let open_left_start = self.open_left_start();
        scan_segment(
            &self.segment,
            None,
            &ScanParams {
                k: &self.k,
                pattern_len: self.pattern_len,
                cell_samples: self.cell_samples,
            },
            &mut self.cursor,
            &mut out,
            open_left_start,
        );
        out
    }

    /// Drops runs already fully consumed by a decided candidate, so
    /// memory stays bounded by the in-flight lookback, not the whole
    /// history (needed for the 10M-run throughput target).
    fn trim(&mut self) {
        let drop = self.cursor.min(self.segment.len());
        if drop > 0 {
            self.segment.drain(0..drop);
            self.cursor -= drop;
        }
    }
}
