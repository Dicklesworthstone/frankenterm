//! Bloom filter for probabilistic set membership.
//!
//! A space-efficient probabilistic data structure that answers "is this element
//! in the set?" with either "definitely no" or "probably yes" (with a tunable
//! false-positive rate). No false negatives.
//!
//! # Use cases in FrankenTerm
//!
//! - **Ingest fast-path**: skip full SHA-256 hash when content is definitely new.
//! - **Seen-pane tracking**: quick check if a pane ID has been observed before.
//! - **Duplicate detection**: fast pre-filter before expensive dedup operations.
//!
//! # Counting variant
//!
//! The [`CountingBloomFilter`] uses 4-bit counters instead of single bits,
//! supporting element removal at the cost of 4× memory.
//!
//! # Sizing
//!
//! Given desired capacity `n` and false-positive rate `fp`:
//!
//! - Bits needed: `m = -n × ln(fp) / (ln2)²`
//! - Hash functions: `k = (m / n) × ln2`

use serde::{Deserialize, Serialize};

// =============================================================================
// Hash helpers (double hashing scheme)
// =============================================================================

/// FNV-1a 64-bit hash.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A second independent hash using a different seed (DJB2 variant).
fn djb2(data: &[u8]) -> u64 {
    let mut h: u64 = 5381;
    for &b in data {
        h = h.wrapping_shl(5).wrapping_add(h).wrapping_add(b as u64);
    }
    h
}

/// Generate `k` hash indices using double hashing: h(i) = (h1 + i*h2) mod m.
fn hash_indices(data: &[u8], k: u32, m: usize) -> Vec<usize> {
    let h1 = fnv1a(data);
    let h2 = djb2(data);
    (0..k)
        .map(|i| {
            let combined = h1.wrapping_add((i as u64).wrapping_mul(h2));
            (combined % m as u64) as usize
        })
        .collect()
}

// =============================================================================
// BloomFilter (standard, single-bit)
// =============================================================================

/// A standard Bloom filter using single-bit buckets.
///
/// Supports `insert` and `contains` but NOT removal. For removal support,
/// use [`CountingBloomFilter`].
///
/// # Example
///
/// ```ignore
/// let mut bf = BloomFilter::with_capacity(1000, 0.01);
/// bf.insert(b"hello");
/// assert!(bf.contains(b"hello"));     // true (definitely inserted)
/// assert!(!bf.contains(b"missing"));  // false (definitely not inserted)
/// ```
#[derive(Clone)]
pub struct BloomFilter {
    bits: Vec<u64>,
    num_bits: usize,
    num_hashes: u32,
    count: usize,
}

impl BloomFilter {
    /// Create a Bloom filter sized for `capacity` items with the given
    /// false-positive rate (e.g., 0.01 for 1%).
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0 or `fp_rate` is not in (0, 1).
    #[must_use]
    pub fn with_capacity(capacity: usize, fp_rate: f64) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        assert!(fp_rate > 0.0 && fp_rate < 1.0, "fp_rate must be in (0, 1)");

        let num_bits = optimal_num_bits(capacity, fp_rate);
        let num_hashes = optimal_num_hashes(num_bits, capacity);
        let words = (num_bits + 63) / 64;

