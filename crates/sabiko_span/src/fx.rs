//! The compiler's hash maps and sets (decision 98): std's, with rustc's `FxHasher`
//! instead of `SipHash`. Their keys are the compiler's own small values (`DefId`, `NodeId`,
//! `LocalId`, types and names), so a fast hash matters and resistance to collisions chosen
//! by an attacker does not, as in rustc. `SipHash` on `DefId` and `NodeId` keys was a fifth
//! of a hello-world's compilation (#153). `clippy.toml` disallows std's `HashMap` and
//! `HashSet`, so every map uses these.
//!
//! A release build hashes the same way in every run. A debug build (the tests') starts
//! each thread's hashes from a random seed, as std's `RandomState` does, so iteration
//! order still differs between two compilations (each runs on a thread of its own) and a
//! result that depends on it shows up as a difference between runs (`no_panic.rs` compares
//! two).

use std::hash::{BuildHasher, Hasher};

/// A [`std::collections::HashMap`] hashed with [`FxHasher`]; make one with
/// `HashMap::default()` (`new` is only for std's own hasher).
#[allow(clippy::disallowed_types)]
pub type HashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;

/// A [`std::collections::HashSet`] hashed with [`FxHasher`]; make one with
/// `HashSet::default()`.
#[allow(clippy::disallowed_types)]
pub type HashSet<T> = std::collections::HashSet<T, FxBuildHasher>;

/// Builds [`FxHasher`]s, all from the seed it was made with: 0 in a release build, the
/// thread's random seed in a debug build (see the module docs).
#[derive(Clone, Copy, Debug)]
pub struct FxBuildHasher {
    seed: u64,
}

impl Default for FxBuildHasher {
    #[inline]
    fn default() -> Self {
        Self { seed: seed() }
    }
}

impl BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;

    #[inline]
    fn build_hasher(&self) -> FxHasher {
        FxHasher { hash: self.seed }
    }
}

#[cfg(not(debug_assertions))]
#[inline]
fn seed() -> u64 {
    0
}

#[cfg(debug_assertions)]
fn seed() -> u64 {
    thread_local! {
        static SEED: u64 = std::hash::RandomState::new().build_hasher().finish();
    }
    SEED.with(|s| *s)
}

/// rustc's `FxHasher` (from Firefox): each word is mixed into the state with a rotate, an
/// xor and a multiplication. Not a cryptographic or collision-resistant hash.
#[derive(Clone, Copy, Debug, Default)]
pub struct FxHasher {
    hash: u64,
}

/// The multiplier of rustc's 64-bit `FxHasher`.
const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(K);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use std::hash::Hash;

    use super::*;

    fn hash<T: Hash>(value: &T) -> u64 {
        FxBuildHasher::default().hash_one(value)
    }

    #[test]
    fn equal_values_hash_equally() {
        assert_eq!(hash(&(1u32, "ab")), hash(&(1u32, "ab")));
        assert_ne!(hash(&1u32), hash(&2u32));
        assert_ne!(hash(&"abcdefghi"), hash(&"abcdefghj"));
        // rustc's function from a zero seed.
        let mut h = FxHasher::default();
        1u32.hash(&mut h);
        assert_eq!(h.finish(), K);
    }

    /// Each thread (each compilation) has its own seed in a debug build, as each
    /// `RandomState` has, and one seed for all its maps.
    #[test]
    fn threads_hash_from_their_own_seeds() {
        let mine = (seed(), seed());
        assert_eq!(mine.0, mine.1);
        let others: Vec<u64> = (0..4)
            .map(|_| std::thread::spawn(seed).join().unwrap())
            .collect();
        if cfg!(debug_assertions) {
            assert!(others.iter().any(|&s| s != mine.0));
        } else {
            assert!(others.iter().all(|&s| s == 0));
        }
    }

    #[test]
    fn maps_and_sets_work() {
        let mut map: HashMap<u32, &str> = HashMap::default();
        for i in 0..1000 {
            map.insert(i, "x");
        }
        assert_eq!(map.len(), 1000);
        assert!((0..1000).all(|i| map.contains_key(&i)));
        let set: HashSet<String> = ["a", "b", "a"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(set.len(), 2);
    }
}
