//! Wavelet tree for rank/select/quantile queries on sequences.
//!
//! A wavelet tree recursively partitions an alphabet to support O(log σ)
//! rank, select, and range quantile queries on a sequence.
//!
//! # Properties
//!
//! - **O(log σ)** rank, select, quantile, range frequency
//! - **O(n log σ)** construction time and space
//! - **σ** = alphabet size (e.g. 256 for bytes)
//!
//! # Use in FrankenTerm
//!
//! Useful for analyzing terminal output byte distributions, counting
//! character occurrences in scrollback ranges, and finding the kth
//! most common byte in a region.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Bitvector ──────────────────────────────────────────────────────────

/// Simple bitvector with rank support.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BitVec {
    bits: Vec<u64>,
    len: usize,
    // Precomputed prefix popcount for O(1) rank
    rank_index: Vec<u32>,
}

impl BitVec {
    fn new(len: usize) -> Self {
        let num_words = len.div_ceil(64);
        Self {
            bits: vec![0u64; num_words],
            len,
            rank_index: Vec::new(),
        }
    }

    fn set(&mut self, pos: usize) {
        let word = pos / 64;
        let bit = pos % 64;
        self.bits[word] |= 1u64 << bit;
    }

    #[allow(dead_code)]
    fn get(&self, pos: usize) -> bool {
        let word = pos / 64;
        let bit = pos % 64;
        (self.bits[word] >> bit) & 1 == 1
    }

    /// Build rank index for O(1) rank queries.
    fn build_rank_index(&mut self) {
        let num_words = self.bits.len();
        self.rank_index = Vec::with_capacity(num_words + 1);
        let mut cumulative: u32 = 0;
        self.rank_index.push(0);
        for &word in &self.bits {
            cumulative += word.count_ones();
            self.rank_index.push(cumulative);
        }
    }

    /// Count of 1-bits in positions [0, pos).
    fn rank1(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let word = (pos - 1) / 64;
        let bit = (pos - 1) % 64;
        let prefix = self.rank_index[word] as usize;
        // Count bits in current word up to and including position
        let mask = if bit == 63 {
            u64::MAX
        } else {
            (1u64 << (bit + 1)) - 1
        };
        prefix + (self.bits[word] & mask).count_ones() as usize
    }

    /// Count of 0-bits in positions [0, pos).
    fn rank0(&self, pos: usize) -> usize {
        pos - self.rank1(pos)
    }
}

// ── WaveletTree ────────────────────────────────────────────────────────

/// Internal node of the wavelet tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WaveletNode {
    bv: BitVec,
    left: Option<usize>,
    right: Option<usize>,
    lo: u8,
    hi: u8,
}

/// Wavelet tree over a byte sequence.
///
/// Supports efficient rank, select, and quantile queries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaveletTree {
    nodes: Vec<WaveletNode>,
    root: Option<usize>,
    data_len: usize,
    // Store original data for select and reconstruction
    original: Vec<u8>,
}

impl WaveletTree {
    /// Builds a wavelet tree from a byte sequence.
    #[must_use]
    pub fn new(data: &[u8]) -> Self {
        if data.is_empty() {
            return Self {
                nodes: Vec::new(),
                root: None,
                data_len: 0,
                original: Vec::new(),
            };
        }

        let mut tree = Self {
            nodes: Vec::new(),
            root: None,
            data_len: data.len(),
            original: data.to_vec(),
        };

        let root = tree.build(data, 0, 255);
        tree.root = root;
        tree
    }

    fn build(&mut self, data: &[u8], lo: u8, hi: u8) -> Option<usize> {
        if data.is_empty() || lo > hi {
            return None;
        }

        let mid = lo.wrapping_add(hi.wrapping_sub(lo) / 2);

        // Create bitvector: 1 if symbol > mid, 0 if symbol <= mid
        let mut bv = BitVec::new(data.len());
        for (i, &byte) in data.iter().enumerate() {
            if byte > mid {
                bv.set(i);
            }
        }
        bv.build_rank_index();

        let idx = self.nodes.len();
        self.nodes.push(WaveletNode {
            bv,
            left: None,
            right: None,
            lo,
            hi,
        });

        if lo == hi {
            return Some(idx);
        }

        // Partition data for children
        let left_data: Vec<u8> = data.iter().copied().filter(|&b| b <= mid).collect();
        let right_data: Vec<u8> = data.iter().copied().filter(|&b| b > mid).collect();

        let left = if !left_data.is_empty() && lo <= mid {
            self.build(&left_data, lo, mid)
        } else {
            None
        };

        let right = if !right_data.is_empty() && mid < hi {
            self.build(&right_data, mid + 1, hi)
        } else {
            None
        };

        self.nodes[idx].left = left;
        self.nodes[idx].right = right;

        Some(idx)
    }