        Self {
            bits: vec![0u64; words],
            num_bits,
            num_hashes,
            count: 0,
        }
    }

    /// Create a Bloom filter with explicit parameters.
    #[must_use]
    pub fn with_params(num_bits: usize, num_hashes: u32) -> Self {
        assert!(num_bits > 0, "num_bits must be > 0");
        assert!(num_hashes > 0, "num_hashes must be > 0");
        let words = (num_bits + 63) / 64;
        Self {
            bits: vec![0u64; words],
            num_bits,
            num_hashes,
            count: 0,
        }
    }

    /// Insert an element.
    pub fn insert(&mut self, data: &[u8]) {
        let indices = hash_indices(data, self.num_hashes, self.num_bits);
        for idx in indices {
            let word = idx / 64;
            let bit = idx % 64;
            self.bits[word] |= 1u64 << bit;
        }
        self.count += 1;
    }

    /// Check if an element is (probably) in the set.
    ///
    /// Returns `false` if the element is definitely NOT in the set.
    /// Returns `true` if the element is probably in the set (subject to
    /// the configured false-positive rate).
    #[must_use]
    pub fn contains(&self, data: &[u8]) -> bool {
        let indices = hash_indices(data, self.num_hashes, self.num_bits);
        indices.iter().all(|&idx| {
            let word = idx / 64;
            let bit = idx % 64;
            (self.bits[word] >> bit) & 1 == 1
        })
    }

    /// Number of elements inserted.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Number of bits in the filter.
    #[must_use]
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    /// Number of hash functions used.
    #[must_use]
    pub fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    /// Estimated current false-positive rate based on the number of set bits.
    #[must_use]
    pub fn estimated_fp_rate(&self) -> f64 {
        let set_bits = self
            .bits
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum::<usize>();
        let fraction = set_bits as f64 / self.num_bits as f64;
        fraction.powi(self.num_hashes as i32)
    }

    /// Memory usage in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.bits.len() * 8
    }

    /// Clear all bits and reset the count.
    pub fn clear(&mut self) {
        self.bits.iter_mut().for_each(|w| *w = 0);
        self.count = 0;
    }

    /// Merge another Bloom filter into this one (union).
    ///
    /// Both filters must have the same parameters (num_bits, num_hashes).
    ///
    /// # Panics
    ///
    /// Panics if the filters have different parameters.
    pub fn union(&mut self, other: &BloomFilter) {
        assert_eq!(self.num_bits, other.num_bits, "num_bits mismatch");
        assert_eq!(self.num_hashes, other.num_hashes, "num_hashes mismatch");
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a |= *b;
        }
        // count is now an upper bound
        self.count += other.count;
    }
}

impl std::fmt::Debug for BloomFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BloomFilter")
            .field("num_bits", &self.num_bits)
            .field("num_hashes", &self.num_hashes)
            .field("count", &self.count)
            .field("memory_bytes", &self.memory_bytes())
            .finish()
    }
}

// =============================================================================
// CountingBloomFilter (4-bit counters, supports removal)
// =============================================================================

/// A counting Bloom filter using 4-bit counters per bucket.
///
/// Supports both `insert` and `remove`, at the cost of 4× the memory of
/// a standard Bloom filter. Counter saturation at 15 prevents overflow.
///
/// # Example
///
/// ```ignore
/// let mut cbf = CountingBloomFilter::with_capacity(1000, 0.01);
/// cbf.insert(b"hello");
/// assert!(cbf.contains(b"hello"));
/// cbf.remove(b"hello");
/// assert!(!cbf.contains(b"hello"));
/// ```
#[derive(Clone)]
pub struct CountingBloomFilter {
    /// Packed 4-bit counters: 16 counters per u64.
    counters: Vec<u64>,
    num_buckets: usize,
    num_hashes: u32,
    count: usize,
}

impl CountingBloomFilter {
    /// Create a counting Bloom filter sized for `capacity` items with the given
    /// false-positive rate.
    #[must_use]
    pub fn with_capacity(capacity: usize, fp_rate: f64) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        assert!(fp_rate > 0.0 && fp_rate < 1.0, "fp_rate must be in (0, 1)");

        let num_buckets = optimal_num_bits(capacity, fp_rate);
        let num_hashes = optimal_num_hashes(num_buckets, capacity);
        // 16 counters per u64 (4 bits each).
        let words = (num_buckets + 15) / 16;

