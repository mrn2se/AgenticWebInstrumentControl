//! Edge-trigger placement for synthetic captures.
//!
//! Real scope hardware arms on a level/slope condition and reports samples
//! straddling that moment. `awic-core`'s [`Trigger`] carries no explicit
//! "pretrigger" position, so this crate adopts the common oscilloscope
//! default: the trigger event lands at the **midpoint** of the capture
//! ([`trigger_target_index`]).
//!
//! Every synthetic source in this crate is defined as a pure function of an
//! absolute (possibly negative) sample index, so placing the trigger is just
//! a phase shift: find the sample index `k` (searching forward from 0) at
//! which the configured edge first occurs, then generate sample `n` of the
//! output using source index `n + shift` where `shift = k - target`.

use awic_core::{Slope, Trigger};

/// Where the trigger condition should land within a capture of `samples`
/// total samples: the midpoint, matching typical scope defaults.
pub fn trigger_target_index(samples: u64) -> u64 {
    samples / 2
}

/// How far forward from index 0 to search for the configured edge before
/// giving up. Generous relative to any waveform period or protocol frame
/// used in this crate's default configurations, while still resolving in a
/// few hundred microseconds of real time.
const SEARCH_SPAN: i64 = 500_000;

/// Resolves the index shift that places `trigger`'s edge at `target`
/// (typically [`trigger_target_index`[`samples`]]) when applied as
/// `source_index = n + shift` for output sample `n`.
///
/// `level_fn` evaluates the trigger channel's scalar value at an arbitrary
/// absolute sample index — voltage for analog, 0.0/1.0 for digital.
///
/// Returns `0` (no shift) with a `tracing::warn!` if no crossing is found
/// within [`SEARCH_SPAN`] samples of the origin — a misconfigured trigger
/// level outside the signal's range, most likely.
pub(crate) fn resolve_trigger_shift(
    trigger: &Trigger,
    target: u64,
    level_fn: impl Fn(i64) -> f64,
) -> i64 {
    let Trigger::Edge { slope, level, .. } = trigger;
    // A digital trigger carries no level (B-96): its channel reads 0/1, and
    // the edge is the crossing of half.
    let level = level.map_or(0.5, f64::from);

    let mut prev = level_fn(-1);
    for k in 0..SEARCH_SPAN {
        let cur = level_fn(k);
        let crossed = match slope {
            Slope::Rising => prev < level && cur >= level,
            Slope::Falling => prev > level && cur <= level,
        };
        if crossed {
            return k - target as i64;
        }
        prev = cur;
    }

    tracing::warn!(
        target = target,
        "fake instrument: no trigger edge found within search window; capture will start untriggered"
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_rising_edge_through_a_linear_ramp() {
        // level_fn(k) = k, so it crosses 100.0 exactly at k = 100.
        let trigger = Trigger::Edge {
            channel: 0,
            slope: Slope::Rising,
            level: Some(100.0),
        };
        let shift = resolve_trigger_shift(&trigger, 1_000, |k| k as f64);
        // output sample n uses source index n + shift; we want source index
        // 100 to land at output index 1_000, so shift = 100 - 1000 = -900.
        assert_eq!(shift, 100 - 1_000);
    }

    #[test]
    fn falling_edge_is_distinguished_from_rising() {
        // A downward ramp crosses 0 on the way down; only the Falling slope
        // should find it starting from a positive value.
        let trigger = Trigger::Edge {
            channel: 0,
            slope: Slope::Falling,
            level: Some(0.0),
        };
        let shift = resolve_trigger_shift(&trigger, 0, |k| 50.0 - k as f64);
        assert_eq!(shift, 50);
    }

    #[test]
    fn a_digital_trigger_without_a_level_finds_the_bit_change() {
        let trigger = Trigger::Edge {
            channel: 0,
            slope: Slope::Rising,
            level: None,
        };
        let shift = resolve_trigger_shift(&trigger, 10, |k| f64::from(u8::from(k >= 30)));
        assert_eq!(shift, 30 - 10);
    }

    #[test]
    fn missing_edge_falls_back_to_zero_shift() {
        let trigger = Trigger::Edge {
            channel: 0,
            slope: Slope::Rising,
            level: Some(1_000_000.0),
        };
        let shift = resolve_trigger_shift(&trigger, 10, |_| 0.0);
        assert_eq!(shift, 0);
    }
}
