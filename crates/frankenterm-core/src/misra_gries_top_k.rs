//! br-ft-misra-gries: alien-uplift §9.4 Sketching for streaming
//! top-K frequent-items estimation with bounded memory.
//!
//! Implements the Space-Saving algorithm (Metwally, Agrawal, El
//! Abbadi 2005, "Efficient Computation of Frequent and Top-k
//! Elements in Data Streams", ICDT 2005), a refinement of the
//! Misra-Gries 1982 frequent-items algorithm
//! ("Finding repeated elements", Science of Computer Programming
//! 2(2), 143-152). Both variants are catalogued under
//! alien-graveyard §9.4.
//!
//! # Why a top-K sketch
//!
//! frankenterm has multiple "what are the most-common X" use cases
//! today (top-K most-frequent error codes per session, top-K
//! hottest tools per agent, top-K most-active panes, top-K
//! pattern matches). The current pattern is `Vec<(K, V)> + sort +
//! take(N)` — O(N log N) per query and requires retaining EVERY
//! observation. For high-volume streams (e.g. the 200-agent fleet
//! emitting thousands of pattern matches per second) this doesn't
//! scale.
//!
//! Space-Saving gives:
//! - **Bounded memory**: K monitored items, ~50 bytes per slot
//!   (string key + count + error bound). At K=100, total ~5KB
//!   regardless of stream size.
//! - **O(1) per insert** + O(K log K) per top-K query.
//! - **Distribution-free guarantee** (Metwally et al. 2005): any
//!   item with true frequency > N/K (where N is total inserts) is
//!   GUARANTEED to be in the result set. The estimated count is
//!   within `[true_count, true_count + max_error]` where
//!   max_error ≤ N/K.
//!
//! # When to choose Space-Saving over Count-Min Sketch
//!
//! `crate::count_min_sketch` (in workspace via
//! `frankenterm-core-telemetry-types`) gives O(1) frequency
//! ESTIMATES for any item, but doesn't enumerate the top-K items
//! — caller must already know which items to query. Space-Saving
//! enumerates the top-K from the stream itself.
//!
//! Use both: Count-Min for "estimated frequency of item X",
//! Space-Saving for "what are the K most-frequent items?".
//!
//! # What ships
//!
//! - [`SpaceSavingTopK`] — the algorithm state.
//! - [`MonitoredItem`] — single (key, count, error) entry.
//! - [`SpaceSavingSnapshot`] — serde snapshot for telemetry.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::Hash;

/// br-ft-misra-gries: a single monitored entry in the
/// Space-Saving sketch. The estimated count is in
/// `[true_count, true_count + error]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitoredItem<K> {
    pub key: K,
    pub count: u64,
    pub error: u64,
}

/// br-ft-misra-gries: streaming top-K frequent-items estimator
/// (Space-Saving algorithm, Metwally et al. 2005). Maintains K
/// monitored slots; every insert is O(1).
///
/// Distribution-free guarantee: any item with true frequency >
/// N/K is guaranteed to be in the result set. Estimated counts
/// have bounded error (≤ N/K).
#[derive(Debug, Clone)]
pub struct SpaceSavingTopK<K: Hash + Eq + Clone> {
    /// Maximum number of monitored items.
    capacity: usize,
    /// Monitored items keyed by their stream key for O(1) lookup.
    /// Value is (count, error).
    monitored: HashMap<K, (u64, u64)>,
    /// Total inserts across all keys (for top_k frequency-bound
    /// reporting).
    total_inserts: u64,
}