        Self {
            counters: vec![0u64; words],
            num_buckets,
            num_hashes,
            count: 0,
        }
    }

    /// Get the 4-bit counter value at the given bucket index.
    fn get_counter(&self, idx: usize) -> u8 {
        let word = idx / 16;
        let nibble = (idx % 16) * 4;
        ((self.counters[word] >> nibble) & 0xF) as u8
    }

    /// Increment the 4-bit counter at the given bucket index (saturates at 15).
    fn increment_counter(&mut self, idx: usize) {
        let word = idx / 16;
        let nibble = (idx % 16) * 4;
        let current = (self.counters[word] >> nibble) & 0xF;
        if current < 15 {
            // Clear the nibble and set the incremented value.
            self.counters[word] &= !(0xFu64 << nibble);
            self.counters[word] |= (current + 1) << nibble;
        }
    }

    /// Decrement the 4-bit counter at the given bucket index (floors at 0).
    fn decrement_counter(&mut self, idx: usize) {
        let word = idx / 16;
        let nibble = (idx % 16) * 4;
        let current = (self.counters[word] >> nibble) & 0xF;
        if current > 0 {
            self.counters[word] &= !(0xFu64 << nibble);
            self.counters[word] |= (current - 1) << nibble;
        }
    }

    /// Insert an element.
    pub fn insert(&mut self, data: &[u8]) {
        let indices = hash_indices(data, self.num_hashes, self.num_buckets);
        for idx in indices {
            self.increment_counter(idx);
        }
        self.count += 1;
    }

    /// Remove an element.
    ///
    /// Only call this if the element was previously inserted. Removing
    /// elements that were never inserted can cause false negatives.
    pub fn remove(&mut self, data: &[u8]) {
        let indices = hash_indices(data, self.num_hashes, self.num_buckets);
        for idx in indices {
            self.decrement_counter(idx);
        }
        self.count = self.count.saturating_sub(1);
    }

    /// Check if an element is (probably) in the set.
    #[must_use]
    pub fn contains(&self, data: &[u8]) -> bool {
        let indices = hash_indices(data, self.num_hashes, self.num_buckets);
        indices.iter().all(|&idx| self.get_counter(idx) > 0)
    }

    /// Number of elements inserted (minus removals).
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Number of buckets in the filter.
    #[must_use]
    pub fn num_buckets(&self) -> usize {
        self.num_buckets
    }

    /// Number of hash functions.
    #[must_use]
    pub fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    /// Memory usage in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.counters.len() * 8
    }

    /// Clear all counters and reset the count.
    pub fn clear(&mut self) {
        self.counters.iter_mut().for_each(|w| *w = 0);
        self.count = 0;
    }
}

impl std::fmt::Debug for CountingBloomFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingBloomFilter")
            .field("num_buckets", &self.num_buckets)
            .field("num_hashes", &self.num_hashes)
            .field("count", &self.count)
            .field("memory_bytes", &self.memory_bytes())
            .finish()
    }
}

// =============================================================================
// BloomStats (serializable summary)
// =============================================================================

/// Serializable statistics about a Bloom filter's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomStats {
    /// Number of elements inserted.
    pub count: usize,
    /// Number of bits (or buckets) in the filter.
    pub num_bits: usize,
    /// Number of hash functions.
    pub num_hashes: u32,
    /// Memory usage in bytes.
    pub memory_bytes: usize,
    /// Estimated current false-positive rate.
    pub estimated_fp_rate: f64,
    /// Fill ratio (fraction of bits set).
    pub fill_ratio: f64,
}

impl BloomFilter {
    /// Get statistics about the current state of the filter.
    #[must_use]
    pub fn stats(&self) -> BloomStats {
        let set_bits = self
            .bits
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum::<usize>();
        BloomStats {
            count: self.count,
            num_bits: self.num_bits,
            num_hashes: self.num_hashes,
            memory_bytes: self.memory_bytes(),
            estimated_fp_rate: self.estimated_fp_rate(),
            fill_ratio: set_bits as f64 / self.num_bits as f64,
        }
    }
}

// =============================================================================
// Sizing helpers
// =============================================================================

/// Optimal number of bits for the given capacity and false-positive rate.
///
/// Formula: m = -(n × ln(fp)) / (ln2)²
#[must_use]
pub fn optimal_num_bits(capacity: usize, fp_rate: f64) -> usize {
    let ln2_sq = std::f64::consts::LN_2 * std::f64::consts::LN_2;
    let m = -(capacity as f64 * fp_rate.ln()) / ln2_sq;
    m.ceil() as usize
}

/// Optimal number of hash functions for the given bit count and capacity.
///
/// Formula: k = (m / n) × ln2
#[must_use]
pub fn optimal_num_hashes(num_bits: usize, capacity: usize) -> u32 {
    let k = (num_bits as f64 / capacity as f64) * std::f64::consts::LN_2;
    let k = k.round() as u32;
    k.max(1) // at least 1 hash function
}

