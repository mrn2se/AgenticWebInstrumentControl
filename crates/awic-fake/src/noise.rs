//! Deterministic pseudo-random noise, keyed by absolute sample index.
//!
//! Every generated sample in this crate must be reproducible from a pure
//! function of `(seed, channel, sample_index)` — the trigger resolver
//! ([`crate::trigger::resolve_trigger_shift`]) evaluates the waveform at
//! arbitrary, possibly negative, sample indices *before* the real streaming
//! pass evaluates the very same indices again, and the two must agree bit
//! for bit. A conventional stateful RNG (draw-and-advance) can't do that
//! without either replaying its whole history or being reseeded per-sample
//! (too slow at MS/s rates), so noise here is a hash instead of a stream.

/// SplitMix64 — small, fast, and good enough avalanche for synthetic test
/// signals (not cryptographic).
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Uniform value in `[0, 1)` derived from `seed`.
fn uniform01(seed: u64) -> f64 {
    // Top 53 bits -> exactly representable in an f64 mantissa.
    (splitmix64(seed) >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// A reproducible standard-normal sample (mean 0, stddev 1) for
/// `(seed, channel, index)`, via Box-Muller. Callers scale by their desired
/// stddev in volts (or whatever unit).
pub(crate) fn gaussian(seed: u64, channel: u8, index: i64) -> f64 {
    let base = seed ^ ((channel as u64) << 48) ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let u1 = uniform01(base).max(1e-12);
    let u2 = uniform01(base ^ 0xD1B5_4A32_D192_ED03);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_is_pure_and_reproducible() {
        assert_eq!(gaussian(1, 0, 42), gaussian(1, 0, 42));
        assert_ne!(gaussian(1, 0, 42), gaussian(1, 0, 43));
        assert_ne!(gaussian(1, 0, 42), gaussian(1, 1, 42));
    }

    #[test]
    fn gaussian_handles_negative_indices() {
        // Must not panic on the arbitrary negative indices the trigger
        // resolver's pre-roll can produce.
        let _ = gaussian(7, 3, -1_000_000);
    }

    #[test]
    fn gaussian_distribution_is_roughly_standard_normal() {
        let n = 20_000;
        let sum: f64 = (0..n).map(|i| gaussian(99, 0, i)).sum();
        let mean = sum / n as f64;
        assert!(mean.abs() < 0.05, "mean drifted too far from 0: {mean}");
    }
}
