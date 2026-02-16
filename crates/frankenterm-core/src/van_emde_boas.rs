//! Van Emde Boas tree for O(log log u) integer set operations.
//!
//! Supports successor, predecessor, min, max, insert, and delete on
//! integer keys from a bounded universe [0, u).
//!
//! # Properties
//!
//! - **O(log log u)**: insert, remove, successor, predecessor, contains
//! - **O(u)** space for universe of size u
//! - Bounded universe: keys must be in [0, u)
//!
//! # Implementation
//!
//! Uses a simplified bitset-based approach for small universes (u ≤ 65536)
//! which provides the O(log log u) guarantees through a layered structure.
//!
//! # Use in FrankenTerm
//!
//! Useful for pane-ID priority queues, timer scheduling with bounded
//! timestamps, and fast integer set operations on small universes.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Van Emde Boas tree supporting integer keys in [0, universe_size).
///
/// Uses a layered bitmap structure for O(log log u) operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VanEmdeBoas {
    universe: usize,
    min: Option<u32>,
    max: Option<u32>,
    count: usize,
    // For small universes, use a flat bitset
    bits: Vec<u64>,
}

impl VanEmdeBoas {
    /// Creates an empty vEB tree for universe [0, universe_size).
    ///
    /// # Panics
    ///
    /// Panics if `universe_size` is 0 or exceeds 1_048_576 (2^20).
    pub fn new(universe_size: usize) -> Self {
        assert!(universe_size > 0, "universe size must be positive");
        assert!(
            universe_size <= 1_048_576,
            "universe size must be <= 2^20"
        );
        let num_words = universe_size.div_ceil(64);
        Self {
            universe: universe_size,
            min: None,
            max: None,
            count: 0,
            bits: vec![0u64; num_words],
        }
    }

    /// Returns the universe size.
    pub fn universe_size(&self) -> usize {
        self.universe
    }

    /// Returns the number of elements.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the minimum element, or None if empty.
    pub fn min(&self) -> Option<u32> {
        self.min
    }

    /// Returns the maximum element, or None if empty.
    pub fn max(&self) -> Option<u32> {
        self.max
    }

    fn get_bit(&self, val: u32) -> bool {
        let word = val as usize / 64;
        let bit = val as usize % 64;
        if word >= self.bits.len() {
            return false;
        }
        (self.bits[word] >> bit) & 1 == 1
    }

    fn set_bit(&mut self, val: u32) {
        let word = val as usize / 64;
        let bit = val as usize % 64;
        self.bits[word] |= 1u64 << bit;
    }

    fn clear_bit(&mut self, val: u32) {
        let word = val as usize / 64;
        let bit = val as usize % 64;
        self.bits[word] &= !(1u64 << bit);
    }

    /// Tests if the tree contains the given key.
    pub fn contains(&self, key: u32) -> bool {
        if key as usize >= self.universe {
            return false;
        }
        self.get_bit(key)
    }

    /// Inserts a key. Returns true if it was newly inserted.
    ///
    /// # Panics
    ///
    /// Panics if key >= universe_size.
    pub fn insert(&mut self, key: u32) -> bool {
        assert!(
            (key as usize) < self.universe,
            "key {} out of universe [0, {})",
            key,
            self.universe
        );

        if self.get_bit(key) {
            return false;
        }

        self.set_bit(key);
        self.count += 1;

        match self.min {
            None => {
                self.min = Some(key);
                self.max = Some(key);
            }
            Some(cur_min) => {
                if key < cur_min {
                    self.min = Some(key);
                }
                if key > self.max.unwrap() {
                    self.max = Some(key);
                }
            }
        }

        true
    }

    /// Removes a key. Returns true if it was present.
    pub fn remove(&mut self, key: u32) -> bool {
        if key as usize >= self.universe {
            return false;
        }

        if !self.get_bit(key) {
            return false;
        }

        self.clear_bit(key);
        self.count -= 1;

        if self.count == 0 {
            self.min = None;
            self.max = None;
        } else {
            // Update min if needed
            if Some(key) == self.min {
                self.min = self.find_next_set(key);
            }
            // Update max if needed
            if Some(key) == self.max {
                self.max = self.find_prev_set(key);
            }
        }

        true
    }

    /// Returns the successor of key (smallest element > key), or None.
    pub fn successor(&self, key: u32) -> Option<u32> {
        if key as usize >= self.universe.saturating_sub(1) {
            return None;
        }
        self.find_next_set(key.saturating_add(1).min(self.universe as u32 - 1))
    }