/// Theoretical false-positive rate for the given parameters.
///
/// Formula: fp = (1 - e^(-kn/m))^k
#[must_use]
pub fn theoretical_fp_rate(num_bits: usize, num_hashes: u32, count: usize) -> f64 {
    let exp = (-(num_hashes as f64) * count as f64 / num_bits as f64).exp();
    (1.0 - exp).powi(num_hashes as i32)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Sizing helpers ---------------------------------------------------------

    #[test]
    fn optimal_bits_reasonable() {
        let bits = optimal_num_bits(1000, 0.01);
        // For 1000 items at 1% FP, should be ~9585 bits.
        assert!(bits > 9000 && bits < 10000, "bits = {bits}");
    }

    #[test]
    fn optimal_hashes_reasonable() {
        let bits = optimal_num_bits(1000, 0.01);
        let k = optimal_num_hashes(bits, 1000);
        // Should be ~7 hash functions.
        assert!(k >= 6 && k <= 8, "k = {k}");
    }

    #[test]
    fn theoretical_fp_rate_empty() {
        let fp = theoretical_fp_rate(1000, 7, 0);
        assert!((fp - 0.0).abs() < 1e-10);
    }

    #[test]
    fn theoretical_fp_rate_at_capacity() {
        let bits = optimal_num_bits(1000, 0.01);
        let k = optimal_num_hashes(bits, 1000);
        let fp = theoretical_fp_rate(bits, k, 1000);
        // Should be close to 1%.
        assert!(fp < 0.02, "fp = {fp}");
    }

    // -- BloomFilter basic ------------------------------------------------------

    #[test]
    fn insert_and_contains() {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        bf.insert(b"hello");
        bf.insert(b"world");
        assert!(bf.contains(b"hello"));
        assert!(bf.contains(b"world"));
        assert_eq!(bf.count(), 2);
    }

    #[test]
    fn does_not_contain_missing() {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        bf.insert(b"hello");
        assert!(!bf.contains(b"world"));
    }

    #[test]
    fn empty_filter_contains_nothing() {
        let bf = BloomFilter::with_capacity(100, 0.01);
        assert!(!bf.contains(b"anything"));
        assert_eq!(bf.count(), 0);
    }

    #[test]
    fn clear_resets() {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        bf.insert(b"data");
        assert!(bf.contains(b"data"));
        bf.clear();
        assert!(!bf.contains(b"data"));
        assert_eq!(bf.count(), 0);
    }

    #[test]
    fn many_inserts_no_false_negatives() {
        let n = 500;
        let mut bf = BloomFilter::with_capacity(n, 0.01);
        let items: Vec<Vec<u8>> = (0..n).map(|i| format!("item-{i}").into_bytes()).collect();

        for item in &items {
            bf.insert(item);
        }

        // No false negatives.
        for item in &items {
            assert!(bf.contains(item), "false negative for {:?}", item);
        }
    }

    #[test]
    fn fp_rate_within_bounds() {
        let n = 1000;
        let target_fp = 0.05;
        let mut bf = BloomFilter::with_capacity(n, target_fp);

        for i in 0..n {
            bf.insert(format!("in-{i}").as_bytes());
        }

        // Test 10000 items that were NOT inserted.
        let mut false_positives = 0;
        let test_count = 10_000;
        for i in 0..test_count {
            if bf.contains(format!("out-{i}").as_bytes()) {
                false_positives += 1;
            }
        }

        let observed_fp = false_positives as f64 / test_count as f64;
        // Allow 3× the target rate as tolerance.
        assert!(
            observed_fp < target_fp * 3.0,
            "observed FP rate {observed_fp} exceeds 3× target {target_fp}"
        );
    }

    #[test]
    fn debug_format() {
        let bf = BloomFilter::with_capacity(100, 0.01);
        let s = format!("{bf:?}");
        assert!(s.contains("BloomFilter"));
        assert!(s.contains("num_bits"));
    }

    #[test]
    fn with_params() {
        let bf = BloomFilter::with_params(1024, 5);
        assert_eq!(bf.num_bits(), 1024);
        assert_eq!(bf.num_hashes(), 5);
    }

    // -- BloomFilter stats ------------------------------------------------------

    #[test]
    fn stats_empty() {
        let bf = BloomFilter::with_capacity(100, 0.01);
        let s = bf.stats();
        assert_eq!(s.count, 0);
        assert!((s.fill_ratio - 0.0).abs() < 1e-10);
        assert!((s.estimated_fp_rate - 0.0).abs() < 1e-10);
    }

    #[test]
    fn stats_after_inserts() {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        for i in 0..50 {
            bf.insert(format!("item-{i}").as_bytes());
        }
        let s = bf.stats();
        assert_eq!(s.count, 50);
        assert!(s.fill_ratio > 0.0);
        assert!(s.estimated_fp_rate > 0.0);
        assert!(s.memory_bytes > 0);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        bf.insert(b"test");
        let s = bf.stats();
        let json = serde_json::to_string(&s).unwrap();
        let back: BloomStats = serde_json::from_str(&json).unwrap();
        assert_eq!(s.count, back.count);
        assert_eq!(s.num_bits, back.num_bits);
    }

    // -- BloomFilter union ------------------------------------------------------

    #[test]
    fn union_merges_sets() {
        let mut a = BloomFilter::with_params(1024, 5);
        let mut b = BloomFilter::with_params(1024, 5);

        a.insert(b"only-a");
        b.insert(b"only-b");

        a.union(&b);

        assert!(a.contains(b"only-a"));
        assert!(a.contains(b"only-b"));
    }

    #[test]
    #[should_panic(expected = "num_bits mismatch")]
    fn union_panics_on_mismatch() {
        let mut a = BloomFilter::with_params(1024, 5);
        let b = BloomFilter::with_params(2048, 5);
        a.union(&b);
    }

    // -- CountingBloomFilter basic -----------------------------------------------

    #[test]
    fn counting_insert_and_contains() {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);
        cbf.insert(b"hello");
        assert!(cbf.contains(b"hello"));
        assert!(!cbf.contains(b"world"));
        assert_eq!(cbf.count(), 1);
    }

    #[test]
    fn counting_remove() {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);
        cbf.insert(b"hello");
        assert!(cbf.contains(b"hello"));

        cbf.remove(b"hello");
        assert!(!cbf.contains(b"hello"));
        assert_eq!(cbf.count(), 0);
    }

    #[test]
    fn counting_remove_preserves_others() {
        let mut cbf = CountingBloomFilter::with_capacity(1000, 0.001);
        cbf.insert(b"alpha");
        cbf.insert(b"beta");
        cbf.insert(b"gamma");

        cbf.remove(b"beta");

        assert!(cbf.contains(b"alpha"));
        assert!(!cbf.contains(b"beta"));
        assert!(cbf.contains(b"gamma"));
    }

    #[test]
    fn counting_multiple_inserts() {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);
        cbf.insert(b"hello");
        cbf.insert(b"hello");
        cbf.insert(b"hello");

        // Remove once — still present (counter > 0).
        cbf.remove(b"hello");
        assert!(cbf.contains(b"hello"));

        // Remove twice more — gone.
        cbf.remove(b"hello");
        cbf.remove(b"hello");
        assert!(!cbf.contains(b"hello"));
    }

    #[test]
    fn counting_clear() {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);
        cbf.insert(b"data");
        cbf.clear();
        assert!(!cbf.contains(b"data"));
        assert_eq!(cbf.count(), 0);
    }

    #[test]
    fn counting_counter_saturation() {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);
        // Insert 20 times — counters should saturate at 15.
        for _ in 0..20 {
            cbf.insert(b"saturate");
        }
        assert!(cbf.contains(b"saturate"));

        // Verify we can still query other items.
        assert!(!cbf.contains(b"other"));
    }

    #[test]
    fn counting_debug_format() {
        let cbf = CountingBloomFilter::with_capacity(100, 0.01);
        let s = format!("{cbf:?}");
        assert!(s.contains("CountingBloomFilter"));
    }

    #[test]
    fn counting_memory() {
        let cbf = CountingBloomFilter::with_capacity(1000, 0.01);
        // 4-bit counters → 4× the bits, packed into u64s (16 per word).
        assert!(cbf.memory_bytes() > 0);
    }

    #[test]
    fn counting_no_false_negatives() {
        let n = 200;
        let mut cbf = CountingBloomFilter::with_capacity(n, 0.01);
        let items: Vec<Vec<u8>> = (0..n).map(|i| format!("item-{i}").into_bytes()).collect();

        for item in &items {
            cbf.insert(item);
        }

        for item in &items {
            assert!(cbf.contains(item), "false negative for {:?}", item);
        }
    }

    // -- Hash function tests ----------------------------------------------------

    #[test]
    fn fnv1a_deterministic() {
        assert_eq!(fnv1a(b"hello"), fnv1a(b"hello"));
        assert_ne!(fnv1a(b"hello"), fnv1a(b"world"));
    }

    #[test]
    fn djb2_deterministic() {
        assert_eq!(djb2(b"hello"), djb2(b"hello"));
        assert_ne!(djb2(b"hello"), djb2(b"world"));
    }

    #[test]
    fn hash_indices_deterministic() {
        let i1 = hash_indices(b"test", 7, 1000);
        let i2 = hash_indices(b"test", 7, 1000);
        assert_eq!(i1, i2);
    }

    #[test]
    fn hash_indices_in_range() {
        let indices = hash_indices(b"test", 10, 500);
        assert_eq!(indices.len(), 10);
        for &idx in &indices {
            assert!(idx < 500, "index {idx} out of range");
        }
    }
}
