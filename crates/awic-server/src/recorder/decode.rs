//! `rec.decode` (SPECIFICATION.md §6, T21): streams a recording's runs
//! through a decoder in bounded windows, mirroring `search.rs`'s
//! `search_digital` — never materializing `[t0_s, t1_s)`'s whole run list at
//! once, however long the requested range is.

use awic_proto::{RecDecodeResult, SearchedRange};

use super::{RecOpError, RecordingEntry, RecordingKindLocal};
use crate::decode::{
    auto_rate_channel, build_decoder, reject_analog_params, resolve_auto_baud, validate_channels,
    wants_auto_baud, DecodeCollector, DecodeSpec,
};

/// Same window size as `search.rs`'s `DIGITAL_WINDOW_SAMPLES`: bounds how
/// many runs live in memory at once for one `Recording::runs` call, not a
/// correctness knob (decoding cost is `O(edges)` regardless of window size —
/// SPEC §6).
const WINDOW_SAMPLES: u64 = 1_000_000;
/// How many of the line's *own* edges are enough for `baud: "auto"` (UART)
/// or `bitrate: "auto"` (CAN) to estimate from. Counting edges per channel
/// rather than transitions matters on a busy analyzer: an I²C bus on
/// neighbouring channels produces tens of thousands of transitions in
/// milliseconds, and a shared budget would be spent on them before the UART
/// line said anything (live test, 2026-09-16).
const AUTO_BAUD_ENOUGH_EDGES: usize = 2_000;

/// The first [`AUTO_BAUD_ENOUGH_EDGES`] edges of bit `rx` in `[start, end)`,
/// for rate detection. The whole range is read, on that one bit only, so a
/// quiet stretch costs a walk of the summary tree, not its samples: the probe
/// used to stop after 16 M samples (1 s at 16 MS/s), and a line that only
/// spoke 5.7 s into a recording was "only 0 pulses" (ttyUSB bench,
/// 2026-10-08).
fn rate_probe_edges(
    rec: &awic_record::Recording,
    start: u64,
    end: u64,
    rx: u8,
) -> Result<Vec<awic_decode::Transition>, awic_record::RecordError> {
    let mut edges = Vec::new();
    let mut level: Option<bool> = None;
    let mut cur = start;
    while cur < end && edges.len() < AUTO_BAUD_ENOUGH_EDGES {
        let window_end = cur.saturating_add(WINDOW_SAMPLES).min(end);
        for run in rec.runs_masked(cur, window_end, 1 << rx)? {
            let high = (run.value >> rx) & 1 == 1;
            if level != Some(high) {
                if level.is_some() {
                    edges.push(awic_decode::Transition {
                        sample_index: run.start,
                        value: run.value,
                    });
                }
                level = Some(high);
            }
        }
        cur = window_end;
    }
    Ok(edges)
}

fn to_transitions(runs: &[awic_record::Run]) -> Vec<awic_decode::Transition> {
    runs.iter()
        .map(|r| awic_decode::Transition {
            sample_index: r.start,
            value: r.value,
        })
        .collect()
}

pub fn decode(
    entry: &RecordingEntry,
    spec: &DecodeSpec,
    t0_s: Option<f64>,
    t1_s: Option<f64>,
) -> Result<RecDecodeResult, RecOpError> {
    if !matches!(entry.kind, RecordingKindLocal::Digital) {
        return Err(RecOpError::new(
            "not_digital",
            "rec.decode requires a digital recording; decode an analog one with rec.snapshot \
             and then capture.decode (which thresholds the waveform)",
        ));
    }
    reject_analog_params(spec.channels, spec.options)
        .map_err(|e| RecOpError::new(e.code, e.msg))?;
    let available: Vec<u8> = entry.channels.iter().map(|c| c.channel).collect();
    validate_channels(&available, spec.channels).map_err(|e| RecOpError::new(e.code, e.msg))?;

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
                "recording {} retains [{:.6}s, {:.6}s); the requested decode start ({:.6}s) is before that",
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
            format!("decode range [{start}, {end}) is empty"),
        ));
    }

    let rec = entry.read_rec();

    let (options, detected_baud) = if wants_auto_baud(spec.decoder, spec.options) {
        let rx = auto_rate_channel(spec.decoder, spec.channels)
            .map_err(|e| RecOpError::new(e.code, e.msg))?;
        let edges = rate_probe_edges(&rec, start, end, rx)?;
        let (options, baud) = resolve_auto_baud(spec.options, spec.channels, &edges, entry.rate_hz)
            .map_err(|e| RecOpError::new(e.code, e.msg))?;
        (options, Some(baud))
    } else {
        (spec.options.to_vec(), None)
    };

    let mut instance = build_decoder(spec.decoder, spec.channels, &options, entry.rate_hz)
        .map_err(|e| RecOpError::new("invalid_decoder_config", e.to_string()))?;
    let mut collector = DecodeCollector::new(spec, entry.rate_hz);

    let mut cur = start;
    let mut complete = true;
    while cur < end {
        let window_end = (cur + WINDOW_SAMPLES).min(end);
        let runs = rec.runs(cur, window_end)?;
        if !runs.is_empty() {
            if cur == start {
                // `t0_s` is arbitrary — often mid-transaction.
                instance.seed_at(start, runs[0].value);
            }
            let known_until = runs.last().map(|r| r.start + r.len).unwrap_or(window_end);
            collector.push(instance.feed(&to_transitions(&runs), known_until.min(window_end)));
            if collector.is_full() {
                complete = false;
                break;
            }
        }
        cur = window_end;
    }
    if complete {
        collector.push(instance.finish(end));
    }
    drop(rec);

    let mut out = collector.finish(complete);
    let (annotations, transactions) = out.take_lists();
    Ok(RecDecodeResult {
        rec: entry.id,
        decoder: spec.decoder,
        annotations,
        transactions,
        summary: out.summary,
        truncated: out.truncated,
        omitted: out.omitted,
        detected_baud,
        note: entry.range_note(start, end),
        decoded: SearchedRange {
            t0_s: start as f64 / rate,
            t1_s: end as f64 / rate,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awic_record::{Recording, RecordingKind, RecordingSpec};

    /// The ttyUSB bench: a UART line that only speaks well into the range.
    /// 20 M quiet samples at 1 MHz (past the old 16 M-sample probe), then
    /// "Hello" at 9600 Bd on bit 5.
    #[test]
    fn the_rate_probe_reads_past_a_long_quiet_stretch() {
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Digital,
            rate_hz: 1_000_000,
        });
        let quiet = vec![0xFFu8; 1_000_000];
        for _ in 0..20 {
            rec.append_packed(&quiet).unwrap();
        }
        let bit = 1_000_000 / 9600;
        let mut line = Vec::new();
        for &c in b"Hello" {
            let bits = std::iter::once(0)
                .chain((0..8).map(|i| (c >> i) & 1))
                .chain(std::iter::once(1));
            for b in bits {
                let v = if b == 1 { 0xFF } else { !(1u8 << 5) };
                line.extend(std::iter::repeat_n(v, bit as usize));
            }
        }
        line.extend(std::iter::repeat_n(0xFF, 1000));
        rec.append_packed(&line).unwrap();

        let end = rec.head_sample();
        let edges = rate_probe_edges(&rec, 0, end, 5).unwrap();
        assert!(edges.len() >= 20, "{} edges", edges.len());
        assert!(edges[0].sample_index >= 20_000_000);
        let e = awic_decode::baud::estimate_baud(&edges, 5, 1_000_000).expect("a rate");
        assert_eq!(e.baud, 9600, "{e:?}");
    }
}
