//! Throughput demonstration for bit-sequence history search (SPEC §2.9.2:
//! "searching iterates runs, so its cost is O(edges), not O(samples)").
//!
//! Run with `cargo run -p awic-pattern --release --example perf_bitseq`.

use std::time::Instant;

use awic_pattern::{parse, search_runs, ChannelInfo, ChannelKind, RecordingInfo, Run};

fn main() {
    const RUN_COUNT: u64 = 10_000_000;
    const CELL_SAMPLES: u64 = 1000;

    // A pseudo-random bitstream (one run per bit cell) rather than a short
    // repeating byte: a realistic stream matches the sought pattern only
    // occasionally, so this stresses run traversal rather than output
    // collection (10M runs but a small, bounded number of matches).
    let mut runs = Vec::with_capacity(RUN_COUNT as usize);
    let mut t = 0u64;
    let mut lfsr: u32 = 0xACE1;
    while (runs.len() as u64) < RUN_COUNT {
        let bit = lfsr & 1;
        lfsr = (lfsr >> 1) ^ (0u32.wrapping_sub(bit) & 0xB400_0000);
        runs.push(Run {
            start: t,
            len: CELL_SAMPLES,
            value: bit as u8,
        });
        t += CELL_SAMPLES;
    }

    let channels = vec![ChannelInfo {
        name: "D0".into(),
        kind: ChannelKind::Digital { bit: 0 },
    }];
    let rec = RecordingInfo {
        rate_hz: 1_000_000.0,
        sample_bits: 8,
        channel_count: 0,
    };
    let pattern = parse("D0 f=1k 1011001010110010", &channels, rec).unwrap();

    // Uncapped (well past SPEC §2.9.3's 1024 hard maximum, which is a
    // caller/server-level limit, not a matcher one) so this exercises the
    // full 10M runs rather than stopping at the first `max_matches`.
    let start = Instant::now();
    let result = search_runs(&pattern, &runs, 0..t, usize::MAX);
    let elapsed = start.elapsed();

    println!(
        "{RUN_COUNT} runs, {} matches, truncated={}, glitches={}, {:?}",
        result.matches.len(),
        result.truncated,
        result.glitches,
        elapsed
    );
    assert!(
        elapsed.as_secs_f64() < 1.0,
        "search over {RUN_COUNT} runs took {elapsed:?}, expected well under 1s in release"
    );
}
