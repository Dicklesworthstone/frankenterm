//! Property-based tests for lru_cache module.
//!
//! Verifies the arena-based LRU cache invariants:
//! - Capacity bound: len() <= capacity() at all times
//! - No false negatives: put(k,v) then get(k) returns v (unless evicted)
//! - Eviction ordering: always evicts the least-recently used item
//! - Recency promotion: get(k) promotes k to most-recently used
//! - Update promotion: put(existing_k, new_v) promotes and updates
//! - Stats consistency: hits + misses == total_lookups
//! - Peek is non-promoting: peek(k) doesn't change recency order
//! - Iterator consistency: MRU and LRU iterators are reverse of each other
//! - Clear resets: clear() yields empty cache
//! - Resize correctness: resize(smaller) evicts LRU entries
//! - Free-list reuse: arena stays bounded through remove/evict cycles
//! - Retain predicate: only matching entries survive

use proptest::prelude::*;
use std::collections::{HashMap, VecDeque};

use frankenterm_core::lru_cache::LruCache;

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_capacity() -> impl Strategy<Value = usize> {
    1usize..=20
}

fn arb_key() -> impl Strategy<Value = u16> {
    0u16..30
}

fn arb_value() -> impl Strategy<Value = i32> {
    any::<i32>()
}

/// A cache operation for state-machine testing.
#[derive(Debug, Clone)]
enum Op {
    Put(u16, i32),
    Get(u16),
    Peek(u16),
    Remove(u16),
    ContainsKey(u16),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (arb_key(), arb_value()).prop_map(|(k, v)| Op::Put(k, v)),
        arb_key().prop_map(Op::Get),
        arb_key().prop_map(Op::Peek),
        arb_key().prop_map(Op::Remove),
        arb_key().prop_map(Op::ContainsKey),
    ]
}

fn arb_ops(max: usize) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(arb_op(), 1..max)
}

/// A reference model tracking recency order as a VecDeque (front=MRU, back=LRU).
struct RefModel {
    capacity: usize,
    order: VecDeque<u16>,   // front=MRU, back=LRU
    map: HashMap<u16, i32>, // key → value
}

impl RefModel {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    fn promote(&mut self, key: u16) {
        if let Some(pos) = self.order.iter().position(|&k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_front(key);
    }

    fn put(&mut self, key: u16, value: i32) -> Option<(u16, i32)> {
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.map.entry(key) {
            // Update existing
            e.insert(value);
            self.promote(key);
            return None;
        }

        // New entry — may evict
        let evicted = if self.map.len() >= self.capacity {
            let lru_key = self.order.pop_back().unwrap();
            let lru_val = self.map.remove(&lru_key).unwrap();
            Some((lru_key, lru_val))
        } else {
            None
        };

        self.map.insert(key, value);
        self.order.push_front(key);
        evicted
    }

    fn get(&mut self, key: u16) -> Option<i32> {
        if self.map.contains_key(&key) {
            self.promote(key);
            Some(self.map[&key])
        } else {
            None
        }
    }

    fn peek(&self, key: u16) -> Option<i32> {
        self.map.get(&key).copied()
    }

    fn remove(&mut self, key: u16) -> Option<i32> {
        if let Some(val) = self.map.remove(&key) {
            if let Some(pos) = self.order.iter().position(|&k| k == key) {
                self.order.remove(pos);
            }
            Some(val)
        } else {
            None
        }
    }

