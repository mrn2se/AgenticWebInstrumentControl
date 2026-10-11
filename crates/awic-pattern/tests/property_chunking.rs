//! Property test (SPEC §2.9.3 task note): feeding the same signal in
//! random chunkings — runs split at random points, `known_until` advancing
//! in random steps — yields exactly the same matches as one-shot feeding.

mod support;

use awic_pattern::{parse, search_runs, ChannelInfo, ChannelKind, Matcher, RecordingInfo, Run};
use proptest::prelude::*;

fn channels() -> Vec<ChannelInfo> {
    vec![ChannelInfo {
        name: "D0".into(),
        kind: ChannelKind::Digital { bit: 0 },
    }]
}

fn rec() -> RecordingInfo {
    RecordingInfo {
        rate_hz: 1_000_000.0,
        sample_bits: 8,
        channel_count: 0,
    }
}

/// Builds a run list from a bit string ("0"/"1" per cell) at `cell_samples`
/// samples/cell, merging consecutive equal bits into one run (as a real
/// recorder would).
fn runs_from_bits(bits: &str, cell_samples: u64) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut t = 0u64;
    for c in bits.chars() {
        let v = if c == '1' { 1 } else { 0 };
        if let Some(last) = runs.last_mut() {
            if last.value == v {
                last.len += cell_samples;
                t += cell_samples;
                continue;
            }
        }
        runs.push(Run {
            start: t,
            len: cell_samples,
            value: v,
        });
        t += cell_samples;
    }
    runs
}

/// Splits `runs` at arbitrary sample positions (never inside a run — only
/// at existing run boundaries, to stay a legal "runs so far" prefix — and
/// re-derives each chunk from the run list truncated/extended at that
/// point) into a list of (runs_for_this_call, known_until) increments that
/// [`Matcher::feed_runs`] can be fed in order, re-splitting/re-growing the
/// last run to simulate it being open across several calls.
fn chunk_plan(runs: &[Run], cut_points: &[usize]) -> Vec<(Vec<Run>, u64)> {
    // cut_points are cell-granularity split positions expressed as sample
    // offsets into the whole stream (deduped, sorted, clipped to the
    // stream length).
    let total: u64 = runs.iter().map(|r| r.len).sum();
    let mut cuts: Vec<u64> = cut_points
        .iter()
        .map(|&c| (c as u64) % total.max(1))
        .filter(|&c| c > 0)
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.push(total);

    let mut out = Vec::new();
    let mut prev_end = 0u64;
    for &end in &cuts {
        if end <= prev_end {
            continue;
        }
        // Build the run list covering [0, end): full runs, with the last
        // one clipped to `end` if it straddles the cut.
        let mut chunk = Vec::new();
        let mut acc = 0u64;
        for r in runs {
            if acc >= end {
                break;
            }
            let r_end = acc + r.len;
            if r_end <= end {
                chunk.push(*r);
            } else {
                chunk.push(Run {
                    start: r.start,
                    len: end - acc,
                    value: r.value,
                });
            }
            acc = r_end;
        }
        // Only the part of `chunk` at/after prev_end's run is "new" for
        // this call; feed_runs wants the runs continuing from last time,
        // so pass only runs whose end is > prev_end, clipping the first if
        // it starts before prev_end (never happens here since prev_end is
        // always an exact previous run boundary or 0).
        let new_part: Vec<Run> = {
            let mut acc2 = 0u64;
            let mut v = Vec::new();
            for r in &chunk {
                let r_end = acc2 + r.len;
                if r_end > prev_end {
                    v.push(*r);
                }
                acc2 = r_end;
            }
            v
        };
        out.push((new_part, end));
        prev_end = end;
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn chunking_never_changes_the_matches(
        bits in "[01]{1,40}",
        cut_seed in prop::collection::vec(0usize..2000, 0..10),
    ) {
        let cell_samples = 1000u64;
        let runs = runs_from_bits(&bits, cell_samples);
        let pattern = parse("D0 f=1k 1010x", &channels(), rec()).unwrap();

        let full_range = 0..(runs.iter().map(|r| r.start + r.len).max().unwrap_or(0) + 1);
        let one_shot = search_runs(&pattern, &runs, full_range.clone(), 1024);

        let plan = chunk_plan(&runs, &cut_seed);
        let mut matcher = Matcher::new(&pattern);
        let mut chunked_matches = Vec::new();
        for (chunk, known_until) in &plan {
            chunked_matches.extend(matcher.feed_runs(chunk, *known_until));
        }
        // `search_runs` finishes the stream (treats the tail as closed);
        // do the same here for a fair comparison.
        chunked_matches.extend(matcher.finish());
        chunked_matches.retain(|m| m.sample < full_range.end);
        prop_assert_eq!(chunked_matches, one_shot.matches);
    }
}
