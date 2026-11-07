//! Deterministic randomness for the simulation.
//!
//! The world is reproducible from its seed: every random choice it makes —
//! the probe wiring, when a fault strikes, comparator noise — comes from a
//! [`Rng`] seeded from it, never from the OS. Per-sample noise is a hash of
//! the sample's time instead ([`noise`]), so it costs no state and two reads
//! of the same instant agree.

/// SplitMix64: small, fast, and well mixed. Not cryptographic.
pub(crate) fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A seeded stream of random numbers.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// An independent stream for one purpose, so adding a draw in one place
    /// does not shift every other random choice.
    pub fn fork(&self, purpose: u64) -> Self {
        Self(splitmix64(
            self.0 ^ purpose.wrapping_mul(0xA24B_AED4_963E_E407),
        ))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        splitmix64(self.0)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    /// `true` with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Roughly standard normal (Irwin–Hall of four uniforms, rescaled).
    pub fn normal(&mut self) -> f64 {
        let x = self.next_u64();
        irwin_hall(x)
    }

    /// Fisher–Yates.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

/// Four 16-bit uniforms from one word, summed and rescaled to mean 0,
/// standard deviation 1. Cheap enough to run per sample at scope rates.
fn irwin_hall(x: u64) -> f64 {
    let sum = (x & 0xFFFF) + ((x >> 16) & 0xFFFF) + ((x >> 32) & 0xFFFF) + (x >> 48);
    // Each uniform has mean 0.5 and variance 1/12; four of them: mean 2,
    // variance 1/3.
    let u = sum as f64 / 65_536.0;
    (u - 2.0) * 1.732_050_807_568_877_2
}

/// Reproducible noise for `(seed, channel, time)`: standard normal.
pub(crate) fn noise(seed: u64, channel: u8, t_ns: u64) -> f64 {
    irwin_hall(splitmix64(
        seed ^ (u64::from(channel) << 56) ^ t_ns.wrapping_mul(0x9E37_79B9_7F4A_7C15),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn normal_has_roughly_unit_spread() {
        let mut rng = Rng::new(1);
        let n = 20_000;
        let xs: Vec<f64> = (0..n).map(|_| rng.normal()).collect();
        let mean = xs.iter().sum::<f64>() / n as f64;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.05, "{mean}");
        assert!((var - 1.0).abs() < 0.1, "{var}");
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut rng = Rng::new(3);
        let mut v: Vec<u8> = (0..8).collect();
        rng.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(sorted, (0..8).collect::<Vec<u8>>());
    }
}
