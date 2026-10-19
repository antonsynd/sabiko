//! A small deterministic generator (splitmix64), so the crate has no dependencies and a
//! seed reproduces a program on every platform.

/// Splitmix64 state. Every method is a pure function of the state, so a seed gives the same
/// sequence everywhere.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Mix the seed so that neighbouring seeds do not share a prefix of choices.
        let mut rng = Rng {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        };
        rng.next_u64();
        rng
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0)");
        // The bias of the modulo is irrelevant for test generation.
        self.next_u64() % n
    }

    /// A value in `0..n` as a `usize`.
    pub fn index(&mut self, n: usize) -> usize {
        usize::try_from(self.below(n as u64)).expect("fits")
    }

    /// A value in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        let width = u64::try_from(i128::from(hi) - i128::from(lo) + 1).expect("positive width");
        let offset = i128::from(self.below(width));
        i64::try_from(i128::from(lo) + offset).expect("in range")
    }

    /// True with probability `percent / 100`.
    pub fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < u64::from(percent)
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.index(items.len())]
    }

    /// Picks an index with the given relative weights.
    pub fn weighted(&mut self, weights: &[u32]) -> usize {
        let total: u32 = weights.iter().sum();
        assert!(total > 0, "all weights are zero");
        let mut roll = self.below(u64::from(total));
        for (i, &w) in weights.iter().enumerate() {
            if roll < u64::from(w) {
                return i;
            }
            roll -= u64::from(w);
        }
        weights.len() - 1
    }

    /// A random bit pattern of `bits` bits.
    pub fn bits(&mut self, bits: u32) -> u64 {
        if bits >= 64 {
            self.next_u64()
        } else {
            self.next_u64() & ((1u64 << bits) - 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn deterministic_and_bounded() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
            let r = a.range(-3, 5);
            assert!((-3..=5).contains(&r));
            b.range(-3, 5);
        }
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }

    #[test]
    fn weighted_respects_zero_weights() {
        let mut rng = Rng::new(3);
        for _ in 0..200 {
            assert_eq!(rng.weighted(&[0, 5, 0]), 1);
        }
    }
}