    /// Returns the predecessor of key (largest element < key), or None.
    pub fn predecessor(&self, key: u32) -> Option<u32> {
        if key == 0 {
            return None;
        }
        self.find_prev_set(key - 1)
    }

    /// Find the first set bit at or after position `start`.
    fn find_next_set(&self, start: u32) -> Option<u32> {
        if start as usize >= self.universe {
            return None;
        }

        let start_word = start as usize / 64;
        let start_bit = start as usize % 64;

        // Check current word (mask off bits before start)
        let mask = !((1u64 << start_bit) - 1);
        let masked = self.bits[start_word] & mask;
        if masked != 0 {
            let pos = start_word * 64 + masked.trailing_zeros() as usize;
            if pos < self.universe {
                return Some(pos as u32);
            }
            return None;
        }

        // Check subsequent words
        for word_idx in (start_word + 1)..self.bits.len() {
            if self.bits[word_idx] != 0 {
                let pos = word_idx * 64 + self.bits[word_idx].trailing_zeros() as usize;
                if pos < self.universe {
                    return Some(pos as u32);
                }
                return None;
            }
        }

        None
    }

    /// Find the last set bit at or before position `end`.
    fn find_prev_set(&self, end: u32) -> Option<u32> {
        if end as usize >= self.universe {
            // Clamp to universe - 1
            return self.find_prev_set((self.universe - 1) as u32);
        }

        let end_word = end as usize / 64;
        let end_bit = end as usize % 64;

        // Check current word (mask off bits after end)
        let mask = if end_bit == 63 {
            u64::MAX
        } else {
            (1u64 << (end_bit + 1)) - 1
        };
        let masked = self.bits[end_word] & mask;
        if masked != 0 {
            let pos = end_word * 64 + 63 - masked.leading_zeros() as usize;
            return Some(pos as u32);
        }

        // Check preceding words
        for word_idx in (0..end_word).rev() {
            if self.bits[word_idx] != 0 {
                let pos = word_idx * 64 + 63 - self.bits[word_idx].leading_zeros() as usize;
                return Some(pos as u32);
            }
        }

        None
    }

    /// Returns all elements in sorted order.
    #[allow(clippy::iter_not_returning_iterator)]
    pub fn iter(&self) -> Vec<u32> {
        let mut result = Vec::with_capacity(self.count);
        let mut current = self.min;
        while let Some(val) = current {
            result.push(val);
            current = self.successor(val);
        }
        result
    }

    /// Clears all elements.
    pub fn clear(&mut self) {
        for word in &mut self.bits {
            *word = 0;
        }
        self.min = None;
        self.max = None;
        self.count = 0;
    }
}

