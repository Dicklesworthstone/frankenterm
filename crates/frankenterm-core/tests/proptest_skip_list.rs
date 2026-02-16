//! Property-based tests for skip_list.rs — probabilistic ordered map.
//!
//! Bead: ft-283h4.19

use frankenterm_core::skip_list::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

// ── Strategies ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum ListOp {
    Insert(i64, i64),
    Remove(i64),
    Get(i64),
}

fn arb_op() -> impl Strategy<Value = ListOp> {
    prop_oneof![
        (0..1000i64, any::<i64>()).prop_map(|(k, v)| ListOp::Insert(k, v)),
        (0..1000i64).prop_map(ListOp::Remove),
        (0..1000i64).prop_map(ListOp::Get),
    ]
}

fn arb_ops() -> impl Strategy<Value = Vec<ListOp>> {
    prop::collection::vec(arb_op(), 1..50)
}

fn arb_insert_ops() -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((0..10000i64, any::<i64>()), 1..50)
}

fn arb_seed() -> impl Strategy<Value = u64> {
    any::<u64>()
}

/// Execute ops on both SkipList and BTreeMap, verify consistency.
fn execute_and_compare(seed: u64, ops: &[ListOp]) {
    let mut sl = SkipList::new(seed);
    let mut bt = BTreeMap::new();

    for op in ops {
        match op {
            ListOp::Insert(k, v) => {
                let sl_old = sl.insert(*k, *v);
                let bt_old = bt.insert(*k, *v);
                assert_eq!(sl_old, bt_old, "insert({}) old value mismatch", k);
            }
            ListOp::Remove(k) => {
                let sl_val = sl.remove(k);
                let bt_val = bt.remove(k);
                assert_eq!(sl_val, bt_val, "remove({}) mismatch", k);
            }
            ListOp::Get(k) => {
                let sl_val = sl.get(k);
                let bt_val = bt.get(k);
                assert_eq!(sl_val, bt_val, "get({}) mismatch", k);
            }
        }
    }

    assert_eq!(sl.len(), bt.len(), "len mismatch");
}

// ── Model equivalence (BTreeMap oracle) ─────────────────────────────

proptest! {
    /// SkipList matches BTreeMap for any sequence of operations.
    #[test]
    fn model_equivalence(seed in arb_seed(), ops in arb_ops()) {
        execute_and_compare(seed, &ops);
    }

    /// Iteration order matches BTreeMap ordering.
    #[test]
    fn iteration_order_matches_btree(seed in arb_seed(), ops in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        let mut bt = BTreeMap::new();

        for (k, v) in &ops {
            sl.insert(*k, *v);
            bt.insert(*k, *v);
        }

        let sl_items: Vec<_> = sl.iter().map(|(k, v)| (*k, *v)).collect();
        let bt_items: Vec<_> = bt.iter().map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(sl_items, bt_items);
    }
}

// ── Ordering properties ─────────────────────────────────────────────

proptest! {
    /// Iteration is always in ascending key order.
    #[test]
    fn iter_ascending(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let keys: Vec<_> = sl.iter().map(|(k, _)| *k).collect();
        for i in 1..keys.len() {
            prop_assert!(
                keys[i] > keys[i - 1],
                "keys not ascending at index {}: {} <= {}", i, keys[i], keys[i-1]
            );
        }
    }

    /// min() returns the smallest key.
    #[test]
    fn min_is_smallest(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        if let Some((min_k, _)) = sl.min() {
            for (k, _) in sl.iter() {
                prop_assert!(
                    k >= min_k,
                    "found key {} < min {}", k, min_k
                );
            }
        }
    }

    /// max() returns the largest key.
    #[test]
    fn max_is_largest(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        if let Some((max_k, _)) = sl.max() {
            for (k, _) in sl.iter() {
                prop_assert!(
                    k <= max_k,
                    "found key {} > max {}", k, max_k
                );
            }
        }
    }
}

// ── Insert properties ───────────────────────────────────────────────