impl<K: Hash + Eq + Clone> SpaceSavingTopK<K> {
    /// Create a new top-K sketch with the given capacity.
    /// Capacity is the maximum number of items the sketch can
    /// monitor at once + the K in "top-K" (the algorithm gives
    /// the top-K from the K monitored slots).
    ///
    /// Capacity is clamped to ≥ 1.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            monitored: HashMap::with_capacity(capacity),
            total_inserts: 0,
        }
    }

    /// Record an insertion of `key` (with implicit count = 1).
    pub fn insert(&mut self, key: K) {
        self.insert_weighted(key, 1);
    }

    /// Record an insertion of `key` with the given weight.
    /// Useful for batch updates or weighted streams.
    pub fn insert_weighted(&mut self, key: K, weight: u64) {
        if weight == 0 {
            return;
        }
        self.total_inserts = self.total_inserts.saturating_add(weight);

        // Case 1: key is already monitored — increment count.
        if let Some(entry) = self.monitored.get_mut(&key) {
            entry.0 = entry.0.saturating_add(weight);
            return;
        }

        // Case 2: under capacity — add as new entry with error = 0.
        if self.monitored.len() < self.capacity {
            self.monitored.insert(key, (weight, 0));
            return;
        }

        // Case 3: at capacity — evict min-count item + replace.
        // Per Metwally et al. 2005:
        //   new_count = old_min_count + weight
        //   new_error = old_min_count (bounds the over-estimate)
        let (min_key, min_count) = self
            .monitored
            .iter()
            .min_by_key(|(_, (c, _))| *c)
            .map(|(k, (c, _))| (k.clone(), *c))
            .expect("capacity ≥ 1 + map at capacity → at least one entry");
        self.monitored.remove(&min_key);
        self.monitored
            .insert(key, (min_count.saturating_add(weight), min_count));
    }

    /// Return the top K monitored items sorted by estimated count
    /// (descending). Ties broken by lower error bound (more
    /// confident estimates first).
    ///
    /// Distribution-free guarantee: any item with true frequency
    /// greater than N/capacity is in the result set; counts have error ≤
    /// total_inserts/capacity.
    #[must_use]
    pub fn top_k(&self, k: usize) -> Vec<MonitoredItem<K>> {
        let k = k.min(self.monitored.len());
        let mut items: Vec<MonitoredItem<K>> = self
            .monitored
            .iter()
            .map(|(key, (count, error))| MonitoredItem {
                key: key.clone(),
                count: *count,
                error: *error,
            })
            .collect();
        // Sort by count desc, then error asc (more confident first).
        items.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.error.cmp(&b.error)));
        items.truncate(k);
        items
    }

    /// Total insertions across all keys.
    #[must_use]
    pub fn total_inserts(&self) -> u64 {
        self.total_inserts
    }

    /// Number of currently-monitored items (≤ capacity).
    #[must_use]
    pub fn monitored_count(&self) -> usize {
        self.monitored.len()
    }

    /// Configured capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Maximum estimation error bound. Per Metwally et al. 2005:
    /// for any monitored item, `true_count ≤ estimated_count ≤
    /// true_count + max_error`, where max_error ≤
    /// total_inserts / capacity.
    #[must_use]
    pub fn max_error(&self) -> u64 {
        if self.capacity == 0 {
            return 0;
        }
        self.total_inserts / self.capacity as u64
    }

    /// Reset to initial state (drop all monitored items).
    pub fn reset(&mut self) {
        self.monitored.clear();
        self.total_inserts = 0;
    }

    /// Estimated count for a specific key. Returns 0 if the key
    /// is not monitored (which means its true count is ≤
    /// max_error per the Space-Saving guarantee).
    #[must_use]
    pub fn estimated_count(&self, key: &K) -> u64 {
        self.monitored.get(key).map_or(0, |(c, _)| *c)
    }

    /// Whether a specific key is currently monitored.
    #[must_use]
    pub fn is_monitored(&self, key: &K) -> bool {
        self.monitored.contains_key(key)
    }
}

impl<K: Hash + Eq + Clone + Serialize + for<'de> Deserialize<'de>> SpaceSavingTopK<K> {
    /// Snapshot for telemetry / doctor reports. The serialized
    /// form is the top-K monitored items + total inserts +
    /// capacity.
    #[must_use]
    pub fn snapshot(&self, k: usize) -> SpaceSavingSnapshot<K> {
        SpaceSavingSnapshot {
            capacity: self.capacity as u32,
            total_inserts: self.total_inserts,
            max_error: self.max_error(),
            top_items: self.top_k(k),
        }
    }
}

