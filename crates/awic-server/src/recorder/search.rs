//! `rec.search` (SPECIFICATION.md §2.9.3): history search in bounded
//! windows, never materializing the whole range's runs/samples at once —
//! `Recording::runs`/`runs_masked`/`extract_analog` all return a `Vec` for
//! the exact span asked, so this module is the one that keeps that span
//! small regardless of how much history a client's `t0_s`/`t1_s` spans.

use awic_pattern::Pattern;
use awic_proto::{PatternMatch, RecSearchResult, SearchedRange};

use super::{RecOpError, RecordingEntry};

/// Hard cap: default 16, hard maximum 1024 (SPEC §2.9.3, §8.3's context
/// budget — a tool result must never carry unbounded data).
const DEFAULT_MAX_MATCHES: usize = 16;
const HARD_MAX_MATCHES: usize = 1024;

/// Samples per digital window: cheap either way (cost is `O(edges)`, SPEC
/// §2.9.2), just bounding how many runs live in memory at once for a single
/// `runs_masked` call.
const DIGITAL_WINDOW_SAMPLES: u64 = 1_000_000;
/// Matches the analog block size (SPEC §2.8.2's 64 Ki samples/channel) so a
/// window's summary bucket lines up with real storage granularity.
const ANALOG_WINDOW_SAMPLES: u64 = 65_536;

pub fn search(
    entry: &RecordingEntry,
    pattern_text: &str,
    t0_s: Option<f64>,
    t1_s: Option<f64>,
    max_matches: Option<u32>,
) -> Result<RecSearchResult, RecOpError> {
    let pattern = entry.parse_pattern(pattern_text)?;
    let max_matches =
        (max_matches.unwrap_or(DEFAULT_MAX_MATCHES as u32) as usize).clamp(1, HARD_MAX_MATCHES);

    let rate = entry.rate_hz.max(1) as f64;
    let (first, head) = {
        let g = entry.read_rec();
        (g.first_sample(), g.head_sample())
    };
    let start = t0_s.map_or(first, |s| (s * rate).round().max(0.0) as u64);
    let end = t1_s
        .map_or(head, |s| (s * rate).round().max(0.0) as u64)
        .min(head);

    if start < first {
        return Err(RecOpError::new(
            "range_evicted",
            format!(
                "recording {} retains [{:.6}s, {:.6}s); the requested search start ({:.6}s) is before that",
                entry.id,
                first as f64 / rate,
                head as f64 / rate,
                start as f64 / rate,
            ),
        ));
    }
    if start >= end {
        return Err(RecOpError::new(
            "empty_range",
            format!("search range [{start}, {end}) is empty"),
        ));
    }

    // A search starting exactly at the recording's retained start (the
    // default, or an explicit `t0_s` landing there) may begin mid-run: the
    // first run `runs_masked` hands back could have begun before the data
    // did, or before the last eviction (SPEC §2.9) — its
    // `start` is this boundary, not necessarily where it truly began, and
    // its `len` is only a lower bound. `awic-pattern` treats it as such
    // rather than as a known run, so a match cannot be spuriously anchored
    // on it (the "0.0000 s" match seen on hardware).
    let first_run_open_left = start == first;

    // A window opening inside a run (start > first, same masked level on both
    // sides of `start`) clips that run to `start`; a match anchored on it
    // reports the window start as the event's onset (B-44). The recording
    // still holds the earlier samples, but the matcher saw only the clipped
    // run, so such a match is marked `truncated` instead.
    let cut_at_start = start > first
        && !pattern.is_analog()
        && entry
            .read_rec()
            .runs_masked(start - 1, start + 1, pattern.mask().unwrap_or(0xFF))
            .is_ok_and(|runs| runs.len() == 1);

    let result = if pattern.is_analog() {
        search_analog(entry, &pattern, start, end, max_matches)
    } else {
        search_digital(
            entry,
            &pattern,
            start,
            end,
            max_matches,
            first_run_open_left,
        )
    };

    Ok(RecSearchResult {
        note: entry.range_note(start, end),
        matches: result
            .matches
            .iter()
            .map(|m| PatternMatch {
                t_s: m.sample as f64 / rate,
                sample: m.sample,
                truncated: cut_at_start && m.sample == start,
            })
            .collect(),
        truncated: result.truncated,
        searched: SearchedRange {
            t0_s: start as f64 / rate,
            t1_s: end as f64 / rate,
        },
        glitches: result.glitches,
    })
}

