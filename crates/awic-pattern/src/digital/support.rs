//! Shared run-stream plumbing for the digital matchers.
//!
//! A recording's run list is fed to a matcher in arbitrary chunks, and the
//! last run of a chunk may be *open* — still growing — and reappear at the
//! start of the next chunk with a larger `len` (SPEC §2.9.3: "the last run
//! is still open and may grow"). Both helpers here absorb that so the
//! per-pattern matchers only ever see genuinely new information.

use crate::types::Run;

/// Skips the leading run of a new batch when it is the same physical run as
/// the last one already seen (same `start`), for matchers that only care
/// about a run's *value* (edges, states) and not its exact duration — so
/// they need no merging of same-valued neighbours, only de-duplication of
/// the repeated open run.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunDedup {
    last_start: Option<u64>,
}

impl RunDedup {
    pub fn new_runs<'a>(&mut self, runs: &'a [Run]) -> &'a [Run] {
        if runs.is_empty() {
            return runs;
        }
        let skip = self.last_start == Some(runs[0].start);
        self.last_start = Some(runs[runs.len() - 1].start);
        if skip {
            &runs[1..]
        } else {
            runs
        }
    }
}

/// Projects incoming runs through `mask` and merges adjacent runs whose
/// masked value is equal — both the repeated-open-run case above, and the
/// task's note that "adjacent runs may carry equal values" at a chunk or
/// storage-bucket boundary even though they are logically one run.
///
/// A run is handed back via `feed`'s return value only once it has
/// *closed* (a differently-valued run started after it), so its `len` is
/// final. The currently accumulating run is available via [`Self::open`].
pub struct RunMerger {
    mask: u8,
    current: Option<Run>,
}

impl RunMerger {
    pub fn new(mask: u8) -> Self {
        Self {
            mask,
            current: None,
        }
    }

    /// Appends any newly-closed runs to `closed_out` (not cleared first —
    /// the caller owns that, so it can reuse one scratch buffer across
    /// many calls instead of allocating afresh each time; §2.9.2's
    /// O(edges) cost would otherwise be an O(edges) count of heap
    /// allocations too).
    pub fn feed(&mut self, runs: &[Run], closed_out: &mut Vec<Run>) {
        for &raw in runs {
            let r = Run {
                start: raw.start,
                len: raw.len,
                value: raw.value & self.mask,
            };
            match &mut self.current {
                None => self.current = Some(r),
                Some(cur) => {
                    if r.start == cur.start {
                        cur.len = r.len;
                        cur.value = r.value;
                    } else if r.value == cur.value {
                        cur.len = (r.start + r.len) - cur.start;
                    } else {
                        closed_out.push(*cur);
                        self.current = Some(r);
                    }
                }
            }
        }
    }

    /// The run still accumulating, if any data has arrived yet.
    pub fn open(&self) -> Option<Run> {
        self.current
    }

    /// Ends the stream: the open run (if any) will never grow further, so
    /// hand it back as closed. Used by [`crate::search_runs`], which
    /// searches a fixed, finite range.
    pub fn finish(&mut self) -> Option<Run> {
        self.current.take()
    }
}
