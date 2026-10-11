//! Semantic tests for bit-sequence matching (SPEC §2.9.2), synthesized as
//! run lists directly (no need for a real recorder or UART encoder).

mod support;

use awic_pattern::{parse, search_runs, Matcher, Run};
use support::{channels, rec_1mhz};

/// Builds runs for a UART-like byte on D0 at `bit_rate` bit/s, sampled at
/// `rate_hz`, starting at `start`: idle high, start bit (low), 8 data bits
/// LSB-first, stop bit (high). Returns (runs, sample of the pattern this
/// test looks for).
fn uart_frame(rate_hz: f64, bit_rate: f64, start: u64, byte: u8, idle_after: u64) -> Vec<Run> {
    let cell = (rate_hz / bit_rate).round() as u64;
    let mut bits = vec![0u8]; // start bit
    for i in 0..8 {
        bits.push((byte >> i) & 1);
    }
    bits.push(1); // stop bit
    let mut runs = Vec::new();
    // Leading idle high before the frame.
    runs.push(Run {
        start: 0,
        len: start,
        value: 1,
    });
    let mut t = start;
    let mut i = 0;
    while i < bits.len() {
        let v = bits[i];
        let mut n = 1u64;
        while i + 1 < bits.len() && bits[i + 1] == v {
            n += 1;
            i += 1;
        }
        runs.push(Run {
            start: t,
            len: n * cell,
            value: v,
        });
        t += n * cell;
        i += 1;
    }
    runs.push(Run {
        start: t,
        len: idle_after,
        value: 1,
    });
    runs
}

#[test]
fn uart_frame_found_at_right_time() {
    let rate_hz = 1_000_000.0;
    let bit_rate = 1000.0; // 1 kbit/s
    let start = 3_214_000u64; // 3.214 s in
    let runs = uart_frame(rate_hz, bit_rate, start, 0b1011_0010, 5000);

    let rec = awic_pattern::RecordingInfo {
        rate_hz,
        sample_bits: 8,
        channel_count: 0,
    };
    // Start bit (0) then data LSB-first "01001101" then stop (1); the
    // pattern below matches the start bit and first few data bits.
    let pattern = parse("D0 f=1k 0010", &channels(), rec).unwrap();
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, start);
    assert_eq!(result.glitches, 0);
    assert!(!result.truncated);
}

#[test]
fn sender_three_percent_fast_still_matches() {
    let rate_hz = 1_000_000.0;
    let bit_rate = 1000.0;
    let fast_bit_rate = bit_rate * 1.03; // sender's clock is 3% fast
    let start = 100_000u64;
    let runs = uart_frame(rate_hz, fast_bit_rate, start, 0b1011_0010, 5000);

    let rec = awic_pattern::RecordingInfo {
        rate_hz,
        sample_bits: 8,
        channel_count: 0,
    };
    let pattern = parse("D0 f=1k 0010", &channels(), rec).unwrap();
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
}

#[test]
fn sender_forty_percent_off_does_not_match() {
    let rate_hz = 1_000_000.0;
    let bit_rate = 1000.0;
    let off_bit_rate = bit_rate * 1.40; // 40% fast: way off tolerance
    let start = 100_000u64;
    let runs = uart_frame(rate_hz, off_bit_rate, start, 0b1011_0010, 5000);

    let rec = awic_pattern::RecordingInfo {
        rate_hz,
        sample_bits: 8,
        channel_count: 0,
    };
    let pattern = parse("D0 f=1k 0010", &channels(), rec).unwrap();
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert!(result.matches.is_empty(), "{:?}", result.matches);
}