/// Digital search: `runs_masked` over successive bounded windows (never the
/// whole range at once), fed into one `Matcher` that spans window
/// boundaries exactly like it spans append-chunk boundaries during
/// `rec.wait` — same contract (`awic_pattern::Matcher::feed_runs`'s doc
/// comment on repeated/adjacent runs at a chunk boundary).
fn search_digital(
    entry: &RecordingEntry,
    pattern: &Pattern,
    start: u64,
    end: u64,
    max_matches: usize,
    first_run_open_left: bool,
) -> awic_pattern::SearchResult {
    let mask = pattern.mask().unwrap_or(0xFF);
    let mut matcher = awic_pattern::Matcher::new(pattern);
    if first_run_open_left {
        matcher.mark_first_run_open_left();
    }
    let mut result = awic_pattern::SearchResult::default();
    let rec = entry.read_rec();

    let mut cur = start;
    'windows: while cur < end {
        let window_end = (cur + DIGITAL_WINDOW_SAMPLES).min(end);
        let runs = match rec.runs_masked(cur, window_end, mask) {
            Ok(runs) => runs,
            Err(_) => break, // Shouldn't happen: `cur`/`window_end` stay in range.
        };
        if !runs.is_empty() {
            let known_until = runs.last().map(|r| r.start + r.len).unwrap_or(window_end);
            let pattern_runs = super::to_pattern_runs(&runs);
            for m in matcher.feed_runs(&pattern_runs, known_until) {
                if m.sample < start || m.sample >= end {
                    continue;
                }
                result.matches.push(m);
                if result.matches.len() >= max_matches {
                    result.truncated = true;
                    break 'windows;
                }
            }
        }
        cur = window_end;
    }
    if !result.truncated {
        for m in matcher.finish() {
            if m.sample < start || m.sample >= end {
                continue;
            }
            result.matches.push(m);
            if result.matches.len() >= max_matches {
                result.truncated = true;
                break;
            }
        }
    }
    result.glitches = matcher.glitches_absorbed();
    result
}

/// Analog search: per-window summaries prune whole windows via
/// `Matcher::try_skip_analog` (SPEC §2.8.3/§2.9.3), extracting raw samples
/// only for a window the summary cannot rule out.
fn search_analog(
    entry: &RecordingEntry,
    pattern: &Pattern,
    start: u64,
    end: u64,
    max_matches: usize,
) -> awic_pattern::SearchResult {
    let channel_index = match pattern {
        Pattern::Analog(p) => p.channel_index,
        _ => 0,
    };
    let mut matcher = awic_pattern::Matcher::new(pattern);
    let mut result = awic_pattern::SearchResult::default();
    let rec = entry.read_rec();

    let mut cur = start;
    'windows: while cur < end {
        let window_end = (cur + ANALOG_WINDOW_SAMPLES).min(end);
        let (min_code, max_code) = match rec.summarize(cur, window_end) {
            Ok(awic_record::Summary::Analog(bucket)) => bucket
                .channels
                .get(channel_index as usize)
                .map(|c| (c.min, c.max))
                .unwrap_or((i32::MIN, i32::MAX)),
            _ => (i32::MIN, i32::MAX),
        };
        let skip = matcher.try_skip_analog(cur, window_end - cur, min_code, max_code);
        if !skip {
            if let Ok(codes) = rec.extract_analog(cur, window_end) {
                for m in matcher.feed_analog(cur, &codes) {
                    if m.sample < start || m.sample >= end {
                        continue;
                    }
                    result.matches.push(m);
                    if result.matches.len() >= max_matches {
                        result.truncated = true;
                        break 'windows;
                    }
                }
            }
        }
        cur = window_end;
    }
    result
}
