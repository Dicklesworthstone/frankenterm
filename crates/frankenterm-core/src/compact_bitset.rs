//! Fixed-size bitset backed by `Vec<u64>` words.
//!
//! Provides efficient set operations using word-level bitwise ops.
//! Useful for tracking pane activity masks, feature flags, resource
//! allocation bitmaps, and compact integer set membership.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Number of bits per storage word.
const BITS_PER_WORD: usize = 64;

/// A fixed-capacity bitset backed by `Vec<u64>`.
///
/// Bit indices run from `0` to `capacity() - 1`.  Operations on indices
/// beyond capacity panic in debug builds and are masked in release.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompactBitset {
    words: Vec<u64>,
    capacity: usize,
}

impl CompactBitset {
    /// Create an all-zeros bitset with room for `capacity` bits.
    ///
    /// # Panics
    /// Panics when `capacity == 0`.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "bitset capacity must be > 0");
        let n_words = (capacity + BITS_PER_WORD - 1) / BITS_PER_WORD;
        Self {
            words: vec![0u64; n_words],
            capacity,
        }
    }

    /// Create an all-ones bitset with room for `capacity` bits.
    #[must_use]
    pub fn full(capacity: usize) -> Self {
        let mut bs = Self::new(capacity);
        for w in &mut bs.words {
            *w = u64::MAX;
        }
        bs.mask_tail();
        bs
    }

    /// Maximum number of bits this bitset can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    // ---- single-bit operations ----

    /// Set bit at `index` to 1.
    ///
    /// # Panics
    /// Panics if `index >= capacity()`.
    pub fn set(&mut self, index: usize) {
        self.bounds_check(index);
        let (word, bit) = Self::pos(index);
        self.words[word] |= 1u64 << bit;
    }

    /// Clear bit at `index` to 0.
    ///
    /// # Panics
    /// Panics if `index >= capacity()`.
    pub fn clear(&mut self, index: usize) {
        self.bounds_check(index);
        let (word, bit) = Self::pos(index);
        self.words[word] &= !(1u64 << bit);
    }

    /// Toggle bit at `index`.
    ///
    /// # Panics
    /// Panics if `index >= capacity()`.
    pub fn toggle(&mut self, index: usize) {
        self.bounds_check(index);
        let (word, bit) = Self::pos(index);
        self.words[word] ^= 1u64 << bit;
    }

    /// Test whether bit at `index` is set.
    ///
    /// # Panics
    /// Panics if `index >= capacity()`.
    #[must_use]
    pub fn test(&self, index: usize) -> bool {
        self.bounds_check(index);
        let (word, bit) = Self::pos(index);
        (self.words[word] >> bit) & 1 == 1
    }

    /// Set bit and return whether it was previously set.
    pub fn test_and_set(&mut self, index: usize) -> bool {
        let was = self.test(index);
        self.set(index);
        was
    }

    /// Clear bit and return whether it was previously set.
    pub fn test_and_clear(&mut self, index: usize) -> bool {
        let was = self.test(index);
        self.clear(index);
        was
    }

    // ---- range operations ----

    /// Set all bits in the closed range `[lo, hi]`.
    ///
    /// # Panics
    /// Panics if `lo > hi` or `hi >= capacity()`.
    pub fn set_range(&mut self, lo: usize, hi: usize) {
        assert!(lo <= hi, "lo must be <= hi");
        self.bounds_check(hi);
        for i in lo..=hi {
            let (word, bit) = Self::pos(i);
            self.words[word] |= 1u64 << bit;
        }
    }

    /// Clear all bits in the closed range `[lo, hi]`.
    pub fn clear_range(&mut self, lo: usize, hi: usize) {
        assert!(lo <= hi, "lo must be <= hi");
        self.bounds_check(hi);
        for i in lo..=hi {
            let (word, bit) = Self::pos(i);
            self.words[word] &= !(1u64 << bit);
        }
    }

    // ---- counting ----

    /// Number of set bits (popcount).
    #[must_use]
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Number of clear bits.
    #[must_use]
    pub fn count_zeros(&self) -> usize {
        self.capacity - self.count_ones()
    }

    /// True if no bits are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// True if all bits within capacity are set.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.count_ones() == self.capacity
    }

    // ---- searching ----

    /// Index of the first set bit, or `None` if empty.
    #[must_use]
    pub fn first_set(&self) -> Option<usize> {
        for (i, &w) in self.words.iter().enumerate() {
            if w != 0 {
                let bit = w.trailing_zeros() as usize;
                let index = i * BITS_PER_WORD + bit;
                if index < self.capacity {
                    return Some(index);
                }
            }
        }
        None
    }

    /// Index of the first clear bit, or `None` if full.
    #[must_use]
    pub fn first_clear(&self) -> Option<usize> {
        for (i, &w) in self.words.iter().enumerate() {
            if w != u64::MAX {
                let bit = (!w).trailing_zeros() as usize;
                let index = i * BITS_PER_WORD + bit;
                if index < self.capacity {
                    return Some(index);
                }
            }
        }
        None
    }

    /// Index of the last set bit, or `None` if empty.
    #[must_use]
    pub fn last_set(&self) -> Option<usize> {
        for (i, &w) in self.words.iter().enumerate().rev() {
            if w != 0 {
                let bit = BITS_PER_WORD - 1 - w.leading_zeros() as usize;
                let index = i * BITS_PER_WORD + bit;
                if index < self.capacity {
                    return Some(index);
                }
            }
        }
        None
    }

    // ---- bulk operations ----

    /// Clear all bits to zero.
    pub fn clear_all(&mut self) {
        for w in &mut self.words {
            *w = 0;
        }
    }

    /// Set all bits within capacity to one.
    pub fn set_all(&mut self) {
        for w in &mut self.words {
            *w = u64::MAX;
        }
        self.mask_tail();
    }

    /// Bitwise NOT (complement within capacity).
    #[must_use]
    pub fn complement(&self) -> Self {
        let mut result = self.clone();
        for w in &mut result.words {
            *w = !*w;
        }
        result.mask_tail();
        result
    }

    // ---- set operations (produce new bitset) ----

    /// Union (OR) of two bitsets. Both must have the same capacity.
    ///
    /// # Panics
    /// Panics if capacities differ.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        self.check_compatible(other);
        let mut result = self.clone();
        for (w, &o) in result.words.iter_mut().zip(other.words.iter()) {
            *w |= o;
        }
        result
    }

    /// Intersection (AND) of two bitsets. Both must have the same capacity.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        self.check_compatible(other);
        let mut result = self.clone();
        for (w, &o) in result.words.iter_mut().zip(other.words.iter()) {
            *w &= o;
        }
        result
    }

    /// Difference (AND NOT) — bits in `self` but not in `other`.
    #[must_use]
    pub fn difference(&self, other: &Self) -> Self {
        self.check_compatible(other);
        let mut result = self.clone();
        for (w, &o) in result.words.iter_mut().zip(other.words.iter()) {
            *w &= !o;
        }
        result
    }

    /// Symmetric difference (XOR).
    #[must_use]
    pub fn symmetric_difference(&self, other: &Self) -> Self {
        self.check_compatible(other);
        let mut result = self.clone();
        for (w, &o) in result.words.iter_mut().zip(other.words.iter()) {
            *w ^= o;
        }
        result.mask_tail();
        result
    }

    // ---- in-place set operations ----

    /// In-place union.
    pub fn union_with(&mut self, other: &Self) {
        self.check_compatible(other);
        for (w, &o) in self.words.iter_mut().zip(other.words.iter()) {
            *w |= o;
        }
    }

    /// In-place intersection.
    pub fn intersection_with(&mut self, other: &Self) {
        self.check_compatible(other);
        for (w, &o) in self.words.iter_mut().zip(other.words.iter()) {
            *w &= o;
        }
    }

    /// In-place difference.
    pub fn difference_with(&mut self, other: &Self) {
        self.check_compatible(other);
        for (w, &o) in self.words.iter_mut().zip(other.words.iter()) {
            *w &= !o;
        }
    }

    /// In-place symmetric difference.
    pub fn symmetric_difference_with(&mut self, other: &Self) {
        self.check_compatible(other);
        for (w, &o) in self.words.iter_mut().zip(other.words.iter()) {
            *w ^= o;
        }
        self.mask_tail();
    }

    // ---- subset/superset ----

    /// True if every set bit in `self` is also set in `other`.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.check_compatible(other);
        self.words
            .iter()
            .zip(other.words.iter())
            .all(|(&a, &b)| a & !b == 0)
    }

    /// True if every set bit in `other` is also set in `self`.
    #[must_use]
    pub fn is_superset_of(&self, other: &Self) -> bool {
        other.is_subset_of(self)
    }

    /// True if the two bitsets share no set bits.
    #[must_use]
    pub fn is_disjoint(&self, other: &Self) -> bool {
        self.check_compatible(other);
        self.words
            .iter()
            .zip(other.words.iter())
            .all(|(&a, &b)| a & b == 0)
    }

    // ---- iteration ----

    /// Iterate over indices of set bits in ascending order.
    pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
        let cap = self.capacity;
        self.words.iter().enumerate().flat_map(move |(wi, &word)| {
            let base = wi * BITS_PER_WORD;
            BitIter::new(word).map(move |bit| base + bit)
        }).take_while(move |&idx| idx < cap)
    }

    /// Iterate over indices of clear bits in ascending order.
    pub fn iter_zeros(&self) -> impl Iterator<Item = usize> + '_ {
        let cap = self.capacity;
        (0..cap).filter(move |&i| !self.test(i))
    }

    /// Collect set-bit indices into a `Vec`.
    #[must_use]
    pub fn to_vec(&self) -> Vec<usize> {
        self.iter_ones().collect()
    }

    /// Build a bitset from an iterator of indices.
    ///
    /// # Panics
    /// Panics if any index is >= `capacity`.
    pub fn from_indices(capacity: usize, indices: impl IntoIterator<Item = usize>) -> Self {
        let mut bs = Self::new(capacity);
        for i in indices {
            bs.set(i);
        }
        bs
    }

    // ---- internal helpers ----

    #[inline]
    fn pos(index: usize) -> (usize, usize) {
        (index / BITS_PER_WORD, index % BITS_PER_WORD)
    }

    #[inline]
    fn bounds_check(&self, index: usize) {
        assert!(
            index < self.capacity,
            "bitset index {index} out of bounds (capacity {})",
            self.capacity
        );
    }

    fn check_compatible(&self, other: &Self) {
        assert_eq!(
            self.capacity, other.capacity,
            "bitset capacities must match ({} vs {})",
            self.capacity, other.capacity
        );
    }

    /// Zero out any bits beyond `capacity` in the last word.
    fn mask_tail(&mut self) {
        let tail_bits = self.capacity % BITS_PER_WORD;
        if tail_bits != 0 {
            if let Some(last) = self.words.last_mut() {
                *last &= (1u64 << tail_bits) - 1;
            }
        }
    }
}