proptest! {
    /// Insert increments len for new keys, not for existing.
    #[test]
    fn insert_len_semantics(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        let mut expected_len = 0;
        let mut seen = std::collections::HashSet::new();

        for (k, v) in &pairs {
            let old = sl.insert(*k, *v);
            if old.is_none() {
                expected_len += 1;
            }
            seen.insert(*k);
            prop_assert_eq!(sl.len(), expected_len);
        }

        prop_assert_eq!(sl.len(), seen.len());
    }

    /// Inserted values are retrievable.
    #[test]
    fn insert_get_roundtrip(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        let mut latest = BTreeMap::new();

        for (k, v) in &pairs {
            sl.insert(*k, *v);
            latest.insert(*k, *v);
        }

        for (k, v) in &latest {
            prop_assert_eq!(sl.get(k), Some(v), "key {} value mismatch", k);
        }
    }

    /// Insert with same key updates value.
    #[test]
    fn insert_updates_value(
        seed in arb_seed(),
        key in 0..1000i64,
        v1 in any::<i64>(),
        v2 in any::<i64>()
    ) {
        let mut sl = SkipList::new(seed);
        sl.insert(key, v1);
        let old = sl.insert(key, v2);
        prop_assert_eq!(old, Some(v1));
        prop_assert_eq!(sl.get(&key), Some(&v2));
        prop_assert_eq!(sl.len(), 1);
    }
}

// ── Remove properties ───────────────────────────────────────────────

proptest! {
    /// Remove decrements len.
    #[test]
    fn remove_decrements_len(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        let mut keys = Vec::new();
        for (k, v) in &pairs {
            if sl.insert(*k, *v).is_none() {
                keys.push(*k);
            }
        }

        for key in &keys {
            let before = sl.len();
            let removed = sl.remove(key);
            prop_assert!(removed.is_some(), "key {} should exist", key);
            prop_assert_eq!(sl.len(), before - 1);
        }

        prop_assert!(sl.is_empty());
    }

    /// Remove nonexistent key returns None.
    #[test]
    fn remove_nonexistent(seed in arb_seed(), key in 0..1000i64) {
        let mut sl: SkipList<i64, i64> = SkipList::new(seed);
        prop_assert!(sl.remove(&key).is_none());
    }

    /// After remove, get returns None.
    #[test]
    fn remove_then_get_none(
        seed in arb_seed(),
        key in 0..1000i64,
        value in any::<i64>()
    ) {
        let mut sl = SkipList::new(seed);
        sl.insert(key, value);
        sl.remove(&key);
        prop_assert!(sl.get(&key).is_none());
    }
}

// ── Range properties ────────────────────────────────────────────────

proptest! {
    /// Range results are within bounds.
    #[test]
    fn range_within_bounds(
        seed in arb_seed(),
        pairs in arb_insert_ops(),
        from in 0..500i64,
        span in 0..500i64
    ) {
        let to = from + span;
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let range = sl.range(&from, &to);
        for (k, _) in &range {
            prop_assert!(
                **k >= from && **k <= to,
                "key {} outside range [{}, {}]", k, from, to
            );
        }
    }

    /// Range results are in ascending order.
    #[test]
    fn range_ordered(
        seed in arb_seed(),
        pairs in arb_insert_ops(),
        from in 0..500i64,
        span in 0..500i64
    ) {
        let to = from + span;
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let range = sl.range(&from, &to);
        for i in 1..range.len() {
            prop_assert!(range[i].0 > range[i - 1].0);
        }
    }

    /// Range matches BTreeMap range.
    #[test]
    fn range_matches_btree(
        seed in arb_seed(),
        pairs in arb_insert_ops(),
        from in 0..500i64,
        span in 0..500i64
    ) {
        let to = from + span;
        let mut sl = SkipList::new(seed);
        let mut bt = BTreeMap::new();
        for (k, v) in &pairs {
            sl.insert(*k, *v);
            bt.insert(*k, *v);
        }

        let sl_range: Vec<_> = sl.range(&from, &to).into_iter().map(|(k, v)| (*k, *v)).collect();
        let bt_range: Vec<_> = bt.range(from..=to).map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(sl_range, bt_range);
    }
}

