//! Shared chunking-invariance harness used by every decoder's tests
//! (SPEC §6 acceptance: "decoders tested against synthesised waveforms incl.
//! chunk boundaries at every position").
//!
//! A streaming decoder's whole contract is that delivery shape must not
//! matter: the same transitions and the same final `valid_until` must
//! produce the same annotations whether they arrive in one `feed` call or
//! many, and regardless of how far `valid_until` lags behind the
//! transitions already handed over. This module runs a decoder both ways
//! and diffs the result.

use crate::{Annotation, Decoder, Transition};

/// Feed `transitions` to a fresh decoder according to `plan` — a sequence
/// of `(chunk_len, valid_until)` pairs consuming `transitions` in order —
/// then `finish(end_sample)`, and assert the result is identical to a
/// single one-shot `feed(transitions, end_sample)` + `finish`.
pub(crate) fn assert_same_result<D: Decoder>(
    make: &impl Fn() -> D,
    transitions: &[Transition],
    end_sample: u64,
    plan: &[(usize, u64)],
) {
    let mut baseline = make();
    let mut expected = baseline.feed(transitions, end_sample);
    expected.extend(baseline.finish(end_sample));

    let mut decoder = make();
    let mut actual: Vec<Annotation> = Vec::new();
    let mut idx = 0usize;
    for &(len, valid_until) in plan {
        let end = (idx + len).min(transitions.len());
        actual.extend(decoder.feed(&transitions[idx..end], valid_until));
        idx = end;
    }
    // Whatever the plan didn't cover (e.g. it undershot transitions.len())
    // still needs to reach the decoder for a fair comparison.
    if idx < transitions.len() {
        actual.extend(decoder.feed(&transitions[idx..], end_sample));
    }
    actual.extend(decoder.finish(end_sample));

    assert_eq!(
        actual, expected,
        "chunked decode diverged from one-shot decode for plan {plan:?}"
    );
}

/// Exhaustively checks every two-way split point, per SPEC's acceptance
/// wording literally ("at every possible position").
pub(crate) fn assert_all_split_points<D: Decoder>(
    make: &impl Fn() -> D,
    transitions: &[Transition],
    end_sample: u64,
) {
    for k in 0..=transitions.len() {
        let valid_until_first = if k == 0 {
            0
        } else {
            transitions[k - 1].sample_index
        };
        assert_same_result(
            make,
            transitions,
            end_sample,
            &[(k, valid_until_first), (transitions.len() - k, end_sample)],
        );
    }
}

/// Turns a set of raw split indices (arbitrary, unsorted, possibly out of
/// range or duplicated — exactly what a property-test strategy hands back)
/// into a valid, monotonic delivery plan: chunk boundaries from the cuts,
/// and for each non-final boundary a `valid_until` that is at least the
/// last transition it revealed but may lag behind by up to `slack` before
/// the final chunk snaps it to `end_sample`. This is what makes the
/// property test explore "valid_until advancing in arbitrary steps"
/// (SPEC's acceptance criterion) rather than only the transition-splitting
/// exercised by [`assert_all_split_points`].
pub(crate) fn build_plan(
    transitions: &[Transition],
    end_sample: u64,
    cuts: &[usize],
    slacks: &[u64],
) -> Vec<(usize, u64)> {
    let mut boundaries: Vec<usize> = cuts
        .iter()
        .copied()
        .filter(|&c| c > 0 && c < transitions.len())
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries.push(transitions.len());

    let mut plan = Vec::with_capacity(boundaries.len());
    let mut prev_idx = 0usize;
    let mut prev_valid = 0u64;
    let last = boundaries.len() - 1;
    for (i, &b) in boundaries.iter().enumerate() {
        let chunk_len = b - prev_idx;
        let min_valid = if b > 0 {
            transitions[b - 1].sample_index
        } else {
            0
        };
        // valid_until must never reach or pass the sample_index of a
        // transition this chunk didn't include — that would tell the
        // decoder input is known past a point where it's actually still
        // withholding an edge, violating `feed`'s contract.
        let cap = if b < transitions.len() {
            transitions[b].sample_index.saturating_sub(1)
        } else {
            end_sample
        };
        let valid_until = if i == last {
            end_sample
        } else {
            let slack = slacks.get(i).copied().unwrap_or(0);
            (prev_valid.max(min_valid) + slack).min(cap)
        }
        .max(prev_valid)
        .max(min_valid);
        plan.push((chunk_len, valid_until));
        prev_valid = valid_until;
        prev_idx = b;
    }
    plan
}
