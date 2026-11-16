//! XOR filter for space-efficient probabilistic set membership.
//!
//! An XOR filter is a static probabilistic data structure that uses
//! ~1.23 × n × fingerprint_bytes to represent a set of n items,
//! more compact than Bloom filters at equivalent false-positive rates.
//!
//! # Properties
//!
//! - **O(1)** lookup with exactly 3 memory accesses + 1 XOR
//! - **~9.84 bits/key** for 8-bit fingerprints (vs ~10 for Bloom at 1% FP)
//! - **~19.68 bits/key** for 16-bit fingerprints
//! - **Static**: built once from a known set, cannot be modified
//! - **No false negatives**: members always report true
//! - **False positive rate**: ~2^(-fingerprint_bits)
//!
//! # Variants
//!
//! - [`XorFilter`]: 8-bit fingerprints, FP rate ≈ 1/256 ≈ 0.39%
//! - [`XorFilter16`]: 16-bit fingerprints, FP rate ≈ 1/65536 ≈ 0.0015%
//!
//! # Use in FrankenTerm
//!
//! - **Dedup pre-filter**: quickly reject definitely-unseen pane content hashes
//!   before expensive storage lookups.
//! - **Event routing**: compact membership check for "is this rule_id in the
//!   active set?" across thousands of pattern rules.
//! - **Snapshot diff**: fast set-difference pre-filter when comparing captured
//!   segment fingerprints between snapshots.
//!
//! # Algorithm
//!
//! Based on "Xor Filters: Faster and Smaller Than Bloom and Cuckoo Filters"
//! (Graf & Lemire, 2020). Construction uses a peeling algorithm on a
//! 3-partite hypergraph:
//!
//! 1. Hash each key to 3 positions (one per segment) + a fingerprint.
//! 2. Build a hypergraph and iteratively peel degree-1 vertices.
//! 3. Assign fingerprint values in reverse peeling order via XOR.
//!
//! If peeling fails (cycle detected), re-seed and retry.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Hash functions ─────────────────────────────────────────────────────

/// MurmurHash3-style 64-bit finalizer.
fn murmur_mix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
    h ^= h >> 33;
    h
}

/// Hash a key with a seed.
#[inline]
fn hash_with_seed(key: u64, seed: u64) -> u64 {
    murmur_mix(key.wrapping_add(seed))
}

/// FNV-1a 64-bit hash for byte slices.
fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Compute 8-bit fingerprint from a key+seed.
#[inline]
fn fingerprint8(key: u64, seed: u64) -> u8 {
    let h = hash_with_seed(key, seed.wrapping_add(0xfedcba9876543210));
    let fp = (h & 0xFF) as u8;
    if fp == 0 { 1 } else { fp }
}

/// Compute 16-bit fingerprint from a key+seed.
#[inline]
fn fingerprint16(key: u64, seed: u64) -> u16 {
    let h = hash_with_seed(key, seed.wrapping_add(0xfedcba9876543210));
    let fp = (h & 0xFFFF) as u16;
    if fp == 0 { 1 } else { fp }
}

// ── Errors ─────────────────────────────────────────────────────────────

/// Error returned when XOR filter construction fails.
#[derive(Debug, Clone)]
pub enum XorFilterError {
    /// Construction failed after the maximum number of retry attempts.
    MaxRetriesExceeded {
        /// Number of retries attempted.
        attempts: usize,
    },
}

impl fmt::Display for XorFilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxRetriesExceeded { attempts } => {
                write!(
                    f,
                    "XOR filter construction failed after {attempts} attempts"
                )
            }
        }
    }
}

impl std::error::Error for XorFilterError {}

// ── XorFilter (8-bit fingerprints) ─────────────────────────────────────

/// Static probabilistic membership filter using XOR-based construction.
///
/// After construction, supports O(1) membership queries with a
/// false positive rate of ≈ 1/256 ≈ 0.39%.
///
/// # Example
///
/// ```
/// use frankenterm_core::xor_filter::XorFilter;
///
/// let keys: Vec<u64> = vec![42, 99, 1337];
/// let filter = XorFilter::build(&keys).unwrap();
/// assert!(filter.contains(42));
/// assert!(filter.contains(1337));
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XorFilter {
    fingerprints: Vec<u8>,
    seed: u64,
    block_length: usize,
    num_keys: usize,
}