// ── Clear properties ────────────────────────────────────────────────

proptest! {
    /// Clear empties the list.
    #[test]
    fn clear_empties(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        sl.clear();
        prop_assert!(sl.is_empty());
        prop_assert_eq!(sl.len(), 0);
        prop_assert!(sl.min().is_none());
        prop_assert!(sl.max().is_none());
    }
}

// ── Determinism properties ──────────────────────────────────────────

proptest! {
    /// Same seed + same operations = same structure.
    #[test]
    fn deterministic(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl1 = SkipList::new(seed);
        let mut sl2 = SkipList::new(seed);

        for (k, v) in &pairs {
            sl1.insert(*k, *v);
            sl2.insert(*k, *v);
        }

        prop_assert_eq!(sl1.len(), sl2.len());
        prop_assert_eq!(sl1.current_level(), sl2.current_level());

        let items1: Vec<_> = sl1.iter().map(|(k, v)| (*k, *v)).collect();
        let items2: Vec<_> = sl2.iter().map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(items1, items2);
    }
}

// ── Stats properties ────────────────────────────────────────────────

proptest! {
    /// Stats len matches list len.
    #[test]
    fn stats_len_matches(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let stats = sl.stats();
        prop_assert_eq!(stats.len, sl.len());
        prop_assert_eq!(stats.current_level, sl.current_level());
    }

    /// SkipListStats serde roundtrip.
    #[test]
    fn stats_serde(
        len in 0..1000usize,
        level in 0..16usize,
        nodes in 1..2000usize,
        free in 0..500usize
    ) {
        let stats = SkipListStats { len, current_level: level, total_nodes: nodes, free_slots: free };
        let json = serde_json::to_string(&stats).unwrap();
        let back: SkipListStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// contains_key matches get().is_some().
    #[test]
    fn contains_key_matches_get(seed in arb_seed(), ops in arb_ops()) {
        let mut sl = SkipList::new(seed);
        for op in &ops {
            match op {
                ListOp::Insert(k, v) => { sl.insert(*k, *v); }
                ListOp::Remove(k) => { sl.remove(k); }
                ListOp::Get(_) => {}
            }
        }
        for key in 0..100i64 {
            prop_assert_eq!(
                sl.contains_key(&key),
                sl.get(&key).is_some(),
                "contains_key and get disagree for key {}", key
            );
        }
    }

    /// Iter count matches len.
    #[test]
    fn iter_count_matches_len(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        prop_assert_eq!(sl.iter().count(), sl.len());
    }

    /// min <= max when non-empty.
    #[test]
    fn min_le_max(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        if let (Some((min_k, _)), Some((max_k, _))) = (sl.min(), sl.max()) {
            prop_assert!(min_k <= max_k);
        }
    }

    /// is_empty agrees with len == 0.
    #[test]
    fn is_empty_agrees_with_len(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        prop_assert_eq!(sl.is_empty(), sl.len() == 0);
    }

    /// Double remove returns None.
    #[test]
    fn double_remove_none(seed in arb_seed(), key in 0..1000i64, val in any::<i64>()) {
        let mut sl = SkipList::new(seed);
        sl.insert(key, val);
        let _ = sl.remove(&key);
        prop_assert!(sl.remove(&key).is_none());
    }

    /// After clear, insertions work normally.
    #[test]
    fn insert_after_clear(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        sl.clear();
        sl.insert(42, 100);
        prop_assert_eq!(sl.len(), 1);
        prop_assert_eq!(sl.get(&42), Some(&100));
    }

    /// Range with inverted bounds returns empty.
    #[test]
    fn range_inverted_empty(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let range = sl.range(&500, &100);
        prop_assert!(range.is_empty());
    }

    /// min/max after single insert.
    #[test]
    fn min_max_single(seed in arb_seed(), key in 0..1000i64, val in any::<i64>()) {
        let mut sl = SkipList::new(seed);
        sl.insert(key, val);
        let (min_k, min_v) = sl.min().unwrap();
        let (max_k, max_v) = sl.max().unwrap();
        prop_assert_eq!(*min_k, key);
        prop_assert_eq!(*min_v, val);
        prop_assert_eq!(*max_k, key);
        prop_assert_eq!(*max_v, val);
    }

    /// Stats total_nodes >= len.
    #[test]
    fn stats_nodes_ge_len(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let stats = sl.stats();
        prop_assert!(stats.total_nodes >= stats.len, "nodes {} < len {}", stats.total_nodes, stats.len);
    }

    /// Remove all then verify empty.
    #[test]
    fn remove_all_yields_empty(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        let mut bt = BTreeMap::new();
        for (k, v) in &pairs {
            sl.insert(*k, *v);
            bt.insert(*k, *v);
        }
        for key in bt.keys() {
            sl.remove(key);
        }
        prop_assert!(sl.is_empty());
        prop_assert_eq!(sl.len(), 0);
    }

    /// current_level grows with insertions.
    #[test]
    fn current_level_grows(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        let empty_level = sl.current_level();
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        if !pairs.is_empty() {
            prop_assert!(sl.current_level() >= empty_level);
        }
    }

    /// Different seeds produce same logical content.
    #[test]
    fn different_seeds_same_content(
        seed1 in arb_seed(),
        seed2 in arb_seed(),
        pairs in arb_insert_ops()
    ) {
        let mut sl1 = SkipList::new(seed1);
        let mut sl2 = SkipList::new(seed2);
        for (k, v) in &pairs {
            sl1.insert(*k, *v);
            sl2.insert(*k, *v);
        }
        let items1: Vec<_> = sl1.iter().map(|(k, v)| (*k, *v)).collect();
        let items2: Vec<_> = sl2.iter().map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(items1, items2);
    }
}

// ── Additional invariants (DarkMill ft-283h4.54) ────────────────────

proptest! {
    /// current_level is always < MAX_LEVEL (16).
    #[test]
    fn level_bounded(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        prop_assert!(sl.current_level() < 16, "level {} >= 16", sl.current_level());
    }

    /// min() == iter().next() for non-empty lists.
    #[test]
    fn min_matches_iter_first(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let min_pair = sl.min().map(|(k, v)| (*k, *v));
        let iter_first = sl.iter().next().map(|(k, v)| (*k, *v));
        prop_assert_eq!(min_pair, iter_first);
    }

    /// max() == iter().last() for non-empty lists.
    #[test]
    fn max_matches_iter_last(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let max_pair = sl.max().map(|(k, v)| (*k, *v));
        let iter_last = sl.iter().last().map(|(k, v)| (*k, *v));
        prop_assert_eq!(max_pair, iter_last);
    }

    /// range(min, max) returns all elements.
    #[test]
    fn range_min_max_returns_all(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        if let (Some((min_k, _)), Some((max_k, _))) = (sl.min(), sl.max()) {
            let range = sl.range(min_k, max_k);
            prop_assert_eq!(range.len(), sl.len());
        }
    }

    /// Inserting same key N times keeps len at 1.
    #[test]
    fn insert_same_key_idempotent_len(
        seed in arb_seed(),
        key in 0..1000i64,
        values in prop::collection::vec(any::<i64>(), 2..10)
    ) {
        let mut sl = SkipList::new(seed);
        for v in &values {
            sl.insert(key, *v);
        }
        prop_assert_eq!(sl.len(), 1);
        prop_assert_eq!(sl.get(&key), Some(values.last().unwrap()));
    }

    /// Remove then reinsert works correctly.
    #[test]
    fn remove_then_reinsert(
        seed in arb_seed(),
        key in 0..1000i64,
        v1 in any::<i64>(),
        v2 in any::<i64>()
    ) {
        let mut sl = SkipList::new(seed);
        sl.insert(key, v1);
        sl.remove(&key);
        prop_assert!(sl.get(&key).is_none());
        sl.insert(key, v2);
        prop_assert_eq!(sl.get(&key), Some(&v2));
        prop_assert_eq!(sl.len(), 1);
    }

    /// Every key in iter() is retrievable via get().
    #[test]
    fn iter_keys_all_gettable(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        for (k, v) in sl.iter() {
            let got = sl.get(k);
            prop_assert_eq!(got, Some(v));
        }
    }

    /// Range completeness: all keys in bounds appear in range result.
    #[test]
    fn range_complete(
        seed in arb_seed(),
        pairs in arb_insert_ops(),
        from in 0..500i64,
        span in 0..500i64
    ) {
        let to = from + span;
        let mut sl = SkipList::new(seed);
        let mut bt = BTreeMap::new();
        for (k, v) in &pairs {
            sl.insert(*k, *v);
            bt.insert(*k, *v);
        }
        let range = sl.range(&from, &to);
        let range_keys: std::collections::HashSet<i64> = range.iter().map(|(k, _)| **k).collect();
        for k in bt.keys() {
            if *k >= from && *k <= to {
                prop_assert!(range_keys.contains(k), "key {} missing from range", k);
            }
        }
    }

    /// Clone produces an independent copy with identical content.
    #[test]
    fn clone_equivalence(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        let cloned = sl.clone();
        prop_assert_eq!(sl.len(), cloned.len());
        let items1: Vec<_> = sl.iter().map(|(k, v)| (*k, *v)).collect();
        let items2: Vec<_> = cloned.iter().map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(items1, items2);
    }

    /// After clear, stats show zero length and zero level.
    #[test]
    fn clear_resets_stats(seed in arb_seed(), pairs in arb_insert_ops()) {
        let mut sl = SkipList::new(seed);
        for (k, v) in &pairs {
            sl.insert(*k, *v);
        }
        sl.clear();
        let stats = sl.stats();
        prop_assert_eq!(stats.len, 0);
        prop_assert_eq!(stats.current_level, 0);
        prop_assert_eq!(stats.free_slots, 0);
    }

    /// Range on single point returns 1 if key exists, 0 otherwise.
    #[test]
    fn range_single_point(
        seed in arb_seed(),
        pairs in arb_insert_ops(),
        probe in 0..10000i64
    ) {
        let mut sl = SkipList::new(seed);
        let mut bt = BTreeMap::new();
        for (k, v) in &pairs {
            sl.insert(*k, *v);
            bt.insert(*k, *v);
        }
        let range = sl.range(&probe, &probe);
        let expected = if bt.contains_key(&probe) { 1 } else { 0 };
        prop_assert_eq!(range.len(), expected);
    }

    /// Interleaved model: every intermediate state matches BTreeMap.
    #[test]
    fn interleaved_model_every_step(seed in arb_seed(), ops in arb_ops()) {
        let mut sl = SkipList::new(seed);
        let mut bt = BTreeMap::new();

        for op in &ops {
            match op {
                ListOp::Insert(k, v) => {
                    sl.insert(*k, *v);
                    bt.insert(*k, *v);
                }
                ListOp::Remove(k) => {
                    sl.remove(k);
                    bt.remove(k);
                }
                ListOp::Get(k) => {
                    prop_assert_eq!(sl.get(k), bt.get(k));
                }
            }
            prop_assert_eq!(sl.len(), bt.len());
        }
    }

    /// Stats total_nodes = 1 (head) + len + free_slots.
    #[test]
    fn stats_node_accounting(seed in arb_seed(), ops in arb_ops()) {
        let mut sl = SkipList::new(seed);
        for op in &ops {
            match op {
                ListOp::Insert(k, v) => { sl.insert(*k, *v); }
                ListOp::Remove(k) => { sl.remove(k); }
                ListOp::Get(_) => {}
            }
        }
        let stats = sl.stats();
        prop_assert_eq!(
            stats.total_nodes,
            1 + stats.len + stats.free_slots,
            "node accounting: total {} != 1 + {} + {}",
            stats.total_nodes, stats.len, stats.free_slots
        );
    }
}