impl fmt::Debug for CompactBitset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CompactBitset({}/{})", self.count_ones(), self.capacity)
    }
}

impl fmt::Display for CompactBitset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        let mut first = true;
        for idx in self.iter_ones() {
            if !first {
                write!(f, ", ")?;
            }
            write!(f, "{idx}")?;
            first = false;
        }
        write!(f, "}}")
    }
}

/// Iterates over set bits within a single `u64` word.
struct BitIter {
    remaining: u64,
}

impl BitIter {
    fn new(word: u64) -> Self {
        Self { remaining: word }
    }
}

impl Iterator for BitIter {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.remaining == 0 {
            return None;
        }
        let bit = self.remaining.trailing_zeros() as usize;
        self.remaining &= self.remaining - 1; // clear lowest set bit
        Some(bit)
    }
}

// ---- BitOps trait impls ----

impl std::ops::BitOr for &CompactBitset {
    type Output = CompactBitset;
    fn bitor(self, rhs: Self) -> CompactBitset {
        self.union(rhs)
    }
}

impl std::ops::BitAnd for &CompactBitset {
    type Output = CompactBitset;
    fn bitand(self, rhs: Self) -> CompactBitset {
        self.intersection(rhs)
    }
}

impl std::ops::BitXor for &CompactBitset {
    type Output = CompactBitset;
    fn bitxor(self, rhs: Self) -> CompactBitset {
        self.symmetric_difference(rhs)
    }
}

