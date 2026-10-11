use std::collections::VecDeque;

use crate::Transition;

/// Turns a bounded, chunked stream of packed-byte transitions into random
/// (but monotonic) access to "what was the byte at sample S".
///
/// This is what makes chunking invisible to a decoder: transitions accepted
/// by [`push`](Self::push) are buffered until a query consumes them, so a
/// decoder can ask about any sample up to the last `valid_until` regardless
/// of which `feed()` call the underlying transition arrived in — the same
/// state produces the same answer whether the caller handed it one big
/// slice or many small ones.
#[derive(Debug, Clone)]
pub(crate) struct LevelTracker {
    current: u8,
    pending: VecDeque<Transition>,
    known_until: u64,
}

impl LevelTracker {
    pub(crate) fn new(initial: u8) -> Self {
        LevelTracker {
            current: initial,
            pending: VecDeque::new(),
            known_until: 0,
        }
    }

    /// Buffer newly fed transitions and record the new known-until point.
    /// `transitions` must be in increasing `sample_index` order, as the
    /// `Decoder::feed` contract requires.
    pub(crate) fn push(&mut self, transitions: &[Transition], valid_until: u64) {
        self.pending.extend(transitions.iter().copied());
        debug_assert!(
            valid_until >= self.known_until,
            "valid_until must not go backwards"
        );
        self.known_until = valid_until;
    }

    /// Replace the assumed level before any input (see `Decoder::seed`).
    pub(crate) fn seed(&mut self, levels: u8) {
        debug_assert!(self.pending.is_empty(), "seed after feed");
        self.current = levels;
    }

    pub(crate) fn known_until(&self) -> u64 {
        self.known_until
    }

    /// The `sample_index` of the next buffered transition, applied or not
    /// yet within known input.
    pub(crate) fn next_index(&self) -> Option<u64> {
        self.pending.front().map(|t| t.sample_index)
    }

    /// The packed byte at `sample`, consuming any buffered transition at or
    /// before it. `sample` must not exceed [`known_until`](Self::known_until).
    pub(crate) fn value_at(&mut self, sample: u64) -> u8 {
        debug_assert!(sample <= self.known_until, "query beyond known input");
        while let Some(t) = self.pending.front() {
            if t.sample_index <= sample {
                self.current = t.value;
                self.pending.pop_front();
            } else {
                break;
            }
        }
        self.current
    }

    pub(crate) fn bit_at(&mut self, sample: u64, bit: u8) -> bool {
        (self.value_at(sample) >> bit) & 1 == 1
    }

    /// Pop and apply the next transition whose `sample_index` is within
    /// known input, returning `(sample_index, value_before, value_after)`.
    /// `None` if there is no such transition yet (caller must wait for more
    /// input before it can make further progress scanning for edges).
    pub(crate) fn advance(&mut self) -> Option<(u64, u8, u8)> {
        let t = *self.pending.front()?;
        if t.sample_index > self.known_until {
            return None;
        }
        self.pending.pop_front();
        let before = self.current;
        self.current = t.value;
        Some((t.sample_index, before, t.value))
    }

    /// Like [`advance`](Self::advance), but skips transitions that don't
    /// change any bit in `mask`, returning the first one that does.
    pub(crate) fn next_relevant(&mut self, mask: u8) -> Option<(u64, u8, u8)> {
        loop {
            let (sample, before, after) = self.advance()?;
            if (before & mask) != (after & mask) {
                return Some((sample, before, after));
            }
        }
    }
}
