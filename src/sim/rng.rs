//! Tiny deterministic PRNG (SplitMix64).
//!
//! We avoid the `rand` crate here so simulation runs are byte-for-byte
//! reproducible from a single `u64` seed, with no dependency-version drift.
//! SplitMix64 is the standard seeding generator (used to seed xoshiro); it is
//! more than adequate for sampling a small discrete latency distribution.

pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `[0, n)` using Lemire's debiased multiply-shift.
    /// `n` must be non-zero.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        // Lemire: avoids modulo bias without a rejection loop in the common case.
        let mut x = self.next_u64();
        let mut m = (x as u128) * (n as u128);
        let mut l = m as u64;
        if l < n {
            let t = n.wrapping_neg() % n;
            while l < t {
                x = self.next_u64();
                m = (x as u128) * (n as u128);
                l = m as u64;
            }
        }
        (m >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_seed() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn below_in_range_and_roughly_uniform() {
        let mut r = SplitMix64::new(7);
        let n = 5u64;
        let mut counts = [0u64; 5];
        let trials = 100_000u64;
        for _ in 0..trials {
            let v = r.below(n);
            assert!(v < n);
            counts[v as usize] += 1;
        }
        // Each bucket within ~10% of trials/n.
        let expected = trials / n;
        for c in counts {
            let diff = c.abs_diff(expected);
            assert!(diff < expected / 10, "bucket {c} too far from {expected}");
        }
    }
}
