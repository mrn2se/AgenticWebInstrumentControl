//! Property tests for digital recordings (SPECIFICATION.md §2.8.1, §2.8.3,
//! §3.4): random data appended in random chunk sizes (including 1-sample
//! chunks and chunks that split runs), interleaved with random evictions,
//! must still answer `extract_packed`, `summarize`, `runs`/`runs_masked` and
//! `view` exactly as a brute-force reference over the same bytes.

use awic_record::{Recording, RecordingKind, RecordingSpec};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Append(Vec<u8>),
    Evict,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => prop::collection::vec(any::<u8>(), 1..6).prop_map(Op::Append),
        6 => (any::<u8>(), 1usize..150).prop_map(|(v, n)| Op::Append(vec![v; n])),
        2 => Just(Op::Evict),
    ]
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op_strategy(), 1..60)
}

/// Brute-force `(and, or, high_count, edge_count, first, last)` over
/// `model[start..end]`, mirroring `DigitalBucket`'s exact semantics.
fn brute_bucket(model: &[u8], start: usize, end: usize) -> (u8, u8, [u64; 8], [u64; 8], u8, u8) {
    let mut and = 0xFFu8;
    let mut or = 0u8;
    let mut high = [0u64; 8];
    let mut edge = [0u64; 8];
    for i in start..end {
        let v = model[i];
        and &= v;
        or |= v;
        for ch in 0..8u8 {
            if v & (1 << ch) != 0 {
                high[ch as usize] += 1;
            }
        }
        if i > start {
            let prev = model[i - 1];
            for ch in 0..8u8 {
                if (v ^ prev) & (1 << ch) != 0 {
                    edge[ch as usize] += 1;
                }
            }
        }
    }
    (and, or, high, edge, model[start], model[end - 1])
}

fn read_uvarint(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = bytes[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return result;
        }
        shift += 7;
    }
}

/// Expand a `awic_record::ViewKind::DigitalRuns` payload back into bytes.
fn expand_runs_payload(payload: &[u8], item_count: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    for _ in 0..item_count {
        let value = payload[pos];
        pos += 1;
        let len = read_uvarint(payload, &mut pos);
        out.extend(std::iter::repeat_n(value, len as usize));
    }
    out
}

