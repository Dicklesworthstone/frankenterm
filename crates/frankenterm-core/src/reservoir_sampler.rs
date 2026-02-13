//! Reservoir sampling for bounded-memory stream sampling.
//!
//! When processing high-throughput streams (pane output, events, metrics), we often
//! need a representative random sample without storing everything. Reservoir sampling
//! maintains a fixed-size sample that is provably uniform over the entire stream seen
//! so far, using O(k) memory for a sample of size k.
//!
//! # Algorithm R (Vitter, 1985)
//!
//! For the first k items: store all of them.
//! For item i (i > k): with probability k/i, replace a random element.
//!
//! This guarantees each item in the stream has equal probability k/n of being in the
//! final sample, where n is the total number of items seen.
//!
//! # Weighted variant
//!
//! [`WeightedReservoir`] uses the A-ES algorithm (Efraimidis & Spirakis, 2006):
//! assign each item a key = random^(1/weight) and keep the top-k keys.
//! Items with higher weight are proportionally more likely to be sampled.
//!
//! # Use cases in FrankenTerm
//!
//! - **Telemetry downsampling**: Keep representative metrics from thousands of snapshots.
//! - **Output sampling**: Maintain a bounded sample of pane output lines for analysis.
//! - **Event sampling**: Sample from high-frequency event streams for pattern detection.

use serde::{Deserialize, Serialize};

// =============================================================================
// ReservoirSampler (Algorithm R)
// =============================================================================

/// A reservoir sampler implementing Algorithm R for uniform random sampling.
///
/// Maintains a sample of at most `capacity` items, where each item in the
/// stream has equal probability of being included in the final sample.
///
/// # Example
///
/// ```ignore
/// let mut rs = ReservoirSampler::new(10); // keep 10 items
/// for i in 0..1_000_000 {
///     rs.observe(i);
/// }
/// let sample = rs.sample(); // exactly 10 uniformly random items
/// ```
pub struct ReservoirSampler<T> {
    reservoir: Vec<T>,
    capacity: usize,
    seen: u64,
    /// Simple LCG state for deterministic pseudo-random numbers.
    rng_state: u64,
}

impl<T> ReservoirSampler<T> {
    /// Create a new reservoir sampler with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            reservoir: Vec::with_capacity(capacity),
            capacity,
            seen: 0,
            rng_state: 0x5DEE_CE66_D1A4_F87D, // arbitrary seed
        }
    }

    /// Create a new reservoir sampler with a specific RNG seed.
    #[must_use]
    pub fn with_seed(capacity: usize, seed: u64) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            reservoir: Vec::with_capacity(capacity),
            capacity,
            seen: 0,
            rng_state: seed,
        }
    }

    /// Observe a new item from the stream.
    ///
    /// The item may or may not be added to the reservoir based on the
    /// Algorithm R probability.
    pub fn observe(&mut self, item: T) {
        self.seen += 1;

        if self.reservoir.len() < self.capacity {
            // Fill phase: always add.
            self.reservoir.push(item);
        } else {
            // Replace phase: with probability capacity/seen.
            let j = self.next_u64() % self.seen;
            if j < self.capacity as u64 {
                self.reservoir[j as usize] = item;
            }
        }
    }

    /// Get the current sample as a slice.
    #[must_use]
    pub fn sample(&self) -> &[T] {
        &self.reservoir
    }

    /// Consume the sampler and return the sample.
    #[must_use]
    pub fn into_sample(self) -> Vec<T> {
        self.reservoir
    }

    /// Number of items observed so far.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Current number of items in the reservoir.
    #[must_use]
    pub fn len(&self) -> usize {
        self.reservoir.len()
    }

    /// Whether the reservoir is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reservoir.is_empty()
    }

    /// Maximum capacity of the reservoir.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clear the reservoir and reset the seen count.
    pub fn clear(&mut self) {
        self.reservoir.clear();
        self.seen = 0;
    }

    /// SplitMix64 pseudo-random number generator.
    fn next_u64(&mut self) -> u64 {
        self.rng_state = self.rng_state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ReservoirSampler<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReservoirSampler")
            .field("capacity", &self.capacity)
            .field("seen", &self.seen)
            .field("len", &self.reservoir.len())
            .finish()
    }
}