/// Maximum construction retries before giving up.
const MAX_ATTEMPTS: usize = 64;

impl XorFilter {
    /// Builds an XOR filter from a set of 64-bit keys.
    ///
    /// Duplicate keys are automatically deduplicated. Empty input produces
    /// an empty filter. Returns `None` if construction fails after all
    /// retry attempts (astronomically unlikely).
    #[must_use]
    pub fn build(keys: &[u64]) -> Option<Self> {
        Self::build_with_max_attempts(keys, MAX_ATTEMPTS)
    }

    /// Builds an XOR filter, returning a proper error on failure.
    ///
    /// # Errors
    ///
    /// Returns [`XorFilterError::MaxRetriesExceeded`] if construction fails
    /// after the maximum number of retries.
    pub fn try_build(keys: &[u64]) -> Result<Self, XorFilterError> {
        Self::build(keys).ok_or(XorFilterError::MaxRetriesExceeded {
            attempts: MAX_ATTEMPTS,
        })
    }

    fn build_with_max_attempts(keys: &[u64], max_attempts: usize) -> Option<Self> {
        if keys.is_empty() {
            return Some(Self {
                fingerprints: Vec::new(),
                seed: 0,
                block_length: 0,
                num_keys: 0,
            });
        }

        let mut unique_keys: Vec<u64> = keys.to_vec();
        unique_keys.sort_unstable();
        unique_keys.dedup();
        let n = unique_keys.len();

        let capacity = ((n as f64 * 1.23) as usize + 32).max(64);
        let block_length = capacity / 3;
        let total = block_length * 3;

        for attempt in 0..max_attempts {
            let seed = murmur_mix(attempt as u64 ^ 0x123456789abcdef0);
            if let Some(fingerprints) = try_build_8bit(&unique_keys, seed, block_length, total) {
                return Some(Self {
                    fingerprints,
                    seed,
                    block_length,
                    num_keys: n,
                });
            }
        }
        None
    }

    /// Tests whether a key is probably in the set.
    ///
    /// Returns `true` if the key was in the original set (no false negatives),
    /// or with probability ≈ 1/256 for keys not in the set.
    #[must_use]
    pub fn contains(&self, key: u64) -> bool {
        if self.fingerprints.is_empty() || self.block_length == 0 {
            return false;
        }
        // Guard against corrupted deserialized state where fingerprints
        // is shorter than the expected 3 * block_length.
        let expected_len = self.block_length.saturating_mul(3);
        if self.fingerprints.len() < expected_len {
            return false;
        }

        let hash = hash_with_seed(key, self.seed);
        let h0 = (hash as usize) % self.block_length;
        let h1 = self.block_length + ((hash >> 21) as usize) % self.block_length;
        let h2 = 2 * self.block_length + ((hash >> 42) as usize) % self.block_length;

        let fp = fingerprint8(key, self.seed);
        let xor_val = self.fingerprints[h0] ^ self.fingerprints[h1] ^ self.fingerprints[h2];
        fp == xor_val
    }

    /// Builds a filter from byte slice keys (hashed via FNV-1a).
    #[must_use]
    pub fn from_bytes(keys: &[&[u8]]) -> Option<Self> {
        let hashed: Vec<u64> = keys.iter().map(|k| fnv1a_hash(k)).collect();
        Self::build(&hashed)
    }

    /// Tests if a byte slice key is probably in the set.
    #[must_use]
    pub fn contains_bytes(&self, key: &[u8]) -> bool {
        self.contains(fnv1a_hash(key))
    }

    /// Number of keys in the filter.
    #[must_use]
    pub fn num_keys(&self) -> usize {
        self.num_keys
    }

    /// Size of the fingerprint table in bytes.
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        self.fingerprints.len()
    }

    /// Bits per key (storage efficiency metric).
    #[must_use]
    pub fn bits_per_key(&self) -> f64 {
        if self.num_keys == 0 {
            return 0.0;
        }
        (self.fingerprints.len() * 8) as f64 / self.num_keys as f64
    }

    /// True if the filter contains zero keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.num_keys == 0
    }

    /// Theoretical false-positive rate for 8-bit fingerprints.
    #[must_use]
    pub fn theoretical_fp_rate() -> f64 {
        1.0 / 256.0 // 2^-8: standard XOR filter FP rate for 8-bit fingerprints
    }

    /// Get serializable statistics.
    #[must_use]
    pub fn stats(&self) -> XorFilterStats {
        XorFilterStats {
            num_keys: self.num_keys,
            table_len: self.fingerprints.len(),
            block_length: self.block_length,
            fingerprint_bits: 8,
            memory_bytes: self.size_bytes(),
            bits_per_key: self.bits_per_key(),
            theoretical_fp_rate: Self::theoretical_fp_rate(),
            seed: self.seed,
        }
    }
}