#[test]
fn glitch_absorption_counts_and_ignores_short_run() {
    // Cell = 1000 samples at 1kbit/s over 1MHz. A run of 10 samples (1% of
    // a cell) is a glitch and must not appear in the cell sequence.
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 101", &channels(), rec).unwrap();
    let runs = vec![
        Run {
            start: 0,
            len: 1000,
            value: 1,
        }, // cells 0-1: "1" (2000 samples, once merged below)
        Run {
            start: 1000,
            len: 10,
            value: 0,
        }, // glitch (absorbed into the surrounding "1"s)
        Run {
            start: 1010,
            len: 990,
            value: 1,
        }, // still part of cells 0-1's "1" (merged)
        Run {
            start: 2000,
            len: 1000,
            value: 0,
        }, // cell 2: "0"
        Run {
            start: 3000,
            len: 1000,
            value: 1,
        }, // cell 3: "1"
        Run {
            start: 4000,
            len: 1000,
            value: 0,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    // Cells are 1,1,0,1: "101" matches starting at cell 1 (sample 1000),
    // not cell 0 — the merged run has *two* cells of "1", but the pattern
    // only needs the last one before the value changes.
    assert_eq!(result.matches[0].sample, 1000);
    assert_eq!(result.glitches, 1);
}

#[test]
fn leading_glitch_at_start_of_stream_is_dropped_not_matched() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 1", &channels(), rec).unwrap();
    let runs = vec![
        Run {
            start: 0,
            len: 5,
            value: 1,
        }, // glitch with nothing before it
        Run {
            start: 5,
            len: 1000,
            value: 0,
        },
        Run {
            start: 1005,
            len: 1000,
            value: 1,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.glitches, 1);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].sample, 1005);
}

#[test]
fn off_rate_run_blocks_a_spanning_match() {
    let rec = rec_1mhz();
    // Pattern "11": needs two consecutive high cells. A run whose duration
    // corresponds to 1.4 cells (tol 0.3) is off-rate and must not be
    // spanned by a match.
    let pattern = parse("D0 f=1k 11", &channels(), rec).unwrap();
    let runs = vec![
        Run {
            start: 0,
            len: 1400, // 1.4 cells: off-rate (tol=0.3 => allowed band is [0.7,1.3])
            value: 1,
        },
        Run {
            start: 1400,
            len: 1000,
            value: 0,
        },
        Run {
            start: 2400,
            len: 2000, // exactly 2 on-rate cells of "1"
            value: 1,
        },
        Run {
            start: 4400,
            len: 1000,
            value: 0,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, 2400);
}

#[test]
fn match_ending_inside_open_run_decided_early() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 01", &channels(), rec).unwrap();
    let mut matcher = Matcher::new(&pattern);

    // Cell 0 ("0") closes normally.
    let m1 = matcher.feed_runs(
        &[
            Run {
                start: 0,
                len: 1000,
                value: 0,
            },
            Run {
                start: 1000,
                len: 400, // open run, only 0.4 cells so far: not yet decided
                value: 1,
            },
        ],
        1400,
    );
    assert!(m1.is_empty(), "{m1:?}");

    // The open run grows to 600 samples (0.6 cells): still short of the
    // 0.5-cell guarantee threshold (need >= (1-0.5)*1000 = 500)... actually
    // 600 already clears 500, so this should decide.
    let m2 = matcher.feed_runs(
        &[Run {
            start: 1000,
            len: 600,
            value: 1,
        }],
        1600,
    );
    assert_eq!(m2.len(), 1, "{m2:?}");
    assert_eq!(m2[0].sample, 0);
    assert_eq!(m2[0].decided_at, 1500); // start(1000) + ceil((1-0.5)*1000)
}

#[test]
fn matches_are_non_overlapping_and_only_first_position_in_long_run() {
    let rec = rec_1mhz();
    // Pattern "1" over one very long high run: only the first position is
    // reported (SPEC §2.9.2 rule 5), not one per cell.
    let pattern = parse("D0 f=1k 1", &channels(), rec).unwrap();
    let runs = vec![
        Run {
            start: 0,
            len: 1000,
            value: 0,
        },
        Run {
            start: 1000,
            len: 50_000, // 50 cells, all "1"
            value: 1,
        },
        Run {
            start: 51_000,
            len: 1000,
            value: 0,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 1024);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, 1000);
}

/// SPEC §2.9: a self-contained pattern (fits entirely
/// within one run) must not anchor on a truncated first run's fictitious
/// `start` — it aligns to the run's known right edge instead, using
/// whatever slack the visible portion has.
#[test]
fn open_left_self_contained_match_aligns_to_the_runs_known_end_not_its_fictitious_start() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 1", &channels(), rec).unwrap();
    let mut matcher = Matcher::new(&pattern);
    matcher.mark_first_run_open_left();
    let matches = matcher.feed_runs(
        &[
            Run {
                start: 0,
                len: 5000, // truncated: 5 visible cells, but the true run may be longer
                value: 1,
            },
            Run {
                start: 5000,
                len: 1000,
                value: 0,
            },
        ],
        6000,
    );
    assert_eq!(matches.len(), 1, "{matches:?}");
    // 5000 (the run's known end) minus the one cell the pattern needs —
    // not 0, the search window's own fictitious start.
    assert_eq!(matches[0].sample, 4000);
}

