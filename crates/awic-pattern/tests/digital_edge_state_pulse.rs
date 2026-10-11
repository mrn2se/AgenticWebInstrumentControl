//! Semantic tests for edge, state and pulse matching (SPEC §2.9.1).

mod support;

use awic_pattern::{parse, search_runs, Matcher, Run};
use support::rec_1mhz;

fn digital_channels() -> Vec<awic_pattern::ChannelInfo> {
    (0..4u8)
        .map(|bit| awic_pattern::ChannelInfo {
            name: format!("D{bit}"),
            kind: awic_pattern::ChannelKind::Digital { bit },
        })
        .collect()
}

#[test]
fn edge_rising_found_once_per_transition() {
    let pattern = parse("D0 rising", &digital_channels(), rec_1mhz()).unwrap();
    let runs = [
        Run {
            start: 0,
            len: 100,
            value: 0,
        },
        Run {
            start: 100,
            len: 100,
            value: 1,
        },
        Run {
            start: 200,
            len: 100,
            value: 0,
        },
        Run {
            start: 300,
            len: 100,
            value: 1,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.matches[0].sample, 100);
    assert_eq!(result.matches[0].decided_at, 100);
    assert_eq!(result.matches[1].sample, 300);
}

#[test]
fn edge_decided_immediately_even_while_new_run_is_open() {
    let pattern = parse("D0 rising", &digital_channels(), rec_1mhz()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let m1 = matcher.feed_runs(
        &[Run {
            start: 0,
            len: 100,
            value: 0,
        }],
        100,
    );
    assert!(m1.is_empty());
    // The new (still open) run already tells us the edge happened.
    let m2 = matcher.feed_runs(
        &[Run {
            start: 100,
            len: 5,
            value: 1,
        }],
        105,
    );
    assert_eq!(m2.len(), 1);
    assert_eq!(m2[0].sample, 100);
}

#[test]
fn edge_either_direction() {
    let pattern = parse("D0 edge", &digital_channels(), rec_1mhz()).unwrap();
    let runs = [
        Run {
            start: 0,
            len: 100,
            value: 1,
        },
        Run {
            start: 100,
            len: 100,
            value: 0,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].sample, 100);
}

#[test]
fn state_fires_only_on_transition_into_it() {
    let pattern = parse("D1,D0 = 11", &digital_channels(), rec_1mhz()).unwrap();
    let runs = [
        // First run already satisfies "11" — must NOT fire (no transition
        // observed, could have been true before the recording started).
        Run {
            start: 0,
            len: 100,
            value: 0b11,
        },
        Run {
            start: 100,
            len: 100,
            value: 0b01,
        },
        Run {
            start: 200,
            len: 100,
            value: 0b11, // genuine transition into "11"
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, 200);
}

#[test]
fn pulse_both_bounds_match_within_range() {
    let pattern = parse("D0 high >2ms <5ms", &digital_channels(), rec_1mhz()).unwrap();
    // 1MHz rate: 2ms=2000 samples, 5ms=5000 samples. A 3ms (3000-sample)
    // pulse should match.
    let runs = [
        Run {
            start: 0,
            len: 1000,
            value: 0,
        },
        Run {
            start: 1000,
            len: 3000,
            value: 1,
        },
        Run {
            start: 4000,
            len: 1000,
            value: 0,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, 1000);
    assert_eq!(result.matches[0].decided_at, 4000);
}

#[test]
fn pulse_both_bounds_rejects_too_long() {
    let pattern = parse("D0 high >2ms <5ms", &digital_channels(), rec_1mhz()).unwrap();
    let runs = [
        Run {
            start: 0,
            len: 1000,
            value: 0,
        },
        Run {
            start: 1000,
            len: 6000, // 6ms: too long
            value: 1,
        },
        Run {
            start: 7000,
            len: 1000,
            value: 0,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert!(result.matches.is_empty(), "{:?}", result.matches);
}

#[test]
fn pulse_level_held_fires_at_the_bound_while_still_open() {
    let pattern = parse("D0 low >=10ms", &digital_channels(), rec_1mhz()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let m0 = matcher.feed_runs(
        &[Run {
            start: 0,
            len: 1000,
            value: 1,
        }],
        1000,
    );
    assert!(m0.is_empty());
    // Low run starts; not yet 10ms (10_000 samples).
    let m1 = matcher.feed_runs(
        &[Run {
            start: 1000,
            len: 9_999,
            value: 0,
        }],
        10_999,
    );
    assert!(m1.is_empty(), "{m1:?}");
    // One more sample reaches exactly the 10ms bound: fires now, even
    // though the run is still open.
    let m2 = matcher.feed_runs(
        &[Run {
            start: 1000,
            len: 10_000,
            value: 0,
        }],
        11_000,
    );
    assert_eq!(m2.len(), 1, "{m2:?}");
    assert_eq!(m2[0].sample, 1000);
    assert_eq!(m2[0].decided_at, 11_000);

    // Growing further must not re-report.
    let m3 = matcher.feed_runs(
        &[Run {
            start: 1000,
            len: 20_000,
            value: 0,
        }],
        21_000,
    );
    assert!(m3.is_empty(), "{m3:?}");
}

/// SPEC §2.9: a recording's first run may have begun
/// before the data did (or before the last eviction) — its true length is
/// only a lower bound, so a "high >2ms" pulse entirely inside it must not
/// be reported: its onset is not actually known to be where the run's
/// `start` claims.
#[test]
fn pulse_truncated_first_run_is_not_reported_as_a_match() {
    let pattern = parse("D0 high >2ms <5ms", &digital_channels(), rec_1mhz()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    matcher.mark_first_run_open_left();
    let runs = [
        Run {
            start: 0,
            len: 3000, // looks like a clean 3ms pulse — but its true start is unknown
            value: 1,
        },
        Run {
            start: 3000,
            len: 1000,
            value: 0,
        },
    ];
    let matches = matcher.feed_runs(&runs, 4000);
    let more = matcher.finish();
    assert!(
        matches.is_empty() && more.is_empty(),
        "{matches:?} {more:?}"
    );
}

/// Same runs, without marking the first run open-left: the baseline
/// confirms the previous test's silence is really due to
/// `mark_first_run_open_left`, not an unrelated regression.
#[test]
fn pulse_first_run_without_open_left_marking_still_reports_normally() {
    let pattern = parse("D0 high >2ms <5ms", &digital_channels(), rec_1mhz()).unwrap();
    let runs = [
        Run {
            start: 0,
            len: 3000,
            value: 1,
        },
        Run {
            start: 3000,
            len: 1000,
            value: 0,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, 0);
}

/// A later, genuine pulse (a real, observed run — not the truncated first
/// one) after the open-left run must still be found normally.
#[test]
fn pulse_after_a_truncated_first_run_is_still_found() {
    let pattern = parse("D0 high >2ms <5ms", &digital_channels(), rec_1mhz()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    matcher.mark_first_run_open_left();
    let runs = [
        Run {
            start: 0,
            len: 3000, // truncated: must not be reported
            value: 1,
        },
        Run {
            start: 3000,
            len: 1000,
            value: 0,
        },
        Run {
            start: 4000,
            len: 3000, // a genuine, fully-observed 3ms pulse
            value: 1,
        },
        Run {
            start: 7000,
            len: 1000,
            value: 0,
        },
    ];
    let matches = matcher.feed_runs(&runs, 8000);
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].sample, 4000);
}

/// The lower-bound-only "level held" form (no upper bound) must also not
/// fire from a truncated first run, mirroring the open-right tail's own
/// "only once the duration is guaranteed" rule (SPEC §2.9.3) but for the
/// opposite edge.
#[test]
fn pulse_level_held_from_a_truncated_first_run_is_not_reported() {
    let pattern = parse("D0 low >=10ms", &digital_channels(), rec_1mhz()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    matcher.mark_first_run_open_left();
    let matches = matcher.feed_runs(
        &[Run {
            start: 0,
            len: 10_000, // reaches the 10ms bound — but its true onset is unknown
            value: 0,
        }],
        10_000,
    );
    assert!(matches.is_empty(), "{matches:?}");
}

#[test]
fn pulse_level_held_does_not_fire_before_the_bound() {
    let pattern = parse("D0 low >=10ms", &digital_channels(), rec_1mhz()).unwrap();
    let runs = [
        Run {
            start: 0,
            len: 1000,
            value: 1,
        },
        Run {
            start: 1000,
            len: 5000, // only 5ms
            value: 0,
        },
        Run {
            start: 6000,
            len: 1000,
            value: 1,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert!(result.matches.is_empty(), "{:?}", result.matches);
}