// =============================================================================
// WeightedReservoir (A-ES algorithm)
// =============================================================================

/// A weighted reservoir sampler using the A-ES algorithm.
///
/// Items with higher weight are proportionally more likely to be included
/// in the final sample. Useful when some items are more "important" than
/// others (e.g., high-entropy output lines, error events).
///
/// # Algorithm
///
/// Each item gets a key = u^(1/weight), where u is uniform(0,1).
/// The reservoir keeps the k items with the highest keys.
pub struct WeightedReservoir<T> {
    /// (key, item) pairs, sorted by key (min-heap by key for efficient replacement).
    items: Vec<(f64, T)>,
    capacity: usize,
    seen: u64,
    rng_state: u64,
}

impl<T> WeightedReservoir<T> {
    /// Create a new weighted reservoir with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            items: Vec::with_capacity(capacity),
            capacity,
            seen: 0,
            rng_state: 0xCAFE_BABE_DEAD_BEEF,
        }
    }

    /// Create a new weighted reservoir with a specific RNG seed.
    #[must_use]
    pub fn with_seed(capacity: usize, seed: u64) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            items: Vec::with_capacity(capacity),
            capacity,
            seen: 0,
            rng_state: seed,
        }
    }

    /// Observe a new item with the given weight.
    ///
    /// Weight must be positive. Higher weight = higher probability of inclusion.
    pub fn observe(&mut self, item: T, weight: f64) {
        assert!(weight > 0.0, "weight must be positive");
        self.seen += 1;

        // key = u^(1/weight) where u is uniform(0,1).
        let u = self.next_f64();
        let key = u.powf(1.0 / weight);

        if self.items.len() < self.capacity {
            self.items.push((key, item));
            // Keep sorted by key (ascending) so index 0 is the minimum.
            self.items
                .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        } else if key > self.items[0].0 {
            // Replace the minimum-key item.
            self.items[0] = (key, item);
            self.items
                .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        }
    }

    /// Get the current sample as references.
    #[must_use]
    pub fn sample(&self) -> Vec<&T> {
        self.items.iter().map(|(_, item)| item).collect()
    }

    /// Consume the sampler and return the sample items (without keys).
    #[must_use]
    pub fn into_sample(self) -> Vec<T> {
        self.items.into_iter().map(|(_, item)| item).collect()
    }

    /// Number of items observed.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Current reservoir size.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the reservoir is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Maximum capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clear the reservoir.
    pub fn clear(&mut self) {
        self.items.clear();
        self.seen = 0;
    }

    /// SplitMix64 pseudo-random number generator.
    fn next_u64(&mut self) -> u64 {
        self.rng_state = self.rng_state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Generate a uniform f64 in (0, 1).
    fn next_f64(&mut self) -> f64 {
        let v = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        // Clamp away from 0 to avoid log(0) issues in powf.
        if v <= 0.0 { 1e-15 } else { v }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for WeightedReservoir<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeightedReservoir")
            .field("capacity", &self.capacity)
            .field("seen", &self.seen)
            .field("len", &self.items.len())
            .finish()
    }
}

// =============================================================================
// SamplerStats (serializable summary)
// =============================================================================

/// Serializable statistics about a reservoir sampler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplerStats {
    /// Maximum sample size.
    pub capacity: usize,
    /// Current number of items in the reservoir.
    pub current_size: usize,
    /// Total items observed.
    pub total_seen: u64,
    /// Sampling rate: capacity / total_seen (when seen > capacity).
    pub sampling_rate: f64,
}

impl<T> ReservoirSampler<T> {
    /// Get sampling statistics.
    #[must_use]
    pub fn stats(&self) -> SamplerStats {
        SamplerStats {
            capacity: self.capacity,
            current_size: self.reservoir.len(),
            total_seen: self.seen,
            sampling_rate: if self.seen > self.capacity as u64 {
                self.capacity as f64 / self.seen as f64
            } else {
                1.0
            },
        }
    }
}

