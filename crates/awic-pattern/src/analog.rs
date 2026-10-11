//! `CH1 rising 1.2V` / `CH1 >1.5V` (SPEC §2.9.1), matched with hysteresis
//! (a Schmitt trigger) so noise sitting on the threshold does not produce a
//! burst of crossings.
//!
//! `>`/`>=` is implemented as the same event as `rising` (entering the
//! "above threshold" state) and `<`/`<=` as `falling`; `cross` fires on
//! either transition. All decisions are causal and immediate — a crossing
//! is certain the instant it is debounced past the far edge of the
//! hysteresis band, so `decided_at == sample` always.

use crate::pattern::{AnalogPattern, FireOn};
use crate::types::Match;

pub struct AnalogMatcher {
    p: AnalogPattern,
    /// `enter_high`/`enter_low` bound the hysteresis band around the
    /// threshold; `is_high` is the debounced state.
    enter_high: f64,
    enter_low: f64,
    is_high: Option<bool>,
    next_sample: u64,
}

impl AnalogMatcher {
    pub fn new(p: AnalogPattern) -> Self {
        let enter_high = p.threshold_code + p.hyst_code;
        let enter_low = p.threshold_code - p.hyst_code;
        Self {
            p,
            enter_high,
            enter_low,
            is_high: None,
            next_sample: 0,
        }
    }

    fn decode_code(&self, codes: &[u8], i: usize) -> f64 {
        if self.p.sample_bits == 16 {
            let lo = codes[i * 2] as i32;
            let hi = codes[i * 2 + 1] as i32;
            (((hi << 8) | lo) as i16) as f64
        } else {
            codes[i] as f64
        }
    }

    fn bytes_per_sample(&self) -> usize {
        if self.p.sample_bits == 16 {
            2
        } else {
            1
        }
    }

    fn step(&mut self, code: f64, sample: u64, out: &mut Vec<Match>) {
        let was_high = self.is_high;
        if was_high != Some(true) && code >= self.enter_high {
            self.is_high = Some(true);
            if was_high.is_some() && matches!(self.p.fire_on, FireOn::Rising | FireOn::Both) {
                out.push(Match {
                    sample,
                    decided_at: sample,
                });
            }
        } else if was_high != Some(false) && code <= self.enter_low {
            self.is_high = Some(false);
            if was_high.is_some() && matches!(self.p.fire_on, FireOn::Falling | FireOn::Both) {
                out.push(Match {
                    sample,
                    decided_at: sample,
                });
            }
        } else if was_high.is_none() {
            // First sample landed inside the band: pick a provisional side
            // with plain threshold comparison so later crossings are
            // measured from a defined state, but this is not itself a
            // transition (nothing preceded it to transition from).
            self.is_high = Some(code >= self.p.threshold_code);
        }
    }

    /// `codes` holds `first_sample`-relative, channel-interleaved raw
    /// samples for every analog channel in the recording, widths per
    /// `RecordingInfo::sample_bits`. Calls must be contiguous: this call's
    /// `first_sample` must equal the sample index right after the last one
    /// fed or skipped.
    pub fn feed_analog(&mut self, first_sample: u64, codes: &[u8]) -> Vec<Match> {
        assert_eq!(
            first_sample, self.next_sample,
            "feed_analog must be called with contiguous sample ranges"
        );
        let bps = self.bytes_per_sample();
        let stride = bps * self.p.channel_count as usize;
        let n = codes.len().checked_div(stride).unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..n {
            let base = i * stride + bps * self.p.channel_index as usize;
            let code = self.decode_code(&codes[base..], 0);
            self.step(code, first_sample + i as u64, &mut out);
        }
        self.next_sample = first_sample + n as u64;
        out
    }

    /// Storage-pruning hook (SPEC §2.8.3): a summary bucket whose raw
    /// `min_code..=max_code` lies entirely on one side of the hysteresis
    /// band cannot contain a debounced transition, so its samples need
    /// never be decoded. Must only be called for the span immediately
    /// after the last one fed or skipped, same as `feed_analog`.
    pub fn try_skip_analog(&mut self, start: u64, len: u64, min_code: i32, max_code: i32) -> bool {
        assert_eq!(
            start, self.next_sample,
            "try_skip_analog must be called with contiguous sample ranges"
        );
        let safe = match self.is_high {
            Some(true) => (min_code as f64) > self.enter_low,
            Some(false) => (max_code as f64) < self.enter_high,
            None => false, // no established state yet; must observe the real samples
        };
        if safe {
            self.next_sample = start + len;
        }
        safe
    }
}