impl fmt::Display for XorFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "XorFilter(keys={}, bytes={}, bpk={:.2})",
            self.num_keys,
            self.size_bytes(),
            self.bits_per_key()
        )
    }
}

// ── XorFilter16 (16-bit fingerprints) ──────────────────────────────────

/// XOR filter with 16-bit fingerprints.
///
/// False-positive rate ≈ 1/65535 ≈ 0.0015%.
/// Space usage ≈ 2.46 bytes per key.
///
/// Same construction algorithm as [`XorFilter`] but with 16-bit
/// fingerprint slots for dramatically lower false-positive rates.
///
/// # Example
///
/// ```
/// use frankenterm_core::xor_filter::XorFilter16;
///
/// let keys: Vec<u64> = (0..1000).collect();
/// let filter = XorFilter16::build(&keys).unwrap();
/// assert!(filter.contains(42));
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XorFilter16 {
    fingerprints: Vec<u16>,
    seed: u64,
    block_length: usize,
    num_keys: usize,
}

impl XorFilter16 {
    /// Builds a 16-bit XOR filter from a set of 64-bit keys.
    ///
    /// Duplicate keys are automatically deduplicated.
    #[must_use]
    pub fn build(keys: &[u64]) -> Option<Self> {
        Self::build_with_max_attempts(keys, MAX_ATTEMPTS)
    }

    /// Builds, returning a proper error on failure.
    ///
    /// # Errors
    ///
    /// Returns [`XorFilterError::MaxRetriesExceeded`] on construction failure.
    pub fn try_build(keys: &[u64]) -> Result<Self, XorFilterError> {
        Self::build(keys).ok_or(XorFilterError::MaxRetriesExceeded {
            attempts: MAX_ATTEMPTS,
        })
    }

    fn build_with_max_attempts(keys: &[u64], max_attempts: usize) -> Option<Self> {
        if keys.is_empty() {
            return Some(Self {
                fingerprints: Vec::new(),
                seed: 0,
                block_length: 0,
                num_keys: 0,
            });
        }

        let mut unique_keys: Vec<u64> = keys.to_vec();
        unique_keys.sort_unstable();
        unique_keys.dedup();
        let n = unique_keys.len();

        let capacity = ((n as f64 * 1.23) as usize + 32).max(64);
        let block_length = capacity / 3;
        let total = block_length * 3;

        for attempt in 0..max_attempts {
            let seed = murmur_mix(attempt as u64 ^ 0x123456789abcdef0);
            if let Some(fingerprints) = try_build_16bit(&unique_keys, seed, block_length, total) {
                return Some(Self {
                    fingerprints,
                    seed,
                    block_length,
                    num_keys: n,
                });
            }
        }
        None
    }

    /// Tests whether a key is probably in the set.
    ///
    /// FP rate ≈ 1/65535 ≈ 0.0015%.
    #[must_use]
    pub fn contains(&self, key: u64) -> bool {
        if self.fingerprints.is_empty() || self.block_length == 0 {
            return false;
        }
        // Guard against corrupted deserialized state where fingerprints
        // is shorter than the expected 3 * block_length.
        let expected_len = self.block_length.saturating_mul(3);
        if self.fingerprints.len() < expected_len {
            return false;
        }

        let hash = hash_with_seed(key, self.seed);
        let h0 = (hash as usize) % self.block_length;
        let h1 = self.block_length + ((hash >> 21) as usize) % self.block_length;
        let h2 = 2 * self.block_length + ((hash >> 42) as usize) % self.block_length;

        let fp = fingerprint16(key, self.seed);
        let xor_val = self.fingerprints[h0] ^ self.fingerprints[h1] ^ self.fingerprints[h2];
        fp == xor_val
    }

    /// Builds from byte slice keys (hashed via FNV-1a).
    #[must_use]
    pub fn from_bytes(keys: &[&[u8]]) -> Option<Self> {
        let hashed: Vec<u64> = keys.iter().map(|k| fnv1a_hash(k)).collect();
        Self::build(&hashed)
    }

