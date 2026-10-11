//! Min/max decimation (SPECIFICATION.md §2.7, §3.2).
//!
//! One function, two call sites: the server-side backpressure path (encode
//! `min/max pairs per screen column` above a stream's high-water mark rather
//! than dropping or buffering unboundedly) and `read_capture` (cap
//! `max_points` server-side per the MCP context-budget rule, §8.3). Plain
//! decimation — keeping every Nth sample — aliases a glitch into a
//! clean-looking lie; min/max never does, because every bucket's extremes
//! are preserved exactly.

/// Yields the `[start, end)` index range of each of `buckets` contiguous,
/// (almost) equal-sized groups over `n` samples.
///
/// Shared by [`minmax_decimate`] and [`bitwise_decimate`] so that an analog
/// capture and a digital capture decimated to the same bucket count stay
/// aligned in time, bucket for bucket.
fn bucket_ranges(n: usize, buckets: usize) -> impl Iterator<Item = (usize, usize)> {
    let buckets = buckets.min(n);
    (0..buckets).map(move |b| {
        let start = b * n / buckets;
        let end = (((b + 1) * n / buckets).max(start + 1)).min(n);
        (start, end)
    })
}

/// Splits `samples` into `buckets` contiguous, (almost) equal-sized groups
/// and returns the `(min, max)` of each — the classic "min/max envelope"
/// decimation used for waveform previews.
///
/// - Returns an empty `Vec` if `samples` is empty or `buckets == 0`.
/// - Never returns more buckets than there are input samples (one sample per
///   bucket, at most).
/// - For **analog** samples only (`u8` interleaved, `i16`). **Do not use this
///   on packed digital data** — see [`bitwise_decimate`].
pub fn minmax_decimate<T: Copy + PartialOrd>(samples: &[T], buckets: usize) -> Vec<(T, T)> {
    if samples.is_empty() || buckets == 0 {
        return Vec::new();
    }
    bucket_ranges(samples.len(), buckets)
        .map(|(start, end)| {
            let slice = &samples[start..end];
            let mut lo = slice[0];
            let mut hi = slice[0];
            for &s in &slice[1..] {
                if s < lo {
                    lo = s;
                }
                if s > hi {
                    hi = s;
                }
            }
            (lo, hi)
        })
        .collect()
}

/// Decimates **packed digital** samples, returning `(AND, OR)` per bucket.
///
/// A packed byte is a bitmask — bit *n* is channel D*n* — not a number, so
/// its numeric min/max is meaningless per channel. Take a bucket holding
/// `0x01` (D0 high) then `0x80` (a D7 glitch): numeric min/max yields
/// `(0x01, 0x80)`, which reads D0 as "min high, max low" — an impossible
/// state — and destroys the per-channel truth.
///
/// AND/OR is the per-bit analogue of min/max: a bit is set in AND only if
/// that channel was high for the *whole* bucket, and set in OR if it was
/// *ever* high. Per channel that gives exactly three honest outcomes:
///
/// | AND | OR | meaning |
/// |---|---|---|
/// | 0 | 0 | low throughout |
/// | 1 | 1 | high throughout |
/// | 0 | 1 | **toggled within the bucket** — a glitch is never lost |
///
/// `AND & !OR == 0` always holds, so `(1, 0)` cannot occur.
pub fn bitwise_decimate(packed: &[u8], buckets: usize) -> Vec<(u8, u8)> {
    if packed.is_empty() || buckets == 0 {
        return Vec::new();
    }
    bucket_ranges(packed.len(), buckets)
        .map(|(start, end)| {
            packed[start..end]
                .iter()
                .fold((0xFFu8, 0x00u8), |(and, or), &b| (and & b, or | b))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitwise_decimate_keeps_each_channel_honest_where_numeric_minmax_lies() {
        // D0 high, then a D7 glitch — the counterexample from the doc comment.
        let packed = [0x01u8, 0x80];
        let numeric = minmax_decimate(&packed, 1)[0];
        assert_eq!(
            numeric,
            (0x01, 0x80),
            "numeric min/max reads D0 as min=1, max=0"
        );

        let (and, or) = bitwise_decimate(&packed, 1)[0];
        assert_eq!(and, 0x00, "no channel was high for the whole bucket");
        assert_eq!(or, 0x81, "D0 and D7 were each high at some point");
        for ch in 0..8 {
            let (a, o) = ((and >> ch) & 1, (or >> ch) & 1);
            let toggled = ch == 0 || ch == 7;
            assert_eq!((a, o), if toggled { (0, 1) } else { (0, 0) }, "D{ch}");
        }
    }

    #[test]
    fn bitwise_decimate_preserves_a_one_sample_glitch_on_one_channel() {
        // D3 steady high; a single-sample D5 pulse buried in 1000 samples.
        let mut packed = vec![0x08u8; 1000];
        packed[517] = 0x08 | 0x20;
        for (and, or) in bitwise_decimate(&packed, 10) {
            assert_eq!((and >> 3) & 1, 1, "D3 high throughout every bucket");
            assert_eq!((or >> 3) & 1, 1);
        }
        let d5_seen = bitwise_decimate(&packed, 10)
            .iter()
            .filter(|(_, or)| (or >> 5) & 1 == 1)
            .count();
        assert_eq!(d5_seen, 1, "the D5 glitch survives in exactly one bucket");
    }

    #[test]
    fn bitwise_decimate_never_yields_and_outside_or() {
        let packed: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        for (and, or) in bitwise_decimate(&packed, 37) {
            assert_eq!(and & !or, 0, "AND must be a subset of OR");
        }
    }

    #[test]
    fn analog_and_digital_decimation_share_bucket_boundaries() {
        let n = 1003;
        let analog: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            minmax_decimate(&analog, 7).len(),
            bitwise_decimate(&analog, 7).len()
        );
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(minmax_decimate::<u8>(&[], 10), Vec::new());
    }

    #[test]
    fn zero_buckets_yields_empty_output() {
        assert_eq!(minmax_decimate(&[1u8, 2, 3], 0), Vec::new());
    }

    #[test]
    fn never_produces_more_buckets_than_samples() {
        let out = minmax_decimate(&[1u8, 2, 3], 100);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn preserves_a_glitch_that_naive_striding_would_alias_away() {
        // A single-sample spike to 255 hidden among otherwise-flat samples.
        // Decimating stride-N (i.e. picking every Nth sample) would very
        // likely skip straight over it; min/max never can.
        let mut samples = vec![10u8; 1000];
        samples[537] = 255;
        let out = minmax_decimate(&samples, 100);
        assert!(out.iter().any(|&(_, hi)| hi == 255));
        // And the baseline floor is preserved too.
        assert!(out.iter().any(|&(lo, _)| lo == 10));
    }

    #[test]
    fn exact_min_max_within_each_bucket() {
        let samples: Vec<i16> = vec![5, -3, 8, 0, -10, 2, 7, 1];
        let out = minmax_decimate(&samples, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], (-3, 8)); // bucket 0: [5, -3, 8, 0]
        assert_eq!(out[1], (-10, 7)); // bucket 1: [-10, 2, 7, 1]
    }

    #[test]
    fn single_bucket_covers_whole_input() {
        let samples = [3u8, 1, 4, 1, 5, 9, 2, 6];
        let out = minmax_decimate(&samples, 1);
        assert_eq!(out, vec![(1, 9)]);
    }
}
