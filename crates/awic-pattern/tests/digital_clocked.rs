//! Semantic tests for clocked (synchronous) patterns (SPEC §2.9.1,
//! SPEC §15 S-02).

mod support;

use awic_pattern::{parse, search_runs, ChannelInfo, ChannelKind, Run};
use support::rec_1mhz;

fn channels() -> Vec<ChannelInfo> {
    vec![
        ChannelInfo {
            name: "D0".into(),
            kind: ChannelKind::Digital { bit: 0 },
        }, // clock
        ChannelInfo {
            name: "D1".into(),
            kind: ChannelKind::Digital { bit: 1 },
        }, // data
    ]
}

/// Builds runs for D1 (data, bit 1) clocked out on D0's (bit 0) rising
/// edges, one bit per half-period `half`, data valid before each edge.
fn clocked_runs(half: u64, data_bits: &[bool]) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut t = 0u64;
    // idle period before the first edge
    runs.push(Run {
        start: t,
        len: half,
        value: 0,
    });
    t += half;
    for &bit in data_bits {
        // Set up data before the rising edge, then raise the clock.
        let data = bit;
        runs.push(Run {
            start: t,
            len: half,
            value: (data as u8) << 1 | 1, // clk high
        });
        t += half;
        runs.push(Run {
            start: t,
            len: half,
            value: (data as u8) << 1, // clk low
        });
        t += half;
    }
    runs
}

#[test]
fn clocked_pattern_matches_sampled_bits() {
    let pattern = parse("D1 clk=D0 rising 1010x", &channels(), rec_1mhz()).unwrap();
    // Bits sampled at each rising edge: 1,0,1,0,1 — "1010x" should match
    // starting at the first edge (x matches the trailing 1).
    let runs = clocked_runs(100, &[true, false, true, false, true]);
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    // First rising edge is at sample 100 (end of the idle run).
    assert_eq!(result.matches[0].sample, 100);
    assert_eq!(
        result.matches[0].decided_at,
        result.matches[0].sample + 4 * 200
    );
}

#[test]
fn clocked_pattern_ignores_data_changes_without_a_clock_edge() {
    let pattern = parse("D1 clk=D0 rising 11", &channels(), rec_1mhz()).unwrap();
    // Clock stays low the whole time; data toggles. No edges, no matches.
    let runs = [
        Run {
            start: 0,
            len: 100,
            value: 0b00,
        },
        Run {
            start: 100,
            len: 100,
            value: 0b10,
        },
        Run {
            start: 200,
            len: 100,
            value: 0b00,
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert!(result.matches.is_empty());
}

#[test]
fn clocked_falling_edge_variant() {
    let pattern = parse("D1 clk=D0 falling 01", &channels(), rec_1mhz()).unwrap();
    // Two falling edges of D0, with D1 = 0 then 1 at those instants.
    let runs = [
        Run {
            start: 0,
            len: 100,
            value: 0b01, // clk high, data low
        },
        Run {
            start: 100,
            len: 100,
            value: 0b00, // clk falls (data still 0) -> sample 0
        },
        Run {
            start: 200,
            len: 100,
            value: 0b11, // clk rises, data goes high
        },
        Run {
            start: 300,
            len: 100,
            value: 0b10, // clk falls (data 1) -> sample 1
        },
    ];
    let result = search_runs(&pattern, &runs, 0..u64::MAX, 16);
    assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
    assert_eq!(result.matches[0].sample, 100); // first falling edge
    assert_eq!(result.matches[0].decided_at, 300); // second falling edge
}
