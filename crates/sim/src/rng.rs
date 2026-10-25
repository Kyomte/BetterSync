//! Seeded RNG for the simulator.
//!
//! A hand-rolled SplitMix64 rather than the `rand` crate: saved regression
//! seeds must replay the same run forever, and a dependency upgrade must not
//! be able to change the random stream.

#[derive(Debug, Clone)]
pub struct SimRng {
    state: u64,
}

impl SimRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`. `n` must be non-zero.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "SimRng::below(0)");
        // Lemire's multiply-shift; the tiny bias is irrelevant for a simulator.
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// Uniform index into a collection of length `len`.
    pub fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// True with probability `percent`/100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = SimRng::new(42);
        let mut b = SimRng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        // Pin the stream so an accidental change to the generator is caught.
        assert_eq!(SimRng::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = SimRng::new(7);
        for n in 1..50 {
            assert!(rng.below(n) < n);
        }
    }
}