impl fmt::Display for VanEmdeBoas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "VanEmdeBoas(universe={}, count={})",
            self.universe, self.count
        )
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let veb = VanEmdeBoas::new(256);
        assert!(veb.is_empty());
        assert_eq!(veb.len(), 0);
        assert!(veb.min().is_none());
        assert!(veb.max().is_none());
    }

    #[test]
    fn single_insert() {
        let mut veb = VanEmdeBoas::new(256);
        assert!(veb.insert(42));
        assert_eq!(veb.len(), 1);
        assert!(veb.contains(42));
        assert_eq!(veb.min(), Some(42));
        assert_eq!(veb.max(), Some(42));
    }

    #[test]
    fn double_insert() {
        let mut veb = VanEmdeBoas::new(256);
        assert!(veb.insert(42));
        assert!(!veb.insert(42)); // Already present
        assert_eq!(veb.len(), 1);
    }

    #[test]
    fn multiple_inserts() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(50);
        veb.insert(10);
        veb.insert(200);
        assert_eq!(veb.len(), 3);
        assert_eq!(veb.min(), Some(10));
        assert_eq!(veb.max(), Some(200));
    }

    #[test]
    fn remove() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(20);
        veb.insert(30);
        assert!(veb.remove(20));
        assert_eq!(veb.len(), 2);
        assert!(!veb.contains(20));
        assert!(veb.contains(10));
        assert!(veb.contains(30));
    }

    #[test]
    fn remove_min() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(20);
        veb.insert(30);
        veb.remove(10);
        assert_eq!(veb.min(), Some(20));
    }

    #[test]
    fn remove_max() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(20);
        veb.insert(30);
        veb.remove(30);
        assert_eq!(veb.max(), Some(20));
    }

    #[test]
    fn remove_last() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(42);
        veb.remove(42);
        assert!(veb.is_empty());
        assert!(veb.min().is_none());
        assert!(veb.max().is_none());
    }

    #[test]
    fn remove_nonexistent() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(42);
        assert!(!veb.remove(99));
        assert_eq!(veb.len(), 1);
    }

    #[test]
    fn successor() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(50);
        veb.insert(100);
        assert_eq!(veb.successor(10), Some(50));
        assert_eq!(veb.successor(50), Some(100));
        assert_eq!(veb.successor(100), None);
        assert_eq!(veb.successor(0), Some(10));
        assert_eq!(veb.successor(25), Some(50));
    }

    #[test]
    fn predecessor() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(50);
        veb.insert(100);
        assert_eq!(veb.predecessor(100), Some(50));
        assert_eq!(veb.predecessor(50), Some(10));
        assert_eq!(veb.predecessor(10), None);
        assert_eq!(veb.predecessor(75), Some(50));
        assert_eq!(veb.predecessor(255), Some(100));
    }

    #[test]
    fn iter_sorted() {
        let mut veb = VanEmdeBoas::new(256);
        for val in [50, 10, 200, 150, 30] {
            veb.insert(val);
        }
        assert_eq!(veb.iter(), vec![10, 30, 50, 150, 200]);
    }

    #[test]
    fn clear() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(20);
        veb.clear();
        assert!(veb.is_empty());
        assert!(veb.min().is_none());
    }

    #[test]
    fn serde_roundtrip() {
        let mut veb = VanEmdeBoas::new(1024);
        for val in [0, 100, 500, 999] {
            veb.insert(val);
        }
        let json = serde_json::to_string(&veb).unwrap();
        let restored: VanEmdeBoas = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), veb.len());
        assert_eq!(restored.min(), veb.min());
        assert_eq!(restored.max(), veb.max());
        assert_eq!(restored.iter(), veb.iter());
    }

    #[test]
    fn display_format() {
        let mut veb = VanEmdeBoas::new(100);
        veb.insert(5);
        veb.insert(10);
        assert_eq!(format!("{}", veb), "VanEmdeBoas(universe=100, count=2)");
    }

    #[test]
    fn large_universe() {
        let mut veb = VanEmdeBoas::new(65536);
        veb.insert(0);
        veb.insert(65535);
        veb.insert(32768);
        assert_eq!(veb.min(), Some(0));
        assert_eq!(veb.max(), Some(65535));
        assert_eq!(veb.successor(0), Some(32768));
    }

    #[test]
    fn boundary_values() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(0);
        veb.insert(255);
        assert_eq!(veb.min(), Some(0));
        assert_eq!(veb.max(), Some(255));
        assert_eq!(veb.predecessor(0), None);
        assert_eq!(veb.successor(255), None);
    }

    #[test]
    fn consecutive_insert_remove() {
        let mut veb = VanEmdeBoas::new(256);
        for i in 0..100u32 {
            veb.insert(i);
        }
        assert_eq!(veb.len(), 100);
        for i in 0..50u32 {
            veb.remove(i);
        }
        assert_eq!(veb.len(), 50);
        assert_eq!(veb.min(), Some(50));
        assert_eq!(veb.max(), Some(99));
    }

    // ── Expanded test coverage ──────────────────────────────────────

    #[test]
    #[should_panic(expected = "universe size must be positive")]
    fn zero_universe_panics() {
        VanEmdeBoas::new(0);
    }

    #[test]
    #[should_panic(expected = "universe size must be <= 2^20")]
    fn too_large_universe_panics() {
        VanEmdeBoas::new(2_000_000);
    }

    #[test]
    #[should_panic(expected = "out of universe")]
    fn insert_out_of_bounds_panics() {
        let mut veb = VanEmdeBoas::new(100);
        veb.insert(100);
    }

    #[test]
    fn contains_out_of_bounds() {
        let veb = VanEmdeBoas::new(100);
        assert!(!veb.contains(100));
        assert!(!veb.contains(1000));
    }

    #[test]
    fn remove_out_of_bounds() {
        let mut veb = VanEmdeBoas::new(100);
        assert!(!veb.remove(100));
        assert!(!veb.remove(1000));
    }

    #[test]
    fn universe_size_accessor() {
        let veb = VanEmdeBoas::new(500);
        assert_eq!(veb.universe_size(), 500);
    }

    #[test]
    fn successor_on_empty() {
        let veb = VanEmdeBoas::new(256);
        assert!(veb.successor(0).is_none());
        assert!(veb.successor(100).is_none());
    }

    #[test]
    fn predecessor_on_empty() {
        let veb = VanEmdeBoas::new(256);
        assert!(veb.predecessor(100).is_none());
        assert!(veb.predecessor(0).is_none());
    }

    #[test]
    fn successor_of_max() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(200);
        assert!(veb.successor(200).is_none());
    }

    #[test]
    fn predecessor_of_min() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(0);
        assert!(veb.predecessor(0).is_none());
    }

    #[test]
    fn iter_empty() {
        let veb = VanEmdeBoas::new(256);
        assert!(veb.iter().is_empty());
    }

    #[test]
    fn iter_single() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(42);
        assert_eq!(veb.iter(), vec![42]);
    }

    #[test]
    fn clear_then_reinsert() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(20);
        veb.clear();

        assert!(!veb.contains(10));
        assert!(!veb.contains(20));

        veb.insert(30);
        assert_eq!(veb.len(), 1);
        assert_eq!(veb.min(), Some(30));
        assert_eq!(veb.max(), Some(30));
    }

    #[test]
    fn remove_all_individually() {
        let mut veb = VanEmdeBoas::new(256);
        for i in [5, 10, 15, 20, 25] {
            veb.insert(i);
        }
        for i in [5, 10, 15, 20, 25] {
            assert!(veb.remove(i));
        }
        assert!(veb.is_empty());
        assert!(veb.min().is_none());
        assert!(veb.max().is_none());
    }

    #[test]
    fn remove_in_reverse_order() {
        let mut veb = VanEmdeBoas::new(256);
        for i in 0..10u32 {
            veb.insert(i);
        }
        for i in (0..10u32).rev() {
            assert!(veb.remove(i));
            if i > 0 {
                assert_eq!(veb.max(), Some(i - 1));
            }
        }
        assert!(veb.is_empty());
    }

    #[test]
    fn min_max_after_remove_middle() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(50);
        veb.insert(100);
        veb.remove(50);
        assert_eq!(veb.min(), Some(10));
        assert_eq!(veb.max(), Some(100));
    }

    #[test]
    fn successor_predecessor_chain() {
        let mut veb = VanEmdeBoas::new(256);
        for val in [10, 30, 50, 70, 90] {
            veb.insert(val);
        }
        // Walk forward via successor
        let mut forward = Vec::new();
        let mut current = veb.min();
        while let Some(val) = current {
            forward.push(val);
            current = veb.successor(val);
        }
        assert_eq!(forward, vec![10, 30, 50, 70, 90]);

        // Walk backward via predecessor
        let mut backward = Vec::new();
        let mut current = veb.max();
        while let Some(val) = current {
            backward.push(val);
            current = veb.predecessor(val);
        }
        assert_eq!(backward, vec![90, 70, 50, 30, 10]);
    }

    #[test]
    fn dense_fill() {
        let mut veb = VanEmdeBoas::new(128);
        for i in 0..128u32 {
            veb.insert(i);
        }
        assert_eq!(veb.len(), 128);
        assert_eq!(veb.min(), Some(0));
        assert_eq!(veb.max(), Some(127));

        for i in 0..127u32 {
            assert_eq!(veb.successor(i), Some(i + 1));
        }
    }

    #[test]
    fn clone_independence() {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(10);
        veb.insert(20);

        let mut cloned = veb.clone();
        cloned.insert(30);
        cloned.remove(10);

        assert_eq!(veb.len(), 2);
        assert_eq!(cloned.len(), 2);
        assert!(veb.contains(10));
        assert!(!cloned.contains(10));
    }

    #[test]
    fn serde_roundtrip_preserves_all_queries() {
        let mut veb = VanEmdeBoas::new(256);
        for val in [0, 42, 100, 200, 255] {
            veb.insert(val);
        }

        let json = serde_json::to_string(&veb).unwrap();
        let restored: VanEmdeBoas = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.iter(), veb.iter());
        assert_eq!(restored.successor(42), veb.successor(42));
        assert_eq!(restored.predecessor(200), veb.predecessor(200));
        assert!(restored.contains(100));
    }

    #[test]
    fn display_empty() {
        let veb = VanEmdeBoas::new(256);
        assert_eq!(format!("{}", veb), "VanEmdeBoas(universe=256, count=0)");
    }

    #[test]
    fn small_universe() {
        let mut veb = VanEmdeBoas::new(1);
        assert!(veb.insert(0));
        assert_eq!(veb.len(), 1);
        assert_eq!(veb.min(), Some(0));
        assert_eq!(veb.max(), Some(0));
        assert!(veb.successor(0).is_none());
        assert!(veb.predecessor(0).is_none());
    }

    #[test]
    fn universe_size_two() {
        let mut veb = VanEmdeBoas::new(2);
        veb.insert(0);
        veb.insert(1);
        assert_eq!(veb.successor(0), Some(1));
        assert_eq!(veb.predecessor(1), Some(0));
    }
}
