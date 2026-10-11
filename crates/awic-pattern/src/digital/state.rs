//! `D3..D0 = 1x01` / `D0,D2 = 10` (SPEC §2.9.1): fires when the masked bits
//! *become* equal to `expected`.
//!
//! Deliberately edge-triggered, not level-triggered: if the recording's
//! first known run already satisfies `expected`, that is not reported,
//! because no transition into the state was observed (it may have become
//! true before the recording started). Only a genuine transition — a
//! previous run that did *not* match, followed by one that does — counts.

use super::support::RunDedup;
use crate::types::{Match, Run};

pub struct StateMatcher {
    mask: u8,
    expected: u8,
    dedup: RunDedup,
    prev_matched: Option<bool>,
}

impl StateMatcher {
    pub fn new(mask: u8, expected: u8) -> Self {
        Self {
            mask,
            expected,
            dedup: RunDedup::default(),
            prev_matched: None,
        }
    }

    pub fn feed(&mut self, runs: &[Run]) -> Vec<Match> {
        let mut out = Vec::new();
        for &r in self.dedup.new_runs(runs) {
            let matched = (r.value & self.mask) == self.expected;
            if self.prev_matched == Some(false) && matched {
                out.push(Match {
                    sample: r.start,
                    decided_at: r.start,
                });
            }
            self.prev_matched = Some(matched);
        }
        out
    }

    pub fn finish(&mut self) -> Vec<Match> {
        Vec::new()
    }
}
