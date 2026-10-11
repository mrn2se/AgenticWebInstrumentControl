//! Property tests for analog recordings (SPECIFICATION.md §2.8.2, §2.8.3,
//! §3.4): random codes appended in random chunk sizes, interleaved with
//! random evictions, must still answer `extract_analog`, `summarize` and
//! `view` exactly as a brute-force reference — including once chunks are
//! large enough to close a real 64 Ki-sample codec block.

use awic_record::{Recording, RecordingKind, RecordingSpec};
use proptest::prelude::*;

const CHANNELS: usize = 2;
const SAMPLE_BITS: u8 = 8;

fn code_strategy() -> impl Strategy<Value = i32> {
    if SAMPLE_BITS == 16 {
        (i32::from(i16::MIN)..=i32::from(i16::MAX)).boxed()
    } else {
        (0i32..=255).boxed()
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// Per-channel codes for one chunk, same length for every channel.
    Append(Vec<Vec<i32>>),
    Evict,
}

fn append_op_strategy() -> impl Strategy<Value = Op> {
    (1usize..4000).prop_flat_map(|n| {
        prop::collection::vec(prop::collection::vec(code_strategy(), n), CHANNELS)
            .prop_map(Op::Append)
    })
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![8 => append_op_strategy(), 2 => Just(Op::Evict)]
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op_strategy(), 1..10)
}

fn encode_frame(codes: &[Vec<i32>]) -> Vec<u8> {
    let n = codes[0].len();
    let mut buf = Vec::with_capacity(n * CHANNELS * if SAMPLE_BITS == 16 { 2 } else { 1 });
    for i in 0..n {
        for ch in codes {
            if SAMPLE_BITS == 16 {
                buf.extend_from_slice(&(ch[i] as i16).to_le_bytes());
            } else {
                buf.push(ch[i] as u8);
            }
        }
    }
    buf
}

fn decode_frame(bytes: &[u8]) -> Vec<Vec<i32>> {
    let bps = if SAMPLE_BITS == 16 { 2 } else { 1 };
    let frame = CHANNELS * bps;
    let n = bytes.len() / frame;
    let mut out: Vec<Vec<i32>> = (0..CHANNELS).map(|_| Vec::with_capacity(n)).collect();
    for i in 0..n {
        for (ch, out_ch) in out.iter_mut().enumerate() {
            let off = i * frame + ch * bps;
            let code = if SAMPLE_BITS == 16 {
                i16::from_le_bytes([bytes[off], bytes[off + 1]]) as i32
            } else {
                bytes[off] as i32
            };
            out_ch.push(code);
        }
    }
    out
}

fn apply_ops(rec: &mut Recording, model: &mut [Vec<i32>], ops: &[Op]) {
    for op in ops {
        match op {
            Op::Append(chunk) => {
                rec.append_analog(&encode_frame(chunk)).unwrap();
                for (ch, chunk_ch) in chunk.iter().enumerate() {
                    model[ch].extend_from_slice(chunk_ch);
                }
            }
            Op::Evict => {
                rec.evict_oldest_block();
            }
        }
    }
}