fn apply_ops(rec: &mut Recording, model: &mut Vec<u8>, ops: &[Op]) {
    for op in ops {
        match op {
            Op::Append(chunk) => {
                rec.append_packed(chunk).unwrap();
                model.extend_from_slice(chunk);
            }
            Op::Evict => {
                rec.evict_oldest_block();
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn extract_matches_model(
        ops in ops_strategy(),
        ra in any::<u64>(),
        rb in any::<u64>(),
    ) {
        let mut rec = Recording::new(RecordingSpec { kind: RecordingKind::Digital, rate_hz: 1_000_000 });
        let mut model = Vec::new();
        apply_ops(&mut rec, &mut model, &ops);

        let first = rec.first_sample();
        let head = rec.head_sample();
        prop_assume!(head > first);
        let span = head - first;
        let start = first + ra % span;
        let end = (start + 1 + rb % span).min(head);
        prop_assume!(start < end);

        let extracted = rec.extract_packed(start, end).unwrap();
        prop_assert_eq!(&extracted[..], &model[start as usize..end as usize]);
    }

    #[test]
    fn summarize_matches_brute_force(
        ops in ops_strategy(),
        ra in any::<u64>(),
        rb in any::<u64>(),
    ) {
        let mut rec = Recording::new(RecordingSpec { kind: RecordingKind::Digital, rate_hz: 1_000_000 });
        let mut model = Vec::new();
        apply_ops(&mut rec, &mut model, &ops);

        let first = rec.first_sample();
        let head = rec.head_sample();
        prop_assume!(head > first);
        let span = head - first;
        let start = first + ra % span;
        let end = (start + 1 + rb % span).min(head);
        prop_assume!(start < end);

        let summary = rec.summarize(start, end).unwrap();
        let awic_record::Summary::Digital(bucket) = summary else { panic!("expected digital summary") };
        let (and, or, high, edge, first_v, last_v) = brute_bucket(&model, start as usize, end as usize);
        prop_assert_eq!(bucket.and, and);
        prop_assert_eq!(bucket.or, or);
        prop_assert_eq!(bucket.high_count, high);
        prop_assert_eq!(bucket.edge_count, edge);
        prop_assert_eq!(bucket.first_value, first_v);
        prop_assert_eq!(bucket.last_value, last_v);
        prop_assert_eq!(bucket.count, end - start);
    }

    #[test]
    fn runs_and_masked_runs_reconstruct_model(
        ops in ops_strategy(),
        ra in any::<u64>(),
        rb in any::<u64>(),
        mask in any::<u8>(),
    ) {
        let mut rec = Recording::new(RecordingSpec { kind: RecordingKind::Digital, rate_hz: 1_000_000 });
        let mut model = Vec::new();
        apply_ops(&mut rec, &mut model, &ops);

        let first = rec.first_sample();
        let head = rec.head_sample();
        prop_assume!(head > first);
        let span = head - first;
        let start = first + ra % span;
        let end = (start + 1 + rb % span).min(head);
        prop_assume!(start < end);

        let runs = rec.runs(start, end).unwrap();
        let mut expanded = Vec::new();
        let mut cursor = start;
        for r in &runs {
            prop_assert_eq!(r.start, cursor);
            expanded.extend(std::iter::repeat_n(r.value, r.len as usize));
            cursor += r.len;
        }
        prop_assert_eq!(cursor, end);
        prop_assert_eq!(&expanded[..], &model[start as usize..end as usize]);
        // Contiguous: no two adjacent runs share a value.
        for w in runs.windows(2) {
            prop_assert_ne!(w[0].value, w[1].value);
        }

        let masked_runs = rec.runs_masked(start, end, mask).unwrap();
        let mut masked_expanded = Vec::new();
        let mut cursor = start;
        for r in &masked_runs {
            prop_assert_eq!(r.start, cursor);
            expanded_push(&mut masked_expanded, r.value, r.len);
            cursor += r.len;
        }
        prop_assert_eq!(cursor, end);
        let expected_masked: Vec<u8> = model[start as usize..end as usize].iter().map(|&b| b & mask).collect();
        prop_assert_eq!(&masked_expanded[..], &expected_masked[..]);
        for w in masked_runs.windows(2) {
            prop_assert_ne!(w[0].value, w[1].value);
        }
    }

    #[test]
    fn view_items_match_brute_force(
        ops in ops_strategy(),
        ra in any::<u64>(),
        rb in any::<u64>(),
        columns in 1u32..40,
    ) {
        let mut rec = Recording::new(RecordingSpec { kind: RecordingKind::Digital, rate_hz: 1_000_000 });
        let mut model = Vec::new();
        apply_ops(&mut rec, &mut model, &ops);

        let first = rec.first_sample();
        let head = rec.head_sample();
        prop_assume!(head > first);
        let span = head - first;
        let start = first + ra % span;
        let end = (start + 1 + rb % span).min(head);
        prop_assume!(start < end);

        let view = rec.view(start, end, columns).unwrap();
        match view.kind {
            awic_record::ViewKind::DigitalRuns => {
                let bytes = expand_runs_payload(&view.payload, view.item_count);
                prop_assert_eq!(view.first_sample, start);
                prop_assert_eq!(&bytes[..], &model[start as usize..end as usize]);
            }
            awic_record::ViewKind::DigitalSummary => {
                prop_assert!(view.samples_per_item.is_power_of_two());
                prop_assert_eq!(view.first_sample % view.samples_per_item, 0);
                prop_assert!(view.first_sample <= start);
                for i in 0..view.item_count {
                    let item_start_raw = view.first_sample + i * view.samples_per_item;
                    let item_end = (item_start_raw + view.samples_per_item).min(end);
                    let lo = item_start_raw.max(start) as usize;
                    let hi = item_end as usize;
                    if lo >= hi {
                        continue;
                    }
                    let (and, or, ..) = brute_bucket(&model, lo, hi);
                    prop_assert_eq!(view.payload[(i * 2) as usize], and);
                    prop_assert_eq!(view.payload[(i * 2 + 1) as usize], or);
                }
            }
            _ => panic!("digital recording produced non-digital view kind"),
        }
    }
}

fn expanded_push(out: &mut Vec<u8>, value: u8, len: u64) {
    out.extend(std::iter::repeat_n(value, len as usize));
}