/// Same runs, without `mark_first_run_open_left`: the baseline, confirming
/// the previous test's different answer is really due to the marking, not
/// some unrelated change — sample 0 here is legitimate (an ordinary run's
/// start is real), it just happens to coincide with what would be spurious
/// for a truncated one.
#[test]
fn without_open_left_marking_a_self_contained_match_uses_the_runs_start() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 1", &channels(), rec).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let matches = matcher.feed_runs(
        &[
            Run {
                start: 0,
                len: 5000,
                value: 1,
            },
            Run {
                start: 5000,
                len: 1000,
                value: 0,
            },
        ],
        6000,
    );
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].sample, 0);
}

/// SPEC task note: a truncated first run whose *entire* visible length is
/// needed to carry the pattern's leading cells (no slack at all) cannot be
/// placed anywhere certain, so it must be refused outright — while a later,
/// genuine occurrence of the same pattern is still found normally.
#[test]
fn open_left_first_run_with_no_slack_is_not_reported_but_a_later_match_still_is() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 011", &channels(), rec).unwrap();
    let mut matcher = Matcher::new(&pattern);
    matcher.mark_first_run_open_left();

    let runs = vec![
        Run {
            start: 0,
            len: 1000,
            value: 0,
        }, // truncated: exactly 1 cell, no slack — would otherwise anchor "011" at sample 0
        Run {
            start: 1000,
            len: 2000,
            value: 1,
        },
        Run {
            start: 3000,
            len: 1000,
            value: 0,
        }, // a later, fully-certain "0" cell
        Run {
            start: 4000,
            len: 2000,
            value: 1,
        }, // completes a second, legitimate "011"
    ];
    let matches = matcher.feed_runs(&runs, 6000);
    let more = matcher.finish();
    let all: Vec<_> = matches.into_iter().chain(more).collect();

    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].sample, 3000);
}

/// Same runs, without marking the first run open-left: two matches are
/// found, the first one (sample 0) exactly the spurious "0.0000 s"-style
/// match seen on hardware — the baseline this fix changes.
#[test]
fn without_open_left_marking_the_same_runs_report_a_match_at_the_fictitious_start() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 011", &channels(), rec).unwrap();
    let runs = vec![
        Run {
            start: 0,
            len: 1000,
            value: 0,
        },
        Run {
            start: 1000,
            len: 2000,
            value: 1,
        },
        Run {
            start: 3000,
            len: 1000,
            value: 0,
        },
        Run {
            start: 4000,
            len: 2000,
            value: 1,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 2, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, 0);
    assert_eq!(result.matches[1].sample, 3000);
}

/// A run that is truncated on the left *and* still growing (open on both
/// edges) has no certain edge at all to anchor a match to — it must report
/// nothing until it closes, the mirror of how the ordinary open-right tail
/// needs its length guaranteed before it can complete a match.
#[test]
fn open_left_run_that_is_also_still_growing_reports_nothing_until_it_closes() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 1", &channels(), rec).unwrap();
    let mut matcher = Matcher::new(&pattern);
    matcher.mark_first_run_open_left();
    let matches = matcher.feed_runs(
        &[Run {
            start: 0,
            len: 5000,
            value: 1,
        }],
        5000,
    );
    assert!(matches.is_empty(), "{matches:?}");
}

#[test]
fn max_matches_truncates() {
    let rec = rec_1mhz();
    let pattern = parse("D0 f=1k 10", &channels(), rec).unwrap();
    // Alternating 1-cell runs: "10" repeats.
    let mut runs = Vec::new();
    let mut t = 0u64;
    for _ in 0..20 {
        runs.push(Run {
            start: t,
            len: 1000,
            value: 1,
        });
        t += 1000;
        runs.push(Run {
            start: t,
            len: 1000,
            value: 0,
        });
        t += 1000;
    }
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 3);
    assert_eq!(result.matches.len(), 3);
    assert!(result.truncated);
}