    /// Tests if a byte slice key is probably in the set.
    #[must_use]
    pub fn contains_bytes(&self, key: &[u8]) -> bool {
        self.contains(fnv1a_hash(key))
    }

    /// Number of keys in the filter.
    #[must_use]
    pub fn num_keys(&self) -> usize {
        self.num_keys
    }

    /// Size of the fingerprint table in bytes.
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        self.fingerprints.len() * 2
    }

    /// Bits per key.
    #[must_use]
    pub fn bits_per_key(&self) -> f64 {
        if self.num_keys == 0 {
            return 0.0;
        }
        (self.fingerprints.len() * 16) as f64 / self.num_keys as f64
    }

    /// True if the filter contains zero keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.num_keys == 0
    }

    /// Theoretical false-positive rate for 16-bit fingerprints.
    #[must_use]
    pub fn theoretical_fp_rate() -> f64 {
        1.0 / 65536.0 // 2^-16
    }

    /// Get serializable statistics.
    #[must_use]
    pub fn stats(&self) -> XorFilterStats {
        XorFilterStats {
            num_keys: self.num_keys,
            table_len: self.fingerprints.len(),
            block_length: self.block_length,
            fingerprint_bits: 16,
            memory_bytes: self.size_bytes(),
            bits_per_key: self.bits_per_key(),
            theoretical_fp_rate: Self::theoretical_fp_rate(),
            seed: self.seed,
        }
    }
}

impl fmt::Display for XorFilter16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "XorFilter16(keys={}, bytes={}, bpk={:.2})",
            self.num_keys,
            self.size_bytes(),
            self.bits_per_key()
        )
    }
}

// ── XorFilterStats ─────────────────────────────────────────────────────

/// Serializable statistics about an XOR filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XorFilterStats {
    /// Number of keys the filter was built from.
    pub num_keys: usize,
    /// Total number of fingerprint slots (3 × block_length).
    pub table_len: usize,
    /// Length of each block.
    pub block_length: usize,
    /// Bits per fingerprint (8 or 16).
    pub fingerprint_bits: u8,
    /// Memory usage in bytes.
    pub memory_bytes: usize,
    /// Actual bits per key.
    pub bits_per_key: f64,
    /// Theoretical false-positive rate.
    pub theoretical_fp_rate: f64,
    /// Seed used during construction.
    pub seed: u64,
}

// ── Internal construction (8-bit) ──────────────────────────────────────

fn try_build_8bit(keys: &[u64], seed: u64, block_length: usize, total: usize) -> Option<Vec<u8>> {
    let n = keys.len();

    let mut h0s = Vec::with_capacity(n);
    let mut h1s = Vec::with_capacity(n);
    let mut h2s = Vec::with_capacity(n);

    for &key in keys {
        let hash = hash_with_seed(key, seed);
        h0s.push((hash as usize) % block_length);
        h1s.push(block_length + ((hash >> 21) as usize) % block_length);
        h2s.push(2 * block_length + ((hash >> 42) as usize) % block_length);
    }

    let mut sets: Vec<Vec<usize>> = vec![Vec::new(); total];
    for i in 0..n {
        sets[h0s[i]].push(i);
        sets[h1s[i]].push(i);
        sets[h2s[i]].push(i);
    }

    // Peeling
    let mut queue: Vec<usize> = Vec::new();
    for (pos, set) in sets.iter().enumerate() {
        if set.len() == 1 {
            queue.push(pos);
        }
    }

    let mut order: Vec<(usize, usize)> = Vec::with_capacity(n);
    let mut removed = vec![false; n];

    while let Some(pos) = queue.pop() {
        let key_idx = match sets[pos].iter().find(|&&ki| !removed[ki]) {
            Some(&ki) => ki,
            None => continue,
        };

        if removed[key_idx] {
            continue;
        }

        removed[key_idx] = true;
        order.push((key_idx, pos));

        for &p in &[h0s[key_idx], h1s[key_idx], h2s[key_idx]] {
            if p == pos {
                continue;
            }
            let remaining = sets[p].iter().filter(|&&ki| !removed[ki]).count();
            if remaining == 1 {
                queue.push(p);
            }
        }
    }

    if order.len() != n {
        return None;
    }

    // Assign fingerprints in reverse
    let mut fingerprints = vec![0u8; total];
    for &(key_idx, pos) in order.iter().rev() {
        let key = keys[key_idx];
        let fp = fingerprint8(key, seed);
        let xor_val =
            fingerprints[h0s[key_idx]] ^ fingerprints[h1s[key_idx]] ^ fingerprints[h2s[key_idx]];
        fingerprints[pos] = fp ^ xor_val;
    }

    Some(fingerprints)
}