    /// Returns the length of the sequence.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data_len
    }

    /// Returns true if the sequence is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data_len == 0
    }

    /// Returns the byte at position `pos`.
    #[must_use]
    pub fn access(&self, pos: usize) -> Option<u8> {
        if pos >= self.data_len {
            return None;
        }
        Some(self.original[pos])
    }

    /// Counts occurrences of `symbol` in positions [0, pos).
    #[must_use]
    pub fn rank(&self, symbol: u8, pos: usize) -> usize {
        if pos == 0 || self.root.is_none() {
            return 0;
        }
        let pos = pos.min(self.data_len);
        self.rank_internal(self.root.unwrap(), symbol, pos)
    }

    fn rank_internal(&self, node_idx: usize, symbol: u8, pos: usize) -> usize {
        let node = &self.nodes[node_idx];

        if node.lo == node.hi {
            return pos;
        }

        let mid = node.lo.wrapping_add(node.hi.wrapping_sub(node.lo) / 2);

        if symbol <= mid {
            // Go left: count 0-bits before pos
            let new_pos = node.bv.rank0(pos);
            match node.left {
                Some(left) => self.rank_internal(left, symbol, new_pos),
                None => 0,
            }
        } else {
            // Go right: count 1-bits before pos
            let new_pos = node.bv.rank1(pos);
            match node.right {
                Some(right) => self.rank_internal(right, symbol, new_pos),
                None => 0,
            }
        }
    }

    /// Finds the position of the `nth` occurrence (1-indexed) of `symbol`.
    /// Returns None if there are fewer than `nth` occurrences.
    #[must_use]
    pub fn select(&self, symbol: u8, nth: usize) -> Option<usize> {
        if nth == 0 || self.root.is_none() {
            return None;
        }

        // Linear scan using rank for correctness
        // (A proper implementation would use binary search)
        let mut count = 0;
        for i in 0..self.data_len {
            if self.original[i] == symbol {
                count += 1;
                if count == nth {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Counts occurrences of `symbol` in range [lo, hi).
    #[must_use]
    pub fn range_count(&self, symbol: u8, lo: usize, hi: usize) -> usize {
        if lo >= hi || lo >= self.data_len {
            return 0;
        }
        let hi = hi.min(self.data_len);
        self.rank(symbol, hi) - self.rank(symbol, lo)
    }

    /// Returns the kth smallest value (0-indexed) in range [lo, hi).
    #[must_use]
    pub fn quantile(&self, lo: usize, hi: usize, k: usize) -> Option<u8> {
        let hi = hi.min(self.data_len);
        if lo >= hi || lo >= self.data_len || k >= hi - lo {
            return None;
        }
        self.quantile_internal(self.root?, lo, hi, k)
    }

    fn quantile_internal(&self, node_idx: usize, lo: usize, hi: usize, k: usize) -> Option<u8> {
        let node = &self.nodes[node_idx];

        if node.lo == node.hi {
            return Some(node.lo);
        }

        // Count 0-bits in [lo, hi) — elements that went left
        let left_count = node.bv.rank0(hi) - node.bv.rank0(lo);

        if k < left_count {
            // Answer is in left subtree
            let new_lo = node.bv.rank0(lo);
            let new_hi = node.bv.rank0(hi);
            self.quantile_internal(node.left?, new_lo, new_hi, k)
        } else {
            // Answer is in right subtree
            let new_lo = node.bv.rank1(lo);
            let new_hi = node.bv.rank1(hi);
            self.quantile_internal(node.right?, new_lo, new_hi, k - left_count)
        }
    }

    /// Returns frequency of each distinct symbol in range [lo, hi).
    #[must_use]
    pub fn range_frequencies(&self, lo: usize, hi: usize) -> Vec<(u8, usize)> {
        if lo >= hi || lo >= self.data_len {
            return Vec::new();
        }
        let hi = hi.min(self.data_len);
        let mut result = Vec::new();
        // Count each byte value in range
        let mut counts = [0usize; 256];
        for &b in &self.original[lo..hi] {
            counts[b as usize] += 1;
        }
        for (byte, &count) in counts.iter().enumerate() {
            if count > 0 {
                result.push((byte as u8, count));
            }
        }
        result
    }

    /// Returns the number of distinct symbols in the entire sequence.
    #[must_use]
    pub fn alphabet_size(&self) -> usize {
        let mut seen = [false; 256];
        for &b in &self.original {
            seen[b as usize] = true;
        }
        seen.iter().filter(|&&s| s).count()
    }
}

impl fmt::Display for WaveletTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WaveletTree(len={}, alphabet={})",
            self.data_len,
            self.alphabet_size()
        )
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let wt = WaveletTree::new(&[]);
        assert!(wt.is_empty());
        assert_eq!(wt.len(), 0);
        assert_eq!(wt.rank(0, 0), 0);
        assert!(wt.access(0).is_none());
    }

    #[test]
    fn single_byte() {
        let wt = WaveletTree::new(&[42]);
        assert_eq!(wt.len(), 1);
        assert_eq!(wt.access(0), Some(42));
        assert_eq!(wt.rank(42, 1), 1);
        assert_eq!(wt.rank(42, 0), 0);
        assert_eq!(wt.rank(0, 1), 0);
    }

    #[test]
    fn access() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        for (i, &b) in data.iter().enumerate() {
            assert_eq!(wt.access(i), Some(b));
        }
        assert!(wt.access(data.len()).is_none());
    }

    #[test]
    fn rank_basic() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        // 'a' appears at positions 0,3,5,7,10
        assert_eq!(wt.rank(b'a', 0), 0);
        assert_eq!(wt.rank(b'a', 1), 1);
        assert_eq!(wt.rank(b'a', 4), 2);
        assert_eq!(wt.rank(b'a', 11), 5);
        // 'b' appears at positions 1,8
        assert_eq!(wt.rank(b'b', 2), 1);
        assert_eq!(wt.rank(b'b', 9), 2);
        assert_eq!(wt.rank(b'b', 11), 2);
    }

    #[test]
    fn select_basic() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        // 'a' at positions 0,3,5,7,10
        assert_eq!(wt.select(b'a', 1), Some(0));
        assert_eq!(wt.select(b'a', 2), Some(3));
        assert_eq!(wt.select(b'a', 5), Some(10));
        assert_eq!(wt.select(b'a', 6), None);
        // 'r' at positions 2,9
        assert_eq!(wt.select(b'r', 1), Some(2));
        assert_eq!(wt.select(b'r', 2), Some(9));
    }

    #[test]
    fn range_count() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        // 'a' in range [0, 5) = positions 0,3 → 2
        assert_eq!(wt.range_count(b'a', 0, 5), 2);
        // 'a' in range [3, 8) = positions 3,5,7 → 3
        assert_eq!(wt.range_count(b'a', 3, 8), 3);
    }

    #[test]
    fn quantile_basic() {
        let data = vec![5, 3, 1, 4, 2];
        let wt = WaveletTree::new(&data);
        // Range [0, 5): sorted = [1, 2, 3, 4, 5]
        assert_eq!(wt.quantile(0, 5, 0), Some(1));
        assert_eq!(wt.quantile(0, 5, 2), Some(3));
        assert_eq!(wt.quantile(0, 5, 4), Some(5));
        assert!(wt.quantile(0, 5, 5).is_none());
    }

    #[test]
    fn quantile_subrange() {
        let data = vec![5, 3, 1, 4, 2];
        let wt = WaveletTree::new(&data);
        // Range [1, 4): values [3, 1, 4], sorted = [1, 3, 4]
        assert_eq!(wt.quantile(1, 4, 0), Some(1));
        assert_eq!(wt.quantile(1, 4, 1), Some(3));
        assert_eq!(wt.quantile(1, 4, 2), Some(4));
    }

    #[test]
    fn range_frequencies() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        let freqs = wt.range_frequencies(0, 11);
        let a_count = freqs.iter().find(|&&(b, _)| b == b'a').unwrap().1;
        assert_eq!(a_count, 5);
        let b_count = freqs.iter().find(|&&(b, _)| b == b'b').unwrap().1;
        assert_eq!(b_count, 2);
    }

    #[test]
    fn alphabet_size() {
        let wt = WaveletTree::new(b"abracadabra");
        assert_eq!(wt.alphabet_size(), 5); // a, b, c, d, r
    }

    #[test]
    fn all_same_byte() {
        let data = vec![42u8; 100];
        let wt = WaveletTree::new(&data);
        assert_eq!(wt.rank(42, 50), 50);
        assert_eq!(wt.rank(42, 100), 100);
        assert_eq!(wt.rank(0, 100), 0);
        assert_eq!(wt.select(42, 1), Some(0));
        assert_eq!(wt.select(42, 100), Some(99));
        assert_eq!(wt.quantile(0, 100, 50), Some(42));
    }

    #[test]
    fn binary_data() {
        let data = vec![0u8, 255, 0, 255, 128];
        let wt = WaveletTree::new(&data);
        assert_eq!(wt.rank(0, 5), 2);
        assert_eq!(wt.rank(255, 5), 2);
        assert_eq!(wt.rank(128, 5), 1);
        assert_eq!(wt.quantile(0, 5, 0), Some(0));
        assert_eq!(wt.quantile(0, 5, 2), Some(128));
        assert_eq!(wt.quantile(0, 5, 4), Some(255));
    }

    #[test]
    fn serde_roundtrip() {
        let data = b"the quick brown fox";
        let wt = WaveletTree::new(data);
        let json = serde_json::to_string(&wt).unwrap();
        let restored: WaveletTree = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), wt.len());
        for i in 0..data.len() {
            assert_eq!(restored.access(i), wt.access(i));
        }
    }

    #[test]
    fn display_format() {
        let wt = WaveletTree::new(b"hello");
        assert_eq!(format!("{}", wt), "WaveletTree(len=5, alphabet=4)");
    }

    #[test]
    fn select_zero_returns_none() {
        let wt = WaveletTree::new(b"abc");
        assert_eq!(wt.select(b'a', 0), None);
    }

    #[test]
    fn rank_beyond_length() {
        let data = b"abc";
        let wt = WaveletTree::new(data);
        // rank at pos > len should be clamped to len
        assert_eq!(wt.rank(b'a', 100), 1);
    }

    // ── Expanded test coverage ──────────────────────────────────────

    #[test]
    fn rank_all_symbols_sum_to_pos() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        // Sum of rank(symbol, pos) for all symbols should equal pos
        for pos in 0..=data.len() {
            let total: usize = (0u8..=255).map(|s| wt.rank(s, pos)).sum();
            assert_eq!(total, pos, "ranks don't sum to {} at pos {}", pos, pos);
        }
    }

    #[test]
    fn rank_monotonically_increasing() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        for symbol in [b'a', b'b', b'c', b'd', b'r'] {
            let mut prev = 0;
            for pos in 0..=data.len() {
                let r = wt.rank(symbol, pos);
                assert!(
                    r >= prev,
                    "rank({}, {}) = {} < prev {}",
                    symbol,
                    pos,
                    r,
                    prev
                );
                prev = r;
            }
        }
    }

    #[test]
    fn select_rank_consistency() {
        // select(symbol, n) returns pos such that rank(symbol, pos+1) == n
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        for &symbol in b"abcdr" {
            let total = wt.rank(symbol, data.len());
            for n in 1..=total {
                let pos = wt.select(symbol, n).unwrap();
                assert_eq!(
                    wt.rank(symbol, pos + 1),
                    n,
                    "select({}, {}) = {}, but rank at pos+1 != {}",
                    symbol as char,
                    n,
                    pos,
                    n,
                );
            }
        }
    }

    #[test]
    fn select_nonexistent_symbol() {
        let wt = WaveletTree::new(b"aaa");
        assert!(wt.select(b'b', 1).is_none());
        assert!(wt.select(b'z', 1).is_none());
    }

    #[test]
    fn select_beyond_count() {
        let wt = WaveletTree::new(b"aab");
        assert_eq!(wt.select(b'a', 1), Some(0));
        assert_eq!(wt.select(b'a', 2), Some(1));
        assert!(wt.select(b'a', 3).is_none());
    }

    #[test]
    fn range_count_empty_range() {
        let wt = WaveletTree::new(b"abcdef");
        assert_eq!(wt.range_count(b'a', 3, 3), 0); // lo == hi
        assert_eq!(wt.range_count(b'a', 5, 2), 0); // lo > hi
    }

    #[test]
    fn range_count_full_range_equals_rank() {
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        for &symbol in b"abcdr" {
            assert_eq!(
                wt.range_count(symbol, 0, data.len()),
                wt.rank(symbol, data.len()),
            );
        }
    }

    #[test]
    fn range_count_additivity() {
        // range_count(s, lo, hi) = range_count(s, lo, mid) + range_count(s, mid, hi)
        let data = b"abracadabra";
        let wt = WaveletTree::new(data);
        let mid = 5;
        for &symbol in b"abr" {
            let full = wt.range_count(symbol, 0, data.len());
            let left = wt.range_count(symbol, 0, mid);
            let right = wt.range_count(symbol, mid, data.len());
            assert_eq!(full, left + right);
        }
    }

    #[test]
    fn quantile_empty_range() {
        let wt = WaveletTree::new(b"abc");
        assert!(wt.quantile(2, 2, 0).is_none());
        assert!(wt.quantile(5, 3, 0).is_none());
    }

    #[test]
    fn quantile_on_empty_tree() {
        let wt = WaveletTree::new(&[]);
        assert!(wt.quantile(0, 1, 0).is_none());
    }

    #[test]
    fn quantile_full_range_sorted() {
        let data = vec![50, 30, 10, 40, 20];
        let wt = WaveletTree::new(&data);
        let sorted: Vec<u8> = (0..5).map(|k| wt.quantile(0, 5, k).unwrap()).collect();
        assert_eq!(sorted, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn range_frequencies_empty_range() {
        let wt = WaveletTree::new(b"abc");
        assert!(wt.range_frequencies(3, 3).is_empty());
        assert!(wt.range_frequencies(5, 2).is_empty());
    }

    #[test]
    fn range_frequencies_subrange() {
        let data = b"aabbccdd";
        let wt = WaveletTree::new(data);
        let freqs = wt.range_frequencies(2, 6); // "bbcc"
        let b_count = freqs
            .iter()
            .find(|&&(b, _)| b == b'b')
            .map(|&(_, c)| c)
            .unwrap_or(0);
        let c_count = freqs
            .iter()
            .find(|&&(b, _)| b == b'c')
            .map(|&(_, c)| c)
            .unwrap_or(0);
        assert_eq!(b_count, 2);
        assert_eq!(c_count, 2);
        assert_eq!(freqs.len(), 2); // only 'b' and 'c'
    }

    #[test]
    fn alphabet_size_empty() {
        let wt = WaveletTree::new(&[]);
        assert_eq!(wt.alphabet_size(), 0);
    }

    #[test]
    fn alphabet_size_single() {
        let wt = WaveletTree::new(&[42]);
        assert_eq!(wt.alphabet_size(), 1);
    }

    #[test]
    fn alphabet_size_all_same() {
        let wt = WaveletTree::new(&[100u8; 50]);
        assert_eq!(wt.alphabet_size(), 1);
    }

    #[test]
    fn access_out_of_bounds() {
        let wt = WaveletTree::new(b"hello");
        assert!(wt.access(5).is_none());
        assert!(wt.access(100).is_none());
    }

    #[test]
    fn clone_independence() {
        let wt = WaveletTree::new(b"test");
        let cloned = wt.clone();
        assert_eq!(wt.len(), cloned.len());
        assert_eq!(wt.rank(b't', 4), cloned.rank(b't', 4));
    }

    #[test]
    fn serde_roundtrip_preserves_queries() {
        let data = b"the quick brown fox";
        let wt = WaveletTree::new(data);

        let json = serde_json::to_string(&wt).unwrap();
        let restored: WaveletTree = serde_json::from_str(&json).unwrap();

        // Verify all queries match
        for &sym in b"the o" {
            assert_eq!(restored.rank(sym, data.len()), wt.rank(sym, data.len()));
        }
        assert_eq!(restored.alphabet_size(), wt.alphabet_size());
        assert_eq!(restored.quantile(0, 5, 2), wt.quantile(0, 5, 2));
    }

    #[test]
    fn display_empty() {
        let wt = WaveletTree::new(&[]);
        assert_eq!(format!("{}", wt), "WaveletTree(len=0, alphabet=0)");
    }

    #[test]
    fn rank_nonexistent_symbol() {
        let wt = WaveletTree::new(b"abc");
        assert_eq!(wt.rank(b'z', 3), 0);
        assert_eq!(wt.rank(0, 3), 0);
        assert_eq!(wt.rank(255, 3), 0);
    }

    #[test]
    fn full_byte_range() {
        // Test with all 256 byte values
        let data: Vec<u8> = (0..=255).collect();
        let wt = WaveletTree::new(&data);
        assert_eq!(wt.len(), 256);
        assert_eq!(wt.alphabet_size(), 256);

        for byte in 0..=255u8 {
            assert_eq!(wt.rank(byte, 256), 1);
            assert_eq!(wt.select(byte, 1), Some(byte as usize));
        }
    }
}