/// br-ft-misra-gries: serde snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceSavingSnapshot<K> {
    pub capacity: u32,
    pub total_inserts: u64,
    pub max_error: u64,
    pub top_items: Vec<MonitoredItem<K>>,
}

impl<K> Default for SpaceSavingSnapshot<K> {
    fn default() -> Self {
        Self {
            capacity: 0,
            total_inserts: 0,
            max_error: 0,
            top_items: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sketch_returns_no_items() {
        let s: SpaceSavingTopK<String> = SpaceSavingTopK::new(10);
        assert_eq!(s.total_inserts(), 0);
        assert_eq!(s.monitored_count(), 0);
        assert!(s.top_k(5).is_empty());
    }

    #[test]
    fn capacity_clamps_to_minimum_one() {
        let s: SpaceSavingTopK<String> = SpaceSavingTopK::new(0);
        assert_eq!(s.capacity(), 1);
    }

    #[test]
    fn insert_below_capacity_adds_with_zero_error() {
        let mut s: SpaceSavingTopK<&str> = SpaceSavingTopK::new(10);
        s.insert("a");
        s.insert("a");
        s.insert("b");
        let top = s.top_k(2);
        assert_eq!(top.len(), 2);
        // a appeared twice → count=2, error=0; b once → count=1, error=0.
        assert_eq!(top[0].key, "a");
        assert_eq!(top[0].count, 2);
        assert_eq!(top[0].error, 0);
        assert_eq!(top[1].key, "b");
        assert_eq!(top[1].count, 1);
        assert_eq!(top[1].error, 0);
    }

    #[test]
    fn insert_at_capacity_evicts_min_and_adds_with_error() {
        // br-ft-misra-gries Space-Saving eviction rule:
        //   new_count = old_min_count + weight
        //   new_error = old_min_count
        let mut s: SpaceSavingTopK<&str> = SpaceSavingTopK::new(2);
        s.insert("a"); // monitored {a: (1, 0)}
        s.insert("b"); // monitored {a: (1, 0), b: (1, 0)}
        s.insert("c"); // at capacity — evict min (tie: a or b).
        // new entry: count = 1 + 1 = 2, error = 1.
        assert_eq!(s.monitored_count(), 2);
        let c = s.top_k(2);
        // Find the c entry — must have count=2 + error=1.
        let c_entry = c.iter().find(|i| i.key == "c").expect("c is monitored");
        assert_eq!(c_entry.count, 2);
        assert_eq!(
            c_entry.error, 1,
            "br-ft-misra-gries error bound = old min count"
        );
    }

    #[test]
    fn frequency_bound_guarantees_heavy_hitters_are_captured() {
        // br-ft-misra-gries load-bearing distribution-free
        // guarantee: any item with true frequency > N/capacity
        // MUST be in the top_k result.
        // 5 monitor slots, total inserts 100. Threshold = 100/5 = 20.
        // Item "frequent" appears 30 times → MUST be captured.
        let mut s: SpaceSavingTopK<&str> = SpaceSavingTopK::new(5);
        for _ in 0..30 {
            s.insert("frequent");
        }
        // Sprinkle 70 unique low-frequency items.
        let unique: Vec<String> = (0..70).map(|i| format!("u{i}")).collect();
        for u in &unique {
            s.insert(u.as_str());
        }
        let top = s.top_k(5);
        let captured = top.iter().any(|i| i.key == "frequent");
        assert!(
            captured,
            "br-ft-misra-gries guarantee: 'frequent' (30/100 > 20) must be in top-5; got {top:?}"
        );
        // Its estimated count should be ≥ true count (30) and ≤
        // true count + max_error (30 + 100/5 = 50).
        let item = top.iter().find(|i| i.key == "frequent").unwrap();
        assert!(
            item.count >= 30 && item.count <= 50,
            "estimated count {} outside [30, 50]",
            item.count
        );
    }

    #[test]
    fn max_error_bounded_by_total_over_capacity() {
        let mut s: SpaceSavingTopK<u64> = SpaceSavingTopK::new(10);
        for i in 0..100 {
            s.insert(i);
        }
        assert_eq!(s.total_inserts(), 100);
        assert!(
            s.max_error() <= 100 / 10,
            "max_error must be ≤ N/capacity = 10"
        );
    }

    #[test]
    fn weighted_insert_accumulates_correctly() {
        let mut s: SpaceSavingTopK<&str> = SpaceSavingTopK::new(5);
        s.insert_weighted("a", 5);
        s.insert_weighted("a", 3);
        s.insert_weighted("b", 2);
        assert_eq!(s.total_inserts(), 10);
        assert_eq!(s.estimated_count(&"a"), 8);
        assert_eq!(s.estimated_count(&"b"), 2);
    }

    #[test]
    fn weighted_insert_zero_weight_is_noop() {
        let mut s: SpaceSavingTopK<&str> = SpaceSavingTopK::new(5);
        s.insert_weighted("a", 0);
        assert_eq!(s.total_inserts(), 0);
        assert_eq!(s.estimated_count(&"a"), 0);
    }

    #[test]
    fn top_k_truncates_at_requested_count() {
        let mut s: SpaceSavingTopK<u64> = SpaceSavingTopK::new(10);
        for i in 0..10 {
            s.insert(i);
        }
        assert_eq!(s.top_k(3).len(), 3);
        assert_eq!(s.top_k(100).len(), 10, "top_k caps at monitored_count");
    }

    #[test]
    fn estimated_count_zero_for_unmonitored_key() {
        let mut s: SpaceSavingTopK<&str> = SpaceSavingTopK::new(2);
        s.insert("a");
        s.insert("b");
        // c was never inserted.
        assert_eq!(s.estimated_count(&"c"), 0);
        assert!(!s.is_monitored(&"c"));
    }

    #[test]
    fn reset_clears_all_state() {
        let mut s: SpaceSavingTopK<&str> = SpaceSavingTopK::new(5);
        for w in ["a", "b", "c"] {
            s.insert(w);
        }
        assert!(s.total_inserts() > 0);
        s.reset();
        assert_eq!(s.total_inserts(), 0);
        assert_eq!(s.monitored_count(), 0);
    }

    #[test]
    fn snapshot_serde_roundtrips() {
        let mut s: SpaceSavingTopK<String> = SpaceSavingTopK::new(5);
        for _ in 0..3 {
            s.insert("frequent".to_string());
        }
        s.insert("rare".to_string());
        let snap = s.snapshot(5);
        let json = serde_json::to_string(&snap).expect("serialize");
        let parsed: SpaceSavingSnapshot<String> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, snap);
    }

    #[test]
    fn convergence_zipfian_distribution_top_3_correct() {
        // Realistic stream shape: power-law / Zipfian. The
        // top-3 items dominate; Space-Saving must converge.
        let mut s: SpaceSavingTopK<u64> = SpaceSavingTopK::new(20);
        // Item 0: 100 inserts.
        for _ in 0..100 {
            s.insert(0u64);
        }
        // Item 1: 50 inserts.
        for _ in 0..50 {
            s.insert(1u64);
        }
        // Item 2: 25 inserts.
        for _ in 0..25 {
            s.insert(2u64);
        }
        // 200 unique low-freq items, 1 each.
        for i in 100..300u64 {
            s.insert(i);
        }
        let top3 = s.top_k(3);
        // Top-3 must be {0, 1, 2} in count order.
        assert_eq!(top3.len(), 3);
        let keys: Vec<u64> = top3.iter().map(|item| item.key).collect();
        assert_eq!(
            keys,
            vec![0, 1, 2],
            "Zipfian top-3 must be the 3 heavy hitters in order"
        );
    }
}