// ── Internal construction (16-bit) ─────────────────────────────────────

fn try_build_16bit(keys: &[u64], seed: u64, block_length: usize, total: usize) -> Option<Vec<u16>> {
    let n = keys.len();

    let mut h0s = Vec::with_capacity(n);
    let mut h1s = Vec::with_capacity(n);
    let mut h2s = Vec::with_capacity(n);

    for &key in keys {
        let hash = hash_with_seed(key, seed);
        h0s.push((hash as usize) % block_length);
        h1s.push(block_length + ((hash >> 21) as usize) % block_length);
        h2s.push(2 * block_length + ((hash >> 42) as usize) % block_length);
    }

    let mut sets: Vec<Vec<usize>> = vec![Vec::new(); total];
    for i in 0..n {
        sets[h0s[i]].push(i);
        sets[h1s[i]].push(i);
        sets[h2s[i]].push(i);
    }

    let mut queue: Vec<usize> = Vec::new();
    for (pos, set) in sets.iter().enumerate() {
        if set.len() == 1 {
            queue.push(pos);
        }
    }

    let mut order: Vec<(usize, usize)> = Vec::with_capacity(n);
    let mut removed = vec![false; n];

    while let Some(pos) = queue.pop() {
        let key_idx = match sets[pos].iter().find(|&&ki| !removed[ki]) {
            Some(&ki) => ki,
            None => continue,
        };

        if removed[key_idx] {
            continue;
        }

        removed[key_idx] = true;
        order.push((key_idx, pos));

        for &p in &[h0s[key_idx], h1s[key_idx], h2s[key_idx]] {
            if p == pos {
                continue;
            }
            let remaining = sets[p].iter().filter(|&&ki| !removed[ki]).count();
            if remaining == 1 {
                queue.push(p);
            }
        }
    }

    if order.len() != n {
        return None;
    }

    let mut fingerprints = vec![0u16; total];
    for &(key_idx, pos) in order.iter().rev() {
        let key = keys[key_idx];
        let fp = fingerprint16(key, seed);
        let xor_val =
            fingerprints[h0s[key_idx]] ^ fingerprints[h1s[key_idx]] ^ fingerprints[h2s[key_idx]];
        fingerprints[pos] = fp ^ xor_val;
    }

    Some(fingerprints)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- XorFilter (8-bit) construction -----------------------------------------

    #[test]
    fn empty_filter() {
        let filter = XorFilter::build(&[]).unwrap();
        assert!(filter.is_empty());
        assert_eq!(filter.num_keys(), 0);
        assert!(!filter.contains(42));
    }

    #[test]
    fn single_key() {
        let filter = XorFilter::build(&[42]).unwrap();
        assert_eq!(filter.num_keys(), 1);
        assert!(filter.contains(42));
    }

    #[test]
    fn two_keys() {
        let filter = XorFilter::build(&[0, u64::MAX]).unwrap();
        assert!(filter.contains(0));
        assert!(filter.contains(u64::MAX));
    }

    #[test]
    fn consecutive_keys() {
        let keys: Vec<u64> = (0..50).collect();
        let filter = XorFilter::build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k), "false negative for {k}");
        }
    }

    #[test]
    fn no_false_negatives() {
        let keys: Vec<u64> = (0..500).collect();
        let filter = XorFilter::build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k), "false negative for key {k}");
        }
    }

    #[test]
    fn large_set() {
        let keys: Vec<u64> = (0..5000).collect();
        let filter = XorFilter::build(&keys).unwrap();
        assert_eq!(filter.num_keys(), 5000);
        for &k in &keys {
            assert!(filter.contains(k));
        }
    }

    #[test]
    fn sparse_keys() {
        let keys: Vec<u64> = (0..100).map(|i| i * 1_000_000).collect();
        let filter = XorFilter::build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k));
        }
    }

    #[test]
    fn powers_of_two() {
        let keys: Vec<u64> = (0..60).map(|i| 1u64 << i).collect();
        let filter = XorFilter::build(&keys).unwrap();
        for &k in &keys {
            assert!(
                filter.contains(k),
                "false negative for 2^{}",
                k.trailing_zeros()
            );
        }
    }

    #[test]
    fn duplicate_keys_deduped() {
        let keys = vec![1u64, 2, 3, 1, 2, 3, 1, 2, 3];
        let filter = XorFilter::build(&keys).unwrap();
        assert_eq!(filter.num_keys(), 3);
        assert!(filter.contains(1));
        assert!(filter.contains(2));
        assert!(filter.contains(3));
    }

    // -- False-positive rate bounds (8-bit) -------------------------------------

    #[test]
    fn low_false_positive_rate() {
        let keys: Vec<u64> = (0..1000).collect();
        let filter = XorFilter::build(&keys).unwrap();

        let test_range = 1000..10000u64;
        let fp_count = test_range.clone().filter(|k| filter.contains(*k)).count();
        let fp_rate = fp_count as f64 / (test_range.end - test_range.start) as f64;
        assert!(
            fp_rate < 0.02,
            "FP rate too high: {fp_rate:.4} ({fp_count} false positives)"
        );
    }

    #[test]
    fn fp_rate_large_sample() {
        let n: u64 = 5000;
        let keys: Vec<u64> = (0..n).collect();
        let filter = XorFilter::build(&keys).unwrap();

        let test_count = 50_000u64;
        let mut fp = 0u64;
        for i in n..(n + test_count) {
            if filter.contains(i) {
                fp += 1;
            }
        }

        let rate = fp as f64 / test_count as f64;
        // 8-bit FP ≈ 1/255 ≈ 0.0039. Allow 3× tolerance.
        assert!(
            rate < 0.012,
            "8-bit FP rate {rate:.4} exceeds 3× theoretical"
        );
    }

    // -- Space efficiency (8-bit) -----------------------------------------------

    #[test]
    fn storage_efficiency() {
        let keys: Vec<u64> = (0..1000).collect();
        let filter = XorFilter::build(&keys).unwrap();
        let bpk = filter.bits_per_key();
        assert!(bpk < 12.0, "bits per key too high: {bpk:.2}");
        assert!(bpk > 8.0, "bits per key too low: {bpk:.2}");
    }

    #[test]
    fn size_bytes_reasonable() {
        let keys: Vec<u64> = (0..100).collect();
        let filter = XorFilter::build(&keys).unwrap();
        assert!(filter.size_bytes() > 0);
        assert!(filter.size_bytes() < 300);
    }

    // -- Byte-slice API (8-bit) -------------------------------------------------

    #[test]
    fn from_bytes_api() {
        let keys: Vec<&[u8]> = vec![b"hello", b"world", b"foo", b"bar"];
        let filter = XorFilter::from_bytes(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains_bytes(k));
        }
    }

    #[test]
    fn from_bytes_no_false_negatives() {
        let items: Vec<Vec<u8>> = (0..200).map(|i| format!("item-{i}").into_bytes()).collect();
        let refs: Vec<&[u8]> = items.iter().map(|v| v.as_slice()).collect();
        let filter = XorFilter::from_bytes(&refs).unwrap();
        for item in &items {
            assert!(
                filter.contains_bytes(item),
                "false negative for {:?}",
                String::from_utf8_lossy(item)
            );
        }
    }

    // -- Serde roundtrip --------------------------------------------------------

    #[test]
    fn serde_roundtrip() {
        let keys: Vec<u64> = (0..50).collect();
        let filter = XorFilter::build(&keys).unwrap();
        let json = serde_json::to_string(&filter).unwrap();
        let restored: XorFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.num_keys(), filter.num_keys());
        for &k in &keys {
            assert!(restored.contains(k));
        }
    }

    #[test]
    fn serde_roundtrip_16bit() {
        let keys: Vec<u64> = (0..50).collect();
        let filter = XorFilter16::build(&keys).unwrap();
        let json = serde_json::to_string(&filter).unwrap();
        let restored: XorFilter16 = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.num_keys(), filter.num_keys());
        for &k in &keys {
            assert!(restored.contains(k));
        }
    }

    // -- Display format ---------------------------------------------------------

    #[test]
    fn display_format() {
        let keys: Vec<u64> = (0..10).collect();
        let filter = XorFilter::build(&keys).unwrap();
        let s = format!("{filter}");
        assert!(s.contains("XorFilter"));
        assert!(s.contains("keys=10"));
    }

    #[test]
    fn display_format_16() {
        let keys: Vec<u64> = (0..10).collect();
        let filter = XorFilter16::build(&keys).unwrap();
        let s = format!("{filter}");
        assert!(s.contains("XorFilter16"));
        assert!(s.contains("keys=10"));
    }

    #[test]
    fn debug_format_8() {
        let filter = XorFilter::build(&[1, 2, 3]).unwrap();
        let s = format!("{filter:?}");
        assert!(s.contains("XorFilter"));
    }

    #[test]
    fn debug_format_16() {
        let filter = XorFilter16::build(&[1, 2, 3]).unwrap();
        let s = format!("{filter:?}");
        assert!(s.contains("XorFilter16"));
    }

    // -- XorFilter16 construction -----------------------------------------------

    #[test]
    fn build_16bit_empty() {
        let filter = XorFilter16::build(&[]).unwrap();
        assert!(filter.is_empty());
        assert!(!filter.contains(42));
    }

    #[test]
    fn build_16bit_single() {
        let filter = XorFilter16::build(&[42]).unwrap();
        assert!(filter.contains(42));
        assert_eq!(filter.num_keys(), 1);
    }

    #[test]
    fn no_false_negatives_16bit() {
        let keys: Vec<u64> = (1000..2000).collect();
        let filter = XorFilter16::build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k), "false negative for {k}");
        }
    }

    #[test]
    fn fp_rate_16bit_within_bounds() {
        let n: u64 = 5000;
        let keys: Vec<u64> = (0..n).collect();
        let filter = XorFilter16::build(&keys).unwrap();

        let test_count = 100_000u64;
        let mut fp = 0u64;
        for i in n..(n + test_count) {
            if filter.contains(i) {
                fp += 1;
            }
        }

        let rate = fp as f64 / test_count as f64;
        // 16-bit FP ≈ 1/65535 ≈ 0.000015. Allow generous tolerance.
        assert!(rate < 0.001, "16-bit FP rate {rate:.6} exceeds tolerance");
    }

    #[test]
    fn space_efficiency_16bit() {
        let n = 10_000;
        let keys: Vec<u64> = (0..n).collect();
        let filter = XorFilter16::build(&keys).unwrap();
        let bpk = filter.bits_per_key();
        assert!(bpk < 24.0, "16-bit bpk too high: {bpk:.2}");
        assert!(bpk > 16.0, "16-bit bpk too low: {bpk:.2}");
    }

    #[test]
    fn build_16bit_from_bytes() {
        let keys: Vec<&[u8]> = vec![b"alpha", b"beta", b"gamma"];
        let filter = XorFilter16::from_bytes(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains_bytes(k));
        }
    }

    #[test]
    fn memory_16bit_double_8bit() {
        let keys: Vec<u64> = (0..500).collect();
        let f8 = XorFilter::build(&keys).unwrap();
        let f16 = XorFilter16::build(&keys).unwrap();
        // 16-bit should use roughly 2× the memory of 8-bit
        let ratio = f16.size_bytes() as f64 / f8.size_bytes() as f64;
        assert!(
            (1.8..2.2).contains(&ratio),
            "memory ratio {ratio:.2} not close to 2.0"
        );
    }

    // -- Stats ------------------------------------------------------------------

    #[test]
    fn stats_8bit() {
        let keys: Vec<u64> = (0..1000).collect();
        let filter = XorFilter::build(&keys).unwrap();
        let stats = filter.stats();
        assert_eq!(stats.num_keys, 1000);
        assert_eq!(stats.fingerprint_bits, 8);
        assert!(stats.bits_per_key > 0.0);
        assert!(stats.memory_bytes > 0);
    }

    #[test]
    fn stats_16bit() {
        let keys: Vec<u64> = (0..1000).collect();
        let filter = XorFilter16::build(&keys).unwrap();
        let stats = filter.stats();
        assert_eq!(stats.num_keys, 1000);
        assert_eq!(stats.fingerprint_bits, 16);
        assert!(stats.memory_bytes > 0);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let keys: Vec<u64> = (0..100).collect();
        let filter = XorFilter::build(&keys).unwrap();
        let stats = filter.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: XorFilterStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats.num_keys, back.num_keys);
        assert_eq!(stats.fingerprint_bits, back.fingerprint_bits);
        assert_eq!(stats.table_len, back.table_len);
    }

    // -- try_build (Result API) -------------------------------------------------

    #[test]
    fn try_build_success() {
        let keys: Vec<u64> = (0..100).collect();
        let filter = XorFilter::try_build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k));
        }
    }

    #[test]
    fn try_build_16bit_success() {
        let keys: Vec<u64> = (0..100).collect();
        let filter = XorFilter16::try_build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k));
        }
    }

    // -- Error types ------------------------------------------------------------

    #[test]
    fn error_display() {
        let e = XorFilterError::MaxRetriesExceeded { attempts: 10 };
        let s = format!("{e}");
        assert!(s.contains("10"));
        assert!(s.contains("failed"));
    }

    #[test]
    fn error_is_std_error() {
        let e: Box<dyn std::error::Error> =
            Box::new(XorFilterError::MaxRetriesExceeded { attempts: 5 });
        let s = format!("{e}");
        assert!(s.contains("5"));
    }

    // -- Clone ------------------------------------------------------------------

    #[test]
    fn clone_8bit() {
        let keys: Vec<u64> = (0..100).collect();
        let filter = XorFilter::build(&keys).unwrap();
        let cloned = filter.clone();
        for &k in &keys {
            assert_eq!(filter.contains(k), cloned.contains(k));
        }
    }

    #[test]
    fn clone_16bit() {
        let keys: Vec<u64> = (0..100).collect();
        let filter = XorFilter16::build(&keys).unwrap();
        let cloned = filter.clone();
        for &k in &keys {
            assert_eq!(filter.contains(k), cloned.contains(k));
        }
    }

    // -- Determinism ------------------------------------------------------------

    #[test]
    fn construction_deterministic_8bit() {
        let keys: Vec<u64> = (0..500).collect();
        let f1 = XorFilter::build(&keys).unwrap();
        let f2 = XorFilter::build(&keys).unwrap();
        assert_eq!(f1.seed, f2.seed);
        assert_eq!(f1.fingerprints, f2.fingerprints);
    }

    #[test]
    fn construction_deterministic_16bit() {
        let keys: Vec<u64> = (0..500).collect();
        let f1 = XorFilter16::build(&keys).unwrap();
        let f2 = XorFilter16::build(&keys).unwrap();
        assert_eq!(f1.seed, f2.seed);
        assert_eq!(f1.fingerprints, f2.fingerprints);
    }

    // -- Edge cases -------------------------------------------------------------

    #[test]
    fn bits_per_key_zero_keys() {
        let filter = XorFilter::build(&[]).unwrap();
        assert!((filter.bits_per_key() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn bits_per_key_16_zero_keys() {
        let filter = XorFilter16::build(&[]).unwrap();
        assert!((filter.bits_per_key() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn empty_contains_nothing() {
        let filter = XorFilter::build(&[]).unwrap();
        for i in 0..100 {
            assert!(!filter.contains(i));
        }
    }

    #[test]
    fn hash_deterministic() {
        assert_eq!(murmur_mix(42), murmur_mix(42));
        assert_ne!(murmur_mix(0), murmur_mix(1));
    }

    #[test]
    fn fnv1a_deterministic() {
        assert_eq!(fnv1a_hash(b"hello"), fnv1a_hash(b"hello"));
        assert_ne!(fnv1a_hash(b"hello"), fnv1a_hash(b"world"));
    }

    // -- 16-bit duplicate handling ----------------------------------------------

    #[test]
    fn duplicate_keys_16bit() {
        let keys = vec![10u64, 20, 30, 10, 20];
        let filter = XorFilter16::build(&keys).unwrap();
        assert_eq!(filter.num_keys(), 3);
        assert!(filter.contains(10));
        assert!(filter.contains(20));
        assert!(filter.contains(30));
    }

    // -- Large key values -------------------------------------------------------

    #[test]
    fn large_key_values() {
        let keys = vec![u64::MAX, u64::MAX - 1, u64::MAX - 2];
        let filter = XorFilter::build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k));
        }
    }

    #[test]
    fn large_key_values_16bit() {
        let keys = vec![u64::MAX, u64::MAX - 1, u64::MAX - 2];
        let filter = XorFilter16::build(&keys).unwrap();
        for &k in &keys {
            assert!(filter.contains(k));
        }
    }
}
