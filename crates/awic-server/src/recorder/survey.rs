//! `survey` over `[t0_s, t1_s)` of a digital recording (live test F-04):
//! streams the retained runs through `awic_decode::survey::Surveyor` in
//! bounded windows, like `decode.rs` — memory stays bounded however long the
//! range is.

use awic_decode::survey::Surveyor;
use awic_proto::SurveyResult;

use super::{RecOpError, RecordingEntry, RecordingKindLocal};

const WINDOW_SAMPLES: u64 = 1_000_000;

pub fn survey(
    entry: &RecordingEntry,
    t0_s: Option<f64>,
    t1_s: Option<f64>,
) -> Result<SurveyResult, RecOpError> {
    if !matches!(entry.kind, RecordingKindLocal::Digital) {
        return Err(RecOpError::new(
            "not_digital",
            "survey needs a digital recording (for an analog one use search_pattern with level \
             patterns, or snapshot_recording + measure)",
        ));
    }
    let rate = entry.rate_hz.max(1) as f64;
    let rec = entry.read_rec();
    let (first, head) = (rec.first_sample(), rec.head_sample());
    let start = t0_s.map_or(first, |s| (s * rate).round().max(0.0) as u64);
    let end = t1_s
        .map_or(head, |s| (s * rate).round().max(0.0) as u64)
        .min(head);
    if start < first {
        return Err(RecOpError::new(
            "range_evicted",
            format!(
                "recording {} retains [{:.6}s, {:.6}s); the requested survey start ({:.6}s) is \
                 before that",
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
            format!("survey range [{start}, {end}) is empty"),
        ));
    }

    let bits: Vec<u8> = entry.channels.iter().map(|c| c.channel).collect();
    let mut surveyor: Option<Surveyor> = None;
    let mut cur = start;
    while cur < end {
        let window_end = (cur + WINDOW_SAMPLES).min(end);
        let runs = rec.runs(cur, window_end)?;
        let s = surveyor.get_or_insert_with(|| {
            let levels = runs.first().map_or(0, |r| r.value);
            Surveyor::new(&bits, entry.rate_hz, start, levels)
        });
        let input: Vec<awic_decode::Transition> = runs
            .iter()
            .filter(|r| r.start > start)
            .map(|r| awic_decode::Transition {
                sample_index: r.start,
                value: r.value,
            })
            .collect();
        s.push(&input);
        cur = window_end;
    }
    drop(rec);
    let surveyor = surveyor.unwrap_or_else(|| Surveyor::new(&bits, entry.rate_hz, start, 0));
    let mut result = crate::survey::to_wire(surveyor.finish(end), entry.rate_hz);
    result.rec = Some(entry.id);
    result.note = entry.range_note(start, end);
    Ok(result)
}
