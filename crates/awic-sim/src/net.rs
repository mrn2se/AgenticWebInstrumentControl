//! A digital net: the level of one wire over time, kept as its edges.
//!
//! Everything the board drives is written here as it happens, and every
//! virtual instrument reads from here. Storing edges, not samples, is what
//! lets one net be read at 24 MS/s by the logic analyzer and at 115 200 baud
//! by the USB-UART adapter, from the same history.

use std::collections::VecDeque;

/// Times are nanoseconds of simulated time since the bench started.
pub type Ns = u64;

#[derive(Debug, Clone)]
pub struct EdgeLog {
    /// The level before the first retained edge.
    base_level: bool,
    /// Times at which the level toggles, strictly increasing.
    edges: VecDeque<Ns>,
}

/// One net's level at the start of a window and its edges inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub level: bool,
    /// Toggle times in `(start, end]`, increasing.
    pub edges: Vec<Ns>,
}

impl EdgeLog {
    pub fn new(level: bool) -> Self {
        Self {
            base_level: level,
            edges: VecDeque::new(),
        }
    }

    /// The level after the last edge — what the net is driven to now.
    pub fn last_level(&self) -> bool {
        self.base_level ^ (self.edges.len() % 2 == 1)
    }

    pub fn last_edge(&self) -> Option<Ns> {
        self.edges.back().copied()
    }

    /// Drives the net to `level` from time `t` on. Writes never go back in
    /// time: a `t` before the last edge is moved up to it, and a toggle back
    /// at the very instant of the last edge cancels that edge (a pulse of
    /// zero width is no pulse).
    pub fn set(&mut self, t: Ns, level: bool) {
        if level == self.last_level() {
            return;
        }
        match self.edges.back() {
            Some(&last) if t <= last => {
                self.edges.pop_back();
            }
            _ => self.edges.push_back(t),
        }
    }

    pub fn level_at(&self, t: Ns) -> bool {
        let n = self.edges.partition_point(|&e| e <= t);
        self.base_level ^ (n % 2 == 1)
    }

    /// The first edge after `t` whose new level is `level`.
    pub fn next_edge_to(&self, t: Ns, level: bool) -> Option<Ns> {
        let start = self.edges.partition_point(|&e| e <= t);
        (start..self.edges.len())
            .find(|&i| (self.base_level ^ (i % 2 == 0)) == level)
            .map(|i| self.edges[i])
    }

    pub fn window(&self, start: Ns, end: Ns) -> Window {
        let first = self.edges.partition_point(|&e| e <= start);
        let last = self.edges.partition_point(|&e| e <= end);
        Window {
            level: self.base_level ^ (first % 2 == 1),
            edges: self.edges.range(first..last).copied().collect(),
        }
    }

    /// Forgets everything driven after `t` — a reset or power loss cancels
    /// what the firmware had already queued — leaving the level at `t`.
    pub fn truncate_after(&mut self, t: Ns) {
        while self.edges.back().is_some_and(|&e| e > t) {
            self.edges.pop_back();
        }
    }

    /// Drops history before `t`, keeping the level at `t` exact.
    pub fn prune_before(&mut self, t: Ns) {
        while self.edges.front().is_some_and(|&e| e < t) {
            self.edges.pop_front();
            self.base_level = !self.base_level;
        }
    }

    pub fn len(&self) -> usize {
        self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_the_edges() {
        let mut net = EdgeLog::new(true);
        net.set(100, false);
        net.set(200, true);
        assert!(net.level_at(99));
        assert!(!net.level_at(100));
        assert!(!net.level_at(199));
        assert!(net.level_at(200));
    }

    #[test]
    fn setting_the_current_level_adds_no_edge() {
        let mut net = EdgeLog::new(false);
        net.set(10, false);
        assert!(net.is_empty());
    }

    #[test]
    fn writes_never_go_back_in_time() {
        let mut net = EdgeLog::new(false);
        net.set(100, true);
        net.set(50, false); // cancels the edge at 100: a zero-width pulse
        assert!(net.is_empty());
        assert!(!net.level_at(150));
    }

    #[test]
    fn a_window_reports_its_start_level_and_inner_edges() {
        let mut net = EdgeLog::new(false);
        for (t, l) in [(10, true), (20, false), (30, true), (40, false)] {
            net.set(t, l);
        }
        let w = net.window(15, 35);
        assert!(w.level);
        assert_eq!(w.edges, vec![20, 30]);
    }

    #[test]
    fn pruning_keeps_the_level() {
        let mut net = EdgeLog::new(false);
        net.set(10, true);
        net.set(20, false);
        net.set(30, true);
        net.prune_before(25);
        assert!(!net.level_at(25));
        assert!(net.level_at(30));
        assert_eq!(net.len(), 1);
    }

    #[test]
    fn next_edge_to_finds_a_falling_edge() {
        let mut net = EdgeLog::new(true);
        net.set(10, false);
        net.set(20, true);
        net.set(30, false);
        assert_eq!(net.next_edge_to(0, false), Some(10));
        assert_eq!(net.next_edge_to(10, false), Some(30));
        assert_eq!(net.next_edge_to(0, true), Some(20));
        assert_eq!(net.next_edge_to(30, false), None);
    }

    #[test]
    fn truncation_cancels_queued_edges() {
        let mut net = EdgeLog::new(true);
        net.set(10, false);
        net.set(20, true);
        net.truncate_after(15);
        assert!(!net.last_level());
    }
}