fn brute_min_max_sum(model: &[i32], start: usize, end: usize) -> (i32, i32, i64) {
    let slice = &model[start..end];
    let min = *slice.iter().min().unwrap();
    let max = *slice.iter().max().unwrap();
    let sum: i64 = slice.iter().map(|&c| c as i64).sum();
    (min, max, sum)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn extract_matches_model(
        ops in ops_strategy(),
        ra in any::<u64>(),
        rb in any::<u64>(),
    ) {
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Analog { channel_count: CHANNELS as u8, sample_bits: SAMPLE_BITS },
            rate_hz: 1_000_000,
        });
        let mut model = vec![Vec::new(); CHANNELS];
        apply_ops(&mut rec, &mut model, &ops);

        let first = rec.first_sample();
        let head = rec.head_sample();
        prop_assume!(head > first);
        let span = head - first;
        let start = first + ra % span;
        let end = (start + 1 + rb % span).min(head);
        prop_assume!(start < end);

        let extracted = rec.extract_analog(start, end).unwrap();
        let decoded = decode_frame(&extracted);
        for ch in 0..CHANNELS {
            prop_assert_eq!(&decoded[ch][..], &model[ch][start as usize..end as usize]);
        }
    }

    #[test]
    fn summarize_matches_brute_force(
        ops in ops_strategy(),
        ra in any::<u64>(),
        rb in any::<u64>(),
    ) {
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Analog { channel_count: CHANNELS as u8, sample_bits: SAMPLE_BITS },
            rate_hz: 1_000_000,
        });
        let mut model = vec![Vec::new(); CHANNELS];
        apply_ops(&mut rec, &mut model, &ops);

        let first = rec.first_sample();
        let head = rec.head_sample();
        prop_assume!(head > first);
        let span = head - first;
        let start = first + ra % span;
        let end = (start + 1 + rb % span).min(head);
        prop_assume!(start < end);

        let summary = rec.summarize(start, end).unwrap();
        let awic_record::Summary::Analog(bucket) = summary else { panic!("expected analog summary") };
        prop_assert_eq!(bucket.count, end - start);
        for (ch, channel_model) in model.iter().enumerate() {
            let (min, max, sum) = brute_min_max_sum(channel_model, start as usize, end as usize);
            prop_assert_eq!(bucket.channels[ch].min, min);
            prop_assert_eq!(bucket.channels[ch].max, max);
            prop_assert_eq!(bucket.channels[ch].sum, sum);
        }
    }

    #[test]
    fn view_items_match_brute_force(
        ops in ops_strategy(),
        ra in any::<u64>(),
        rb in any::<u64>(),
        columns in 1u32..40,
    ) {
        let mut rec = Recording::new(RecordingSpec {
            kind: RecordingKind::Analog { channel_count: CHANNELS as u8, sample_bits: SAMPLE_BITS },
            rate_hz: 1_000_000,
        });
        let mut model = vec![Vec::new(); CHANNELS];
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
            awic_record::ViewKind::AnalogRaw => {
                prop_assert_eq!(view.first_sample, start);
                let decoded = decode_frame(&view.payload);
                for ch in 0..CHANNELS {
                    prop_assert_eq!(&decoded[ch][..], &model[ch][start as usize..end as usize]);
                }
            }
            awic_record::ViewKind::AnalogSummary => {
                prop_assert!(view.samples_per_item.is_power_of_two());
                prop_assert_eq!(view.first_sample % view.samples_per_item, 0);
                let bps = if SAMPLE_BITS == 16 { 2usize } else { 1 };
                let per_channel_bytes = view.item_count as usize * bps * 2;
                for (ch, channel_model) in model.iter().enumerate() {
                    let base = ch * per_channel_bytes;
                    for i in 0..view.item_count {
                        let item_start_raw = view.first_sample + i * view.samples_per_item;
                        let item_end = (item_start_raw + view.samples_per_item).min(end);
                        let lo = item_start_raw.max(start) as usize;
                        let hi = item_end as usize;
                        if lo >= hi {
                            continue;
                        }
                        let (min, max, _sum) = brute_min_max_sum(channel_model, lo, hi);
                        let off = base + (i as usize) * bps * 2;
                        let (got_min, got_max) = if SAMPLE_BITS == 16 {
                            (
                                i16::from_le_bytes([view.payload[off], view.payload[off + 1]]) as i32,
                                i16::from_le_bytes([view.payload[off + 2], view.payload[off + 3]]) as i32,
                            )
                        } else {
                            (view.payload[off] as i32, view.payload[off + 1] as i32)
                        };
                        prop_assert_eq!(got_min, min);
                        prop_assert_eq!(got_max, max);
                    }
                }
            }
            _ => panic!("analog recording produced non-analog view kind"),
        }
    }
}