impl<T> WeightedReservoir<T> {
    /// Get sampling statistics.
    #[must_use]
    pub fn stats(&self) -> SamplerStats {
        SamplerStats {
            capacity: self.capacity,
            current_size: self.items.len(),
            total_seen: self.seen,
            sampling_rate: if self.seen > self.capacity as u64 {
                self.capacity as f64 / self.seen as f64
            } else {
                1.0
            },
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- ReservoirSampler basic -------------------------------------------------

    #[test]
    fn empty_sampler() {
        let rs: ReservoirSampler<i32> = ReservoirSampler::new(10);
        assert!(rs.is_empty());
        assert_eq!(rs.len(), 0);
        assert_eq!(rs.seen(), 0);
        assert_eq!(rs.capacity(), 10);
    }

    #[test]
    fn fill_phase() {
        let mut rs = ReservoirSampler::new(5);
        for i in 0..5 {
            rs.observe(i);
        }
        assert_eq!(rs.len(), 5);
        assert_eq!(rs.seen(), 5);
        // All items should be present.
        let sample = rs.sample();
        for i in 0..5 {
            assert!(sample.contains(&i), "missing {i}");
        }
    }

    #[test]
    fn never_exceeds_capacity() {
        let mut rs = ReservoirSampler::new(10);
        for i in 0..10_000 {
            rs.observe(i);
        }
        assert_eq!(rs.len(), 10);
        assert_eq!(rs.seen(), 10_000);
    }

    #[test]
    fn deterministic_with_seed() {
        let mut rs1 = ReservoirSampler::with_seed(5, 42);
        let mut rs2 = ReservoirSampler::with_seed(5, 42);

        for i in 0..100 {
            rs1.observe(i);
            rs2.observe(i);
        }

        assert_eq!(rs1.sample(), rs2.sample());
    }

    #[test]
    fn different_seeds_different_samples() {
        let mut rs1 = ReservoirSampler::with_seed(5, 1);
        let mut rs2 = ReservoirSampler::with_seed(5, 2);

        for i in 0..1000 {
            rs1.observe(i);
            rs2.observe(i);
        }

        // Very unlikely to be identical with different seeds.
        assert_ne!(rs1.sample(), rs2.sample());
    }

    #[test]
    fn clear_resets() {
        let mut rs = ReservoirSampler::new(5);
        for i in 0..10 {
            rs.observe(i);
        }
        rs.clear();
        assert!(rs.is_empty());
        assert_eq!(rs.seen(), 0);
    }

    #[test]
    fn into_sample() {
        let mut rs = ReservoirSampler::new(3);
        rs.observe(10);
        rs.observe(20);
        rs.observe(30);
        let sample = rs.into_sample();
        assert_eq!(sample.len(), 3);
    }

    #[test]
    fn debug_format() {
        let rs: ReservoirSampler<i32> = ReservoirSampler::new(5);
        let s = format!("{rs:?}");
        assert!(s.contains("ReservoirSampler"));
        assert!(s.contains("capacity"));
    }

    // -- Uniform distribution test ----------------------------------------------

    #[test]
    fn approximate_uniformity() {
        // Run many trials and check each position is roughly equally likely.
        let n = 100;
        let k = 10;
        let trials = 50_000;
        let mut counts = vec![0u64; n];

        for seed in 0..trials {
            let mut rs = ReservoirSampler::with_seed(k, seed);
            for i in 0..n {
                rs.observe(i);
            }
            for &item in rs.sample() {
                counts[item] += 1;
            }
        }

        // Expected count per item = trials * k / n = 50000 * 10 / 100 = 5000.
        let expected = (trials as f64 * k as f64) / n as f64;
        for (i, &c) in counts.iter().enumerate() {
            let ratio = c as f64 / expected;
            // Allow 20% deviation (statistical test).
            assert!(
                ratio > 0.8 && ratio < 1.2,
                "item {i}: count={c}, expected={expected}, ratio={ratio}"
            );
        }
    }

    // -- Stats ------------------------------------------------------------------

    #[test]
    fn stats_during_fill() {
        let mut rs = ReservoirSampler::new(10);
        for i in 0..5 {
            rs.observe(i);
        }
        let s = rs.stats();
        assert_eq!(s.capacity, 10);
        assert_eq!(s.current_size, 5);
        assert_eq!(s.total_seen, 5);
        assert!((s.sampling_rate - 1.0).abs() < 1e-10);
    }

    #[test]
    fn stats_after_fill() {
        let mut rs = ReservoirSampler::new(10);
        for i in 0..1000 {
            rs.observe(i);
        }
        let s = rs.stats();
        assert_eq!(s.current_size, 10);
        assert_eq!(s.total_seen, 1000);
        assert!((s.sampling_rate - 0.01).abs() < 1e-10);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let s = SamplerStats {
            capacity: 10,
            current_size: 10,
            total_seen: 1000,
            sampling_rate: 0.01,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SamplerStats = serde_json::from_str(&json).unwrap();
        assert_eq!(s.capacity, back.capacity);
        assert_eq!(s.total_seen, back.total_seen);
    }

    // -- WeightedReservoir basic ------------------------------------------------

    #[test]
    fn weighted_empty() {
        let wr: WeightedReservoir<i32> = WeightedReservoir::new(5);
        assert!(wr.is_empty());
        assert_eq!(wr.seen(), 0);
    }

    #[test]
    fn weighted_fill() {
        let mut wr = WeightedReservoir::new(3);
        wr.observe(10, 1.0);
        wr.observe(20, 1.0);
        wr.observe(30, 1.0);
        assert_eq!(wr.len(), 3);
        assert_eq!(wr.seen(), 3);
    }

    #[test]
    fn weighted_never_exceeds_capacity() {
        let mut wr = WeightedReservoir::new(5);
        for i in 0..1000 {
            wr.observe(i, 1.0);
        }
        assert_eq!(wr.len(), 5);
        assert_eq!(wr.seen(), 1000);
    }

    #[test]
    fn weighted_high_weight_preferred() {
        // With extremely skewed weights, the high-weight item should almost
        // always appear in the sample.
        let mut found_heavy = 0;
        let trials = 1000;

        for seed in 0..trials {
            let mut wr = WeightedReservoir::with_seed(1, seed);
            // Insert a "light" item and a "heavy" item.
            wr.observe("light", 0.001);
            wr.observe("heavy", 1000.0);

            let sample = wr.sample();
            if sample.contains(&&"heavy") {
                found_heavy += 1;
            }
        }

        // The heavy item should appear in most samples.
        assert!(
            found_heavy > trials * 90 / 100,
            "heavy found in {found_heavy}/{trials} trials"
        );
    }

    #[test]
    fn weighted_clear() {
        let mut wr = WeightedReservoir::new(5);
        wr.observe(1, 1.0);
        wr.clear();
        assert!(wr.is_empty());
        assert_eq!(wr.seen(), 0);
    }

    #[test]
    fn weighted_into_sample() {
        let mut wr = WeightedReservoir::new(2);
        wr.observe(10, 1.0);
        wr.observe(20, 1.0);
        let sample = wr.into_sample();
        assert_eq!(sample.len(), 2);
    }

    #[test]
    fn weighted_debug_format() {
        let wr: WeightedReservoir<i32> = WeightedReservoir::new(5);
        let s = format!("{wr:?}");
        assert!(s.contains("WeightedReservoir"));
    }

    #[test]
    fn weighted_stats() {
        let mut wr = WeightedReservoir::new(5);
        for i in 0..100 {
            wr.observe(i, 1.0);
        }
        let s = wr.stats();
        assert_eq!(s.capacity, 5);
        assert_eq!(s.current_size, 5);
        assert_eq!(s.total_seen, 100);
        assert!((s.sampling_rate - 0.05).abs() < 1e-10);
    }

    #[test]
    #[should_panic(expected = "weight must be positive")]
    fn weighted_zero_weight_panics() {
        let mut wr = WeightedReservoir::new(5);
        wr.observe(1, 0.0);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _rs: ReservoirSampler<i32> = ReservoirSampler::new(0);
    }

    #[test]
    fn single_capacity() {
        let mut rs = ReservoirSampler::new(1);
        for i in 0..100 {
            rs.observe(i);
        }
        assert_eq!(rs.len(), 1);
        assert_eq!(rs.seen(), 100);
    }

    #[test]
    fn weighted_deterministic_with_seed() {
        let mut wr1 = WeightedReservoir::with_seed(3, 42);
        let mut wr2 = WeightedReservoir::with_seed(3, 42);

        for i in 0..50 {
            let w = (i as f64 + 1.0).sqrt();
            wr1.observe(i, w);
            wr2.observe(i, w);
        }

        let s1 = wr1.into_sample();
        let s2 = wr2.into_sample();
        assert_eq!(s1, s2);
    }
}
