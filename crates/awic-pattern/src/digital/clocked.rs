//! `D1 clk=D0 rising 1010x` (SPEC §2.9.1, SPEC §15 S-02).
//!
//! No frequency and no tolerance: each cell's value is exactly the data
//! bit's level the instant the chosen clock edge lands, decided
//! immediately, so unlike [`super::bitseq`] there is nothing to
//! glitch-absorb or classify as off-rate.

use std::collections::VecDeque;

use super::support::RunDedup;
use crate::pattern::{BitVal, EdgeDir};
use crate::types::{Match, Run};

pub struct ClockedMatcher {
    data_bit: u8,
    clk_bit: u8,
    dir: EdgeDir,
    bits: Vec<BitVal>,
    dedup: RunDedup,
    prev_clk: Option<bool>,
    /// Ring buffer of the last `bits.len()` sampled (value, sample) pairs.
    window: VecDeque<(bool, u64)>,
}

impl ClockedMatcher {
    pub fn new(data_bit: u8, clk_bit: u8, dir: EdgeDir, bits: Vec<BitVal>) -> Self {
        Self {
            data_bit,
            clk_bit,
            dir,
            bits,
            dedup: RunDedup::default(),
            prev_clk: None,
            window: VecDeque::new(),
        }
    }

    pub fn feed(&mut self, runs: &[Run]) -> Vec<Match> {
        let mut out = Vec::new();
        for &r in self.dedup.new_runs(runs) {
            let clk = (r.value >> self.clk_bit) & 1 != 0;
            if let Some(prev_clk) = self.prev_clk {
                let edge = match self.dir {
                    EdgeDir::Rising => !prev_clk && clk,
                    EdgeDir::Falling => prev_clk && !clk,
                    EdgeDir::Either => unreachable!("parser only allows rising/falling for clk="),
                };
                if edge {
                    let data = (r.value >> self.data_bit) & 1 != 0;
                    self.window.push_back((data, r.start));
                    if self.window.len() > self.bits.len() {
                        self.window.pop_front();
                    }
                    if self.window.len() == self.bits.len() && !self.bits.is_empty() {
                        let matched = self
                            .bits
                            .iter()
                            .zip(self.window.iter())
                            .all(|(want, (have, _))| want.matches(*have));
                        if matched {
                            let sample = self.window.front().unwrap().1;
                            let decided_at = self.window.back().unwrap().1;
                            out.push(Match { sample, decided_at });
                            self.window.clear(); // non-overlapping (SPEC §2.9.2 rule 5)
                        }
                    }
                }
            }
            self.prev_clk = Some(clk);
        }
        out
    }

    /// A partial window at end-of-stream can never complete: no more clock
    /// edges are coming.
    pub fn finish(&mut self) -> Vec<Match> {
        Vec::new()
    }
}
