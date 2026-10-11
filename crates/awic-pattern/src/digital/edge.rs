//! `D2 rising` / `falling` / `edge` (SPEC §2.9.1).

use super::support::RunDedup;
use crate::pattern::EdgeDir;
use crate::types::{Match, Run};

pub struct EdgeMatcher {
    bit: u8,
    dir: EdgeDir,
    dedup: RunDedup,
    prev: Option<bool>,
}

impl EdgeMatcher {
    pub fn new(bit: u8, dir: EdgeDir) -> Self {
        Self {
            bit,
            dir,
            dedup: RunDedup::default(),
            prev: None,
        }
    }

    /// A transition is certain the instant the run after it starts, even if
    /// that run is still open — more of the same level only confirms it.
    pub fn feed(&mut self, runs: &[Run]) -> Vec<Match> {
        let mut out = Vec::new();
        for &r in self.dedup.new_runs(runs) {
            let level = (r.value >> self.bit) & 1 != 0;
            if let Some(prev) = self.prev {
                let fired = match self.dir {
                    EdgeDir::Rising => !prev && level,
                    EdgeDir::Falling => prev && !level,
                    EdgeDir::Either => prev != level,
                };
                if fired {
                    out.push(Match {
                        sample: r.start,
                        decided_at: r.start,
                    });
                }
            }
            self.prev = Some(level);
        }
        out
    }

    /// Nothing is ever left pending: every edge is decided the moment the
    /// run after it appears.
    pub fn finish(&mut self) -> Vec<Match> {
        Vec::new()
    }
}