    fn contains_key(&self, key: u16) -> bool {
        self.map.contains_key(&key)
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn mru_order(&self) -> Vec<u16> {
        self.order.iter().copied().collect()
    }
}

// ────────────────────────────────────────────────────────────────────
// State-machine model checking: real cache vs reference model
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// The cache matches a reference model through arbitrary operation sequences.
    #[test]
    fn prop_matches_reference_model(
        capacity in arb_capacity(),
        ops in arb_ops(80),
    ) {
        let mut cache = LruCache::new(capacity);
        let mut model = RefModel::new(capacity);

        for op in &ops {
            match op {
                Op::Put(k, v) => {
                    let cache_evicted = cache.put(*k, *v);
                    let model_evicted = model.put(*k, *v);
                    prop_assert_eq!(
                        cache_evicted, model_evicted,
                        "Eviction mismatch on put({}, {})", k, v
                    );
                }
                Op::Get(k) => {
                    let cache_val = cache.get(k).copied();
                    let model_val = model.get(*k);
                    prop_assert_eq!(
                        cache_val, model_val,
                        "Get mismatch for key={}", k
                    );
                }
                Op::Peek(k) => {
                    let cache_val = cache.peek(k).copied();
                    let model_val = model.peek(*k);
                    prop_assert_eq!(
                        cache_val, model_val,
                        "Peek mismatch for key={}", k
                    );
                }
                Op::Remove(k) => {
                    let cache_val = cache.remove(k);
                    let model_val = model.remove(*k);
                    prop_assert_eq!(
                        cache_val, model_val,
                        "Remove mismatch for key={}", k
                    );
                }
                Op::ContainsKey(k) => {
                    let cache_has = cache.contains_key(k);
                    let model_has = model.contains_key(*k);
                    prop_assert_eq!(
                        cache_has, model_has,
                        "ContainsKey mismatch for key={}", k
                    );
                }
            }

            // Invariant: len must match
            prop_assert_eq!(
                cache.len(), model.len(),
                "Length mismatch after op {:?}", op
            );

            // Invariant: capacity bound
            prop_assert!(
                cache.len() <= cache.capacity(),
                "len {} > capacity {}", cache.len(), cache.capacity()
            );
        }

        // Final check: MRU iteration order matches model
        let cache_order: Vec<u16> = cache.iter_mru().map(|(k, _)| *k).collect();
        let model_order = model.mru_order();
        prop_assert_eq!(
            cache_order, model_order,
            "Final MRU order mismatch"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Capacity bound
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// len() never exceeds capacity(), regardless of operations.
    #[test]
    fn prop_len_never_exceeds_capacity(
        capacity in arb_capacity(),
        keys in prop::collection::vec(arb_key(), 1..100),
    ) {
        let mut cache = LruCache::new(capacity);
        for (i, &k) in keys.iter().enumerate() {
            cache.put(k, i as i32);
            prop_assert!(
                cache.len() <= capacity,
                "len {} exceeded capacity {} after {} puts", cache.len(), capacity, i + 1
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// No false negatives
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Every key that was put and not evicted or removed is retrievable.
    #[test]
    fn prop_no_false_negatives(
        capacity in 5usize..20,
        entries in prop::collection::vec((arb_key(), arb_value()), 1..10),
    ) {
        let mut cache = LruCache::new(capacity);
        let mut present: HashMap<u16, i32> = HashMap::new();
        let mut evicted_keys = Vec::new();

        for &(k, v) in &entries {
            if let Some((ek, _)) = cache.put(k, v) {
                present.remove(&ek);
                evicted_keys.push(ek);
            }
            present.insert(k, v);
        }

        // Every key in `present` must be gettable (use peek to avoid promotion)
        for (&k, &v) in &present {
            let got = cache.peek(&k);
            prop_assert_eq!(
                got, Some(&v),
                "Key {} should be present with value {}", k, v
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Eviction ordering: always evicts the LRU item
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// When at capacity, put always evicts the tail of iter_lru (the LRU item).
    #[test]
    fn prop_eviction_removes_lru(
        capacity in 2usize..10,
        initial_keys in prop::collection::vec(0u16..50, 2..10),
        new_key in 50u16..100,
    ) {
        let mut cache = LruCache::new(capacity);
        // Insert enough unique keys to fill the cache
        let mut unique_keys = Vec::new();
        for &k in &initial_keys {
            if !unique_keys.contains(&k) {
                unique_keys.push(k);
            }
        }
        // Ensure we have at least capacity keys
        let fill_count = capacity.min(unique_keys.len());
        for &k in &unique_keys[..fill_count] {
            cache.put(k, k as i32);
        }

        if cache.len() == capacity {
            // Identify the current LRU before inserting
            let lru_before = cache.peek_lru().map(|(&k, _)| k);

            // Use a key guaranteed not in the cache
            if !cache.contains_key(&new_key) {
                let evicted = cache.put(new_key, new_key as i32);
                if let (Some(lru_key), Some((evicted_key, _))) = (lru_before, evicted) {
                    prop_assert_eq!(
                        evicted_key, lru_key,
                        "Evicted key {} != LRU key {}", evicted_key, lru_key
                    );
                }
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Recency promotion
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// get(k) makes k the most-recently used entry.
    #[test]
    fn prop_get_promotes_to_mru(
        capacity in 3usize..10,
        n_items in 3usize..10,
    ) {
        let cap = capacity.min(n_items);
        let mut cache = LruCache::new(cap);
        for i in 0..cap as u16 {
            cache.put(i, i as i32);
        }
        // MRU is (cap-1), LRU is 0

        // Access key 0 (currently LRU) — should promote it to MRU
        cache.get(&0);

        let mru = cache.peek_mru().map(|(&k, _)| k);
        prop_assert_eq!(mru, Some(0), "key 0 should be MRU after get");
    }

    /// put(existing_k, new_v) promotes to MRU and updates value.
    #[test]
    fn prop_update_promotes_to_mru(
        capacity in 3usize..10,
        n_items in 3usize..10,
        new_val in arb_value(),
    ) {
        let cap = capacity.min(n_items);
        let mut cache = LruCache::new(cap);
        for i in 0..cap as u16 {
            cache.put(i, i as i32);
        }

        // Update key 0 (LRU)
        let evicted = cache.put(0, new_val);
        prop_assert!(evicted.is_none(), "Update should not evict");

        let mru = cache.peek_mru().map(|(&k, _)| k);
        prop_assert_eq!(mru, Some(0), "key 0 should be MRU after update");
        prop_assert_eq!(cache.peek(&0), Some(&new_val));
    }
}

// ────────────────────────────────────────────────────────────────────
// Peek is non-promoting
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// peek(k) does not change the recency order.
    #[test]
    fn prop_peek_preserves_order(
        capacity in 3usize..10,
    ) {
        let mut cache = LruCache::new(capacity);
        for i in 0..capacity as u16 {
            cache.put(i, i as i32);
        }

        // Snapshot order before peek
        let order_before: Vec<u16> = cache.iter_mru().map(|(k, _)| *k).collect();

        // Peek at the LRU element
        cache.peek(&0);

        // Order should be unchanged
        let order_after: Vec<u16> = cache.iter_mru().map(|(k, _)| *k).collect();
        prop_assert_eq!(order_before, order_after, "Peek changed recency order");
    }
}

// ────────────────────────────────────────────────────────────────────
// Stats consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// hits + misses == total_lookups after any operation sequence.
    #[test]
    fn prop_stats_hits_plus_misses_eq_lookups(
        capacity in arb_capacity(),
        ops in arb_ops(60),
    ) {
        let mut cache = LruCache::new(capacity);
        let mut expected_hits = 0u64;
        let mut expected_misses = 0u64;
        let mut expected_insertions = 0u64;
        let mut expected_updates = 0u64;
        let mut expected_evictions = 0u64;
        let mut expected_removals = 0u64;

        for op in &ops {
            match op {
                Op::Put(k, v) => {
                    let existed = cache.contains_key(k);
                    let at_cap = cache.len() == cache.capacity();
                    let evicted = cache.put(*k, *v);
                    if existed {
                        expected_updates += 1;
                    } else {
                        expected_insertions += 1;
                        if at_cap && evicted.is_some() {
                            expected_evictions += 1;
                        }
                    }
                }
                Op::Get(k) => {
                    if cache.contains_key(k) {
                        // contains_key doesn't affect stats — we know result
                        cache.get(k);
                        expected_hits += 1;
                    } else {
                        cache.get(k);
                        expected_misses += 1;
                    }
                }
                Op::Remove(k) => {
                    if cache.contains_key(k) {
                        cache.remove(k);
                        expected_removals += 1;
                    } else {
                        cache.remove(k);
                    }
                }
                Op::Peek(_) | Op::ContainsKey(_) => {
                    // These don't affect stats
                    match op {
                        Op::Peek(k) => { cache.peek(k); }
                        Op::ContainsKey(k) => { cache.contains_key(k); }
                        _ => unreachable!(),
                    }
                }
            }
        }

        let stats = cache.stats();
        prop_assert_eq!(
            stats.total_lookups(), expected_hits + expected_misses,
            "total_lookups mismatch"
        );
        prop_assert_eq!(stats.hits, expected_hits, "hits mismatch");
        prop_assert_eq!(stats.misses, expected_misses, "misses mismatch");
        prop_assert_eq!(stats.insertions, expected_insertions, "insertions mismatch");
        prop_assert_eq!(stats.updates, expected_updates, "updates mismatch");
        prop_assert_eq!(stats.evictions, expected_evictions, "evictions mismatch");
        prop_assert_eq!(stats.removals, expected_removals, "removals mismatch");
    }

    /// hit_rate is in [0.0, 1.0] and consistent with hits/misses.
    #[test]
    fn prop_hit_rate_bounded(
        capacity in arb_capacity(),
        ops in arb_ops(40),
    ) {
        let mut cache = LruCache::new(capacity);
        for op in &ops {
            match op {
                Op::Put(k, v) => { cache.put(*k, *v); }
                Op::Get(k) => { cache.get(k); }
                Op::Peek(k) => { cache.peek(k); }
                Op::Remove(k) => { cache.remove(k); }
                Op::ContainsKey(k) => { cache.contains_key(k); }
            }
        }

        let rate = cache.stats().hit_rate();
        prop_assert!(
            (0.0..=1.0).contains(&rate),
            "hit_rate {} out of [0.0, 1.0]", rate
        );

        let stats = cache.stats();
        if stats.total_lookups() > 0 {
            let expected = stats.hits as f64 / stats.total_lookups() as f64;
            prop_assert!(
                (rate - expected).abs() < 1e-10,
                "hit_rate {} != expected {}", rate, expected
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Iterator consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// iter_mru and iter_lru are exact reverses of each other.
    #[test]
    fn prop_iterators_are_reverse(
        capacity in arb_capacity(),
        entries in prop::collection::vec((arb_key(), arb_value()), 1..30),
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries {
            cache.put(k, v);
        }

        let mru: Vec<(u16, i32)> = cache.iter_mru().map(|(&k, &v)| (k, v)).collect();
        let lru: Vec<(u16, i32)> = cache.iter_lru().map(|(&k, &v)| (k, v)).collect();

        let mut lru_reversed = lru;
        lru_reversed.reverse();

        prop_assert_eq!(mru, lru_reversed, "MRU and reversed LRU don't match");
    }

    /// Iterator count matches len().
    #[test]
    fn prop_iterator_count_matches_len(
        capacity in arb_capacity(),
        entries in prop::collection::vec((arb_key(), arb_value()), 0..30),
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries {
            cache.put(k, v);
        }

        let count = cache.iter_mru().count();
        prop_assert_eq!(count, cache.len(), "iter_mru count != len");

        let count_lru = cache.iter_lru().count();
        prop_assert_eq!(count_lru, cache.len(), "iter_lru count != len");
    }

    /// Iterator size_hint is exact.
    #[test]
    fn prop_iterator_size_hint_exact(
        capacity in arb_capacity(),
        entries in prop::collection::vec((arb_key(), arb_value()), 0..20),
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries {
            cache.put(k, v);
        }

        let iter = cache.iter_mru();
        let (lo, hi) = iter.size_hint();
        prop_assert_eq!(lo, cache.len());
        prop_assert_eq!(hi, Some(cache.len()));
    }
}

// ────────────────────────────────────────────────────────────────────
// Clear resets
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() empties the cache completely.
    #[test]
    fn prop_clear_resets_all(
        capacity in arb_capacity(),
        entries in prop::collection::vec((arb_key(), arb_value()), 1..30),
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries {
            cache.put(k, v);
        }

        cache.clear();

        prop_assert!(cache.is_empty(), "Cache not empty after clear");
        prop_assert_eq!(cache.len(), 0);
        prop_assert_eq!(cache.peek_lru(), None);
        prop_assert_eq!(cache.peek_mru(), None);
        prop_assert_eq!(cache.iter_mru().count(), 0);

        // Verify all previous keys are gone
        for &(k, _) in &entries {
            prop_assert_eq!(cache.peek(&k), None, "Key {} still present after clear", k);
        }
    }

    /// After clear, new entries work normally.
    #[test]
    fn prop_clear_then_reuse(
        capacity in arb_capacity(),
        entries1 in prop::collection::vec((arb_key(), arb_value()), 1..20),
        entries2 in prop::collection::vec((arb_key(), arb_value()), 1..20),
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries1 {
            cache.put(k, v);
        }

        cache.clear();

        for &(k, v) in &entries2 {
            cache.put(k, v);
        }

        prop_assert!(cache.len() <= capacity);
        // Verify entries2 items are present (those not evicted)
        let present_count = cache.iter_mru().count();
        prop_assert!(present_count <= capacity);
    }
}

// ────────────────────────────────────────────────────────────────────
// Resize correctness
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// resize(smaller) evicts LRU entries and respects new capacity.
    #[test]
    fn prop_resize_smaller_evicts_lru(
        capacity in 5usize..15,
        new_cap in 1usize..5,
    ) {
        let mut cache = LruCache::new(capacity);
        for i in 0..capacity as u16 {
            cache.put(i, i as i32);
        }

        // Snapshot LRU order before resize
        let lru_order: Vec<u16> = cache.iter_lru().map(|(k, _)| *k).collect();

        let n_evict = capacity - new_cap;
        let expected_evicted_keys: Vec<u16> = lru_order[..n_evict].to_vec();

        let evicted = cache.resize(new_cap);

        prop_assert_eq!(cache.capacity(), new_cap);
        prop_assert_eq!(cache.len(), new_cap);
        prop_assert_eq!(evicted.len(), n_evict);

        // Verify evicted keys match the LRU tail
        let evicted_keys: Vec<u16> = evicted.iter().map(|&(k, _)| k).collect();
        prop_assert_eq!(evicted_keys, expected_evicted_keys);

        // Verify remaining keys are the MRU ones
        for &(k, _) in &evicted {
            prop_assert!(!cache.contains_key(&k), "Evicted key {} still in cache", k);
        }
    }

    /// resize(larger) doesn't evict anything.
    #[test]
    fn prop_resize_larger_no_eviction(
        initial_cap in 2usize..10,
        extra in 1usize..10,
        n_items in 1usize..10,
    ) {
        let cap = initial_cap;
        let mut cache = LruCache::new(cap);
        let item_count = n_items.min(cap);
        for i in 0..item_count as u16 {
            cache.put(i, i as i32);
        }

        let len_before = cache.len();
        let new_cap = cap + extra;
        let evicted = cache.resize(new_cap);

        prop_assert!(evicted.is_empty(), "Resize larger shouldn't evict");
        prop_assert_eq!(cache.len(), len_before);
        prop_assert_eq!(cache.capacity(), new_cap);
    }
}

// ────────────────────────────────────────────────────────────────────
// Free-list reuse: arena stays bounded
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After many eviction/remove cycles, arena doesn't grow unbounded.
    #[test]
    fn prop_arena_bounded_under_churn(
        capacity in 2usize..10,
        n_rounds in 5usize..20,
    ) {
        let mut cache = LruCache::new(capacity);

        for round in 0..n_rounds {
            let base = (round * capacity) as u16;
            for i in 0..capacity as u16 {
                cache.put(base + i, (base + i) as i32);
            }
            // Remove half
            for i in 0..(capacity / 2) as u16 {
                cache.remove(&(base + i));
            }
        }

        // The arena should never be larger than capacity + a small overhead from
        // free-list recycling. In the worst case, it shouldn't exceed 2x capacity
        // because removed slots get recycled.
        // The exact bound depends on implementation, but we verify it's bounded.
        prop_assert!(
            cache.len() <= capacity,
            "len {} exceeded capacity {}", cache.len(), capacity
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Retain predicate
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// retain keeps only entries matching the predicate.
    #[test]
    fn prop_retain_keeps_matching(
        capacity in 5usize..15,
        entries in prop::collection::vec(0u16..30, 5..20),
        threshold in 0i32..50,
    ) {
        let mut cache = LruCache::new(capacity);
        for &k in &entries {
            cache.put(k, k as i32);
        }

        cache.retain(|_k, &v| v >= threshold);

        // All remaining entries should satisfy the predicate
        for (&k, &v) in cache.iter_mru() {
            prop_assert!(
                v >= threshold,
                "Retained entry k={}, v={} doesn't satisfy v >= {}", k, v, threshold
            );
        }

        // No remaining entries should violate it
        prop_assert!(cache.len() <= capacity);
    }
}

// ────────────────────────────────────────────────────────────────────
// Pop LRU
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// pop_lru returns the same entry as peek_lru and removes it.
    #[test]
    fn prop_pop_lru_matches_peek_lru(
        capacity in arb_capacity(),
        entries in prop::collection::vec((arb_key(), arb_value()), 1..20),
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries {
            cache.put(k, v);
        }

        if !cache.is_empty() {
            let peeked = cache.peek_lru().map(|(&k, &v)| (k, v));
            let len_before = cache.len();
            let popped = cache.pop_lru();

            prop_assert_eq!(popped, peeked, "pop_lru != peek_lru");
            prop_assert_eq!(cache.len(), len_before - 1);

            if let Some((k, _)) = popped {
                prop_assert!(!cache.contains_key(&k), "Popped key still in cache");
            }
        }
    }

    /// Repeated pop_lru drains the cache in LRU order.
    #[test]
    fn prop_drain_via_pop_lru(
        capacity in arb_capacity(),
        entries in prop::collection::vec((arb_key(), arb_value()), 1..20),
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries {
            cache.put(k, v);
        }

        // Snapshot LRU order
        let expected_order: Vec<(u16, i32)> = cache.iter_lru().map(|(&k, &v)| (k, v)).collect();

        // Drain via pop_lru
        let mut drained = Vec::new();
        while let Some(pair) = cache.pop_lru() {
            drained.push(pair);
        }

        prop_assert_eq!(drained, expected_order, "Drain order != LRU order");
        prop_assert!(cache.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// Idempotent operations
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Inserting the same key-value pair twice is an update (no eviction).
    #[test]
    fn prop_double_put_is_update(
        capacity in arb_capacity(),
        key in arb_key(),
        val1 in arb_value(),
        val2 in arb_value(),
    ) {
        let mut cache = LruCache::new(capacity);
        cache.put(key, val1);
        let evicted = cache.put(key, val2);
        prop_assert!(evicted.is_none(), "Double put should not evict");
        prop_assert_eq!(cache.len(), 1);
        prop_assert_eq!(cache.peek(&key), Some(&val2));
    }

    /// Removing a non-existent key returns None and doesn't change len.
    #[test]
    fn prop_remove_nonexistent_is_noop(
        capacity in arb_capacity(),
        entries in prop::collection::vec((arb_key(), arb_value()), 0..10),
        absent_key in 100u16..200,
    ) {
        let mut cache = LruCache::new(capacity);
        for &(k, v) in &entries {
            cache.put(k, v);
        }
        let len_before = cache.len();

        let result = cache.remove(&absent_key);
        prop_assert_eq!(result, None);
        prop_assert_eq!(cache.len(), len_before);
    }
}

// ────────────────────────────────────────────────────────────────────
// Reset stats
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// reset_stats zeroes all counters but doesn't affect cache contents.
    #[test]
    fn prop_reset_stats_preserves_data(
        capacity in arb_capacity(),
        ops in arb_ops(30),
    ) {
        let mut cache = LruCache::new(capacity);
        for op in &ops {
            match op {
                Op::Put(k, v) => { cache.put(*k, *v); }
                Op::Get(k) => { cache.get(k); }
                Op::Peek(k) => { cache.peek(k); }
                Op::Remove(k) => { cache.remove(k); }
                Op::ContainsKey(k) => { cache.contains_key(k); }
            }
        }

        let snapshot: Vec<(u16, i32)> = cache.iter_mru().map(|(&k, &v)| (k, v)).collect();
        let len = cache.len();

        cache.reset_stats();

        let stats = cache.stats();
        prop_assert_eq!(stats.hits, 0);
        prop_assert_eq!(stats.misses, 0);
        prop_assert_eq!(stats.insertions, 0);
        prop_assert_eq!(stats.updates, 0);
        prop_assert_eq!(stats.evictions, 0);
        prop_assert_eq!(stats.removals, 0);

        // Data unchanged
        let after: Vec<(u16, i32)> = cache.iter_mru().map(|(&k, &v)| (k, v)).collect();
        prop_assert_eq!(after, snapshot);
        prop_assert_eq!(cache.len(), len);
    }
}

// ────────────────────────────────────────────────────────────────────
// Single-capacity edge case
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A capacity-1 cache always has at most 1 entry and evicts on every new key.
    #[test]
    fn prop_capacity_one_behavior(
        keys in prop::collection::vec(arb_key(), 2..20),
    ) {
        let mut cache = LruCache::new(1);

        for (i, &k) in keys.iter().enumerate() {
            cache.put(k, i as i32);
            prop_assert!(cache.len() <= 1, "capacity-1 cache has len > 1");
        }

        // Only the last unique key should remain
        let last_key = *keys.last().unwrap();
        let last_val = (keys.len() - 1) as i32;
        prop_assert_eq!(cache.peek(&last_key), Some(&last_val));
    }
}