impl std::ops::Not for &CompactBitset {
    type Output = CompactBitset;
    fn not(self) -> CompactBitset {
        self.complement()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_empty() {
        let bs = CompactBitset::new(100);
        assert_eq!(bs.capacity(), 100);
        assert!(bs.is_empty());
        assert_eq!(bs.count_ones(), 0);
        assert_eq!(bs.count_zeros(), 100);
    }

    #[test]
    fn full_creates_all_set() {
        let bs = CompactBitset::full(100);
        assert!(bs.is_full());
        assert_eq!(bs.count_ones(), 100);
        assert_eq!(bs.count_zeros(), 0);
    }

    #[test]
    fn set_and_test() {
        let mut bs = CompactBitset::new(128);
        bs.set(0);
        bs.set(63);
        bs.set(64);
        bs.set(127);
        assert!(bs.test(0));
        assert!(bs.test(63));
        assert!(bs.test(64));
        assert!(bs.test(127));
        assert!(!bs.test(1));
        assert!(!bs.test(65));
    }

    #[test]
    fn clear_bit() {
        let mut bs = CompactBitset::new(10);
        bs.set(5);
        assert!(bs.test(5));
        bs.clear(5);
        assert!(!bs.test(5));
    }

    #[test]
    fn toggle() {
        let mut bs = CompactBitset::new(10);
        assert!(!bs.test(3));
        bs.toggle(3);
        assert!(bs.test(3));
        bs.toggle(3);
        assert!(!bs.test(3));
    }

    #[test]
    fn test_and_set_returns_previous() {
        let mut bs = CompactBitset::new(10);
        assert!(!bs.test_and_set(5));
        assert!(bs.test_and_set(5));
    }

    #[test]
    fn test_and_clear_returns_previous() {
        let mut bs = CompactBitset::new(10);
        bs.set(5);
        assert!(bs.test_and_clear(5));
        assert!(!bs.test_and_clear(5));
    }

    #[test]
    fn set_range() {
        let mut bs = CompactBitset::new(100);
        bs.set_range(10, 20);
        for i in 0..100 {
            assert_eq!(bs.test(i), (10..=20).contains(&i), "bit {i}");
        }
    }

    #[test]
    fn clear_range() {
        let mut bs = CompactBitset::full(100);
        bs.clear_range(10, 20);
        for i in 0..100 {
            assert_eq!(bs.test(i), !(10..=20).contains(&i), "bit {i}");
        }
    }

    #[test]
    fn count_ones_and_zeros() {
        let mut bs = CompactBitset::new(100);
        bs.set(0);
        bs.set(50);
        bs.set(99);
        assert_eq!(bs.count_ones(), 3);
        assert_eq!(bs.count_zeros(), 97);
    }

    #[test]
    fn is_empty_and_full() {
        let mut bs = CompactBitset::new(64);
        assert!(bs.is_empty());
        assert!(!bs.is_full());

        bs.set_all();
        assert!(!bs.is_empty());
        assert!(bs.is_full());

        bs.clear(0);
        assert!(!bs.is_full());
    }

    #[test]
    fn first_set() {
        let bs = CompactBitset::new(100);
        assert_eq!(bs.first_set(), None);

        let mut bs2 = CompactBitset::new(100);
        bs2.set(42);
        assert_eq!(bs2.first_set(), Some(42));

        bs2.set(10);
        assert_eq!(bs2.first_set(), Some(10));
    }

    #[test]
    fn first_clear() {
        let bs = CompactBitset::full(100);
        assert_eq!(bs.first_clear(), None);

        let mut bs2 = CompactBitset::full(100);
        bs2.clear(42);
        assert_eq!(bs2.first_clear(), Some(42));
    }

    #[test]
    fn last_set() {
        let bs = CompactBitset::new(100);
        assert_eq!(bs.last_set(), None);

        let mut bs2 = CompactBitset::new(100);
        bs2.set(10);
        bs2.set(90);
        assert_eq!(bs2.last_set(), Some(90));
    }

    #[test]
    fn clear_all_and_set_all() {
        let mut bs = CompactBitset::new(100);
        bs.set(5);
        bs.set(95);
        bs.clear_all();
        assert!(bs.is_empty());

        bs.set_all();
        assert!(bs.is_full());
        assert_eq!(bs.count_ones(), 100);
    }

    #[test]
    fn complement() {
        let mut bs = CompactBitset::new(10);
        bs.set(0);
        bs.set(5);
        bs.set(9);
        let comp = bs.complement();
        assert_eq!(comp.count_ones(), 7);
        assert!(!comp.test(0));
        assert!(comp.test(1));
        assert!(!comp.test(5));
        assert!(comp.test(8));
        assert!(!comp.test(9));
    }

    #[test]
    fn union() {
        let a = CompactBitset::from_indices(10, [0, 2, 4]);
        let b = CompactBitset::from_indices(10, [1, 2, 3]);
        let u = a.union(&b);
        assert_eq!(u.to_vec(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn intersection() {
        let a = CompactBitset::from_indices(10, [0, 2, 4]);
        let b = CompactBitset::from_indices(10, [1, 2, 3]);
        let i = a.intersection(&b);
        assert_eq!(i.to_vec(), vec![2]);
    }

    #[test]
    fn difference() {
        let a = CompactBitset::from_indices(10, [0, 2, 4]);
        let b = CompactBitset::from_indices(10, [1, 2, 3]);
        let d = a.difference(&b);
        assert_eq!(d.to_vec(), vec![0, 4]);
    }

    #[test]
    fn symmetric_difference() {
        let a = CompactBitset::from_indices(10, [0, 2, 4]);
        let b = CompactBitset::from_indices(10, [1, 2, 3]);
        let sd = a.symmetric_difference(&b);
        assert_eq!(sd.to_vec(), vec![0, 1, 3, 4]);
    }

    #[test]
    fn in_place_union() {
        let mut a = CompactBitset::from_indices(10, [0, 2]);
        let b = CompactBitset::from_indices(10, [1, 3]);
        a.union_with(&b);
        assert_eq!(a.to_vec(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn in_place_intersection() {
        let mut a = CompactBitset::from_indices(10, [0, 1, 2]);
        let b = CompactBitset::from_indices(10, [1, 2, 3]);
        a.intersection_with(&b);
        assert_eq!(a.to_vec(), vec![1, 2]);
    }

    #[test]
    fn in_place_difference() {
        let mut a = CompactBitset::from_indices(10, [0, 1, 2]);
        let b = CompactBitset::from_indices(10, [1]);
        a.difference_with(&b);
        assert_eq!(a.to_vec(), vec![0, 2]);
    }

    #[test]
    fn in_place_symmetric_difference() {
        let mut a = CompactBitset::from_indices(10, [0, 1]);
        let b = CompactBitset::from_indices(10, [1, 2]);
        a.symmetric_difference_with(&b);
        assert_eq!(a.to_vec(), vec![0, 2]);
    }

    #[test]
    fn subset_superset() {
        let a = CompactBitset::from_indices(10, [1, 3]);
        let b = CompactBitset::from_indices(10, [0, 1, 2, 3, 4]);
        assert!(a.is_subset_of(&b));
        assert!(!b.is_subset_of(&a));
        assert!(b.is_superset_of(&a));
    }

    #[test]
    fn is_disjoint() {
        let a = CompactBitset::from_indices(10, [0, 2, 4]);
        let b = CompactBitset::from_indices(10, [1, 3, 5]);
        assert!(a.is_disjoint(&b));

        let c = CompactBitset::from_indices(10, [2, 6]);
        assert!(!a.is_disjoint(&c));
    }

    #[test]
    fn iter_ones() {
        let bs = CompactBitset::from_indices(200, [0, 63, 64, 127, 128, 199]);
        let ones: Vec<usize> = bs.iter_ones().collect();
        assert_eq!(ones, vec![0, 63, 64, 127, 128, 199]);
    }

    #[test]
    fn iter_zeros() {
        let mut bs = CompactBitset::full(5);
        bs.clear(1);
        bs.clear(3);
        let zeros: Vec<usize> = bs.iter_zeros().collect();
        assert_eq!(zeros, vec![1, 3]);
    }

    #[test]
    fn to_vec_roundtrip() {
        let indices = vec![3, 7, 15, 31, 63, 64, 99];
        let bs = CompactBitset::from_indices(100, indices.iter().copied());
        assert_eq!(bs.to_vec(), indices);
    }

    #[test]
    fn serde_roundtrip() {
        let bs = CompactBitset::from_indices(100, [0, 50, 99]);
        let json = serde_json::to_string(&bs).unwrap();
        let back: CompactBitset = serde_json::from_str(&json).unwrap();
        assert_eq!(bs, back);
    }

    #[test]
    fn debug_format() {
        let bs = CompactBitset::from_indices(100, [1, 2, 3]);
        let dbg = format!("{bs:?}");
        assert!(dbg.contains("CompactBitset(3/100)"));
    }

    #[test]
    fn display_format() {
        let bs = CompactBitset::from_indices(10, [1, 5, 9]);
        assert_eq!(format!("{bs}"), "{1, 5, 9}");
    }

    #[test]
    fn display_empty() {
        let bs = CompactBitset::new(10);
        assert_eq!(format!("{bs}"), "{}");
    }

    #[test]
    fn operator_bitor() {
        let a = CompactBitset::from_indices(10, [0, 2]);
        let b = CompactBitset::from_indices(10, [1, 3]);
        let c = &a | &b;
        assert_eq!(c.to_vec(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn operator_bitand() {
        let a = CompactBitset::from_indices(10, [0, 1, 2]);
        let b = CompactBitset::from_indices(10, [1, 2, 3]);
        let c = &a & &b;
        assert_eq!(c.to_vec(), vec![1, 2]);
    }

    #[test]
    fn operator_bitxor() {
        let a = CompactBitset::from_indices(10, [0, 1]);
        let b = CompactBitset::from_indices(10, [1, 2]);
        let c = &a ^ &b;
        assert_eq!(c.to_vec(), vec![0, 2]);
    }

    #[test]
    fn operator_not() {
        let a = CompactBitset::from_indices(4, [0, 3]);
        let b = !&a;
        assert_eq!(b.to_vec(), vec![1, 2]);
    }

    #[test]
    #[should_panic(expected = "bitset capacity must be > 0")]
    fn zero_capacity_panics() {
        let _ = CompactBitset::new(0);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn out_of_bounds_set_panics() {
        let mut bs = CompactBitset::new(10);
        bs.set(10);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn out_of_bounds_test_panics() {
        let bs = CompactBitset::new(10);
        let _ = bs.test(100);
    }

    #[test]
    #[should_panic(expected = "capacities must match")]
    fn mismatched_capacity_panics() {
        let a = CompactBitset::new(10);
        let b = CompactBitset::new(20);
        let _ = a.union(&b);
    }

    #[test]
    fn non_power_of_two_capacity() {
        let mut bs = CompactBitset::new(65);
        bs.set(64);
        assert!(bs.test(64));
        assert_eq!(bs.count_ones(), 1);

        let full = CompactBitset::full(65);
        assert_eq!(full.count_ones(), 65);
    }

    #[test]
    fn capacity_1() {
        let mut bs = CompactBitset::new(1);
        assert!(bs.is_empty());
        bs.set(0);
        assert!(bs.is_full());
        assert_eq!(bs.count_ones(), 1);
    }

    #[test]
    fn capacity_64_boundary() {
        let bs = CompactBitset::full(64);
        assert_eq!(bs.count_ones(), 64);
        assert!(bs.is_full());

        let mut bs2 = CompactBitset::new(64);
        bs2.set(63);
        assert_eq!(bs2.first_set(), Some(63));
        assert_eq!(bs2.last_set(), Some(63));
    }

    #[test]
    fn clone_equality() {
        let bs = CompactBitset::from_indices(50, [0, 10, 49]);
        let cloned = bs.clone();
        assert_eq!(bs, cloned);
    }

    #[test]
    fn hash_consistency() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let a = CompactBitset::from_indices(10, [1, 5]);
        let b = CompactBitset::from_indices(10, [1, 5]);

        let mut h1 = DefaultHasher::new();
        a.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        b.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn empty_set_is_subset_of_everything() {
        let empty = CompactBitset::new(10);
        let any = CompactBitset::from_indices(10, [3, 7]);
        assert!(empty.is_subset_of(&any));
        assert!(empty.is_subset_of(&empty));
    }

    #[test]
    fn full_set_is_superset_of_everything() {
        let full = CompactBitset::full(10);
        let any = CompactBitset::from_indices(10, [3, 7]);
        assert!(full.is_superset_of(&any));
        assert!(full.is_superset_of(&full));
    }

    #[test]
    fn complement_of_complement_is_identity() {
        let bs = CompactBitset::from_indices(100, [0, 33, 66, 99]);
        let double_comp = bs.complement().complement();
        assert_eq!(bs, double_comp);
    }

    #[test]
    fn from_indices_deduplicates() {
        let bs = CompactBitset::from_indices(10, [1, 1, 1, 5, 5]);
        assert_eq!(bs.count_ones(), 2);
        assert_eq!(bs.to_vec(), vec![1, 5]);
    }

    #[test]
    fn large_bitset() {
        let mut bs = CompactBitset::new(10_000);
        for i in (0..10_000).step_by(3) {
            bs.set(i);
        }
        let expected_count = (10_000 + 2) / 3;
        assert_eq!(bs.count_ones(), expected_count);
        assert_eq!(bs.first_set(), Some(0));
        assert_eq!(bs.last_set(), Some(9999));
    }
}
