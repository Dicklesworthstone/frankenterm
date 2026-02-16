#![allow(clippy::needless_range_loop)]
//! Property-based tests for persistent_ds.rs — immutable data structures.
//!
//! Bead: ft-283h4.7.1

use frankenterm_core::persistent_ds::*;
use proptest::prelude::*;
use std::collections::HashMap;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_small_vec_ops() -> impl Strategy<Value = Vec<VecOp>> {
    prop::collection::vec(arb_vec_op(), 1..50)
}

#[derive(Clone, Debug)]
enum VecOp {
    Push(i64),
    Set(usize, i64),
    Pop,
}

fn arb_vec_op() -> impl Strategy<Value = VecOp> {
    prop_oneof![
        any::<i64>().prop_map(VecOp::Push),
        (0..100usize, any::<i64>()).prop_map(|(i, v)| VecOp::Set(i, v)),
        Just(VecOp::Pop),
    ]
}

#[derive(Clone, Debug)]
enum MapOp {
    Insert(String, i64),
    Remove(String),
}

fn arb_map_op() -> impl Strategy<Value = MapOp> {
    prop_oneof![
        ("[a-z]{1,8}", any::<i64>()).prop_map(|(k, v)| MapOp::Insert(k, v)),
        "[a-z]{1,8}".prop_map(MapOp::Remove),
    ]
}

fn arb_map_ops() -> impl Strategy<Value = Vec<MapOp>> {
    prop::collection::vec(arb_map_op(), 1..60)
}

// ── PersistentVec properties ────────────────────────────────────────

proptest! {
    /// Empty vec has length 0 and returns None for all indices.
    #[test]
    fn vec_empty_invariants(idx in 0..100usize) {
        let v: PersistentVec<i32> = PersistentVec::new();
        prop_assert!(v.is_empty());
        prop_assert_eq!(v.len(), 0);
        prop_assert!(v.get(idx).is_none());
    }

    /// Push increments length by 1 and the element is retrievable.
    #[test]
    fn vec_push_increments_len(items in prop::collection::vec(any::<i64>(), 0..100)) {
        let mut v = PersistentVec::new();
        for (i, &item) in items.iter().enumerate() {
            v = v.push(item);
            prop_assert_eq!(v.len(), i + 1);
            prop_assert_eq!(v.get(i), Some(&item));
        }
    }

    /// Push preserves all previous elements.
    #[test]
    fn vec_push_preserves_existing(items in prop::collection::vec(any::<i64>(), 1..80)) {
        let mut v = PersistentVec::new();
        for &item in &items {
            v = v.push(item);
        }
        for (i, &expected) in items.iter().enumerate() {
            prop_assert_eq!(
                v.get(i), Some(&expected),
                "element at index {} should be {}", i, expected
            );
        }
    }

    /// Old versions are unaffected by new pushes (structural sharing).
    #[test]
    fn vec_old_version_preserved(
        items in prop::collection::vec(any::<i64>(), 1..50),
        extra in any::<i64>()
    ) {
        let mut v = PersistentVec::new();
        for &item in &items {
            v = v.push(item);
        }
        let old = v.clone();
        let new = v.push(extra);

        // Old version unchanged
        prop_assert_eq!(old.len(), items.len());
        prop_assert_eq!(new.len(), items.len() + 1);
        for (i, &expected) in items.iter().enumerate() {
            prop_assert_eq!(old.get(i), Some(&expected));
        }
    }

    /// Set produces correct update without affecting original.
    #[test]
    fn vec_set_correctness(
        items in prop::collection::vec(any::<i64>(), 1..50),
        new_val in any::<i64>()
    ) {
        let mut v = PersistentVec::new();
        for &item in &items {
            v = v.push(item);
        }
        let idx = items.len() / 2;
        let old = v.clone();
        let updated = v.set(idx, new_val).unwrap();

        // Old unchanged
        prop_assert_eq!(old.get(idx), Some(&items[idx]));
        // New updated
        prop_assert_eq!(updated.get(idx), Some(&new_val));
        // Other elements unchanged in new
        for i in 0..items.len() {
            if i != idx {
                prop_assert_eq!(updated.get(i), Some(&items[i]));
            }
        }
    }

    /// Set out of bounds returns None.
    #[test]
    fn vec_set_oob(items in prop::collection::vec(any::<i64>(), 0..20)) {
        let mut v = PersistentVec::new();
        for &item in &items {
            v = v.push(item);
        }
        prop_assert!(v.set(items.len() + 10, 0).is_none());
    }

    /// Pop returns last element and reduces length by 1.
    #[test]
    fn vec_pop_correctness(items in prop::collection::vec(any::<i64>(), 1..50)) {
        let mut v = PersistentVec::new();
        for &item in &items {
            v = v.push(item);
        }
        let (popped, last) = v.pop().unwrap();
        prop_assert_eq!(last, *items.last().unwrap());
        prop_assert_eq!(popped.len(), items.len() - 1);
        for i in 0..items.len() - 1 {
            prop_assert_eq!(popped.get(i), Some(&items[i]));
        }
    }

    /// Pop on empty returns None.
    #[test]
    fn vec_pop_empty(_dummy in 0..1u8) {
        let v: PersistentVec<i32> = PersistentVec::new();
        prop_assert!(v.pop().is_none());
    }

    /// Iterator yields all elements in order.
    #[test]
    fn vec_iter_order(items in prop::collection::vec(any::<i64>(), 0..80)) {
        let mut v = PersistentVec::new();
        for &item in &items {
            v = v.push(item);
        }
        let collected: Vec<_> = v.iter().copied().collect();
        prop_assert_eq!(collected, items);
    }

    /// Iterator has correct size_hint.
    #[test]
    fn vec_iter_size_hint(items in prop::collection::vec(any::<i32>(), 0..50)) {
        let v = PersistentVec::from_iter(items.clone());
        let iter = v.iter();
        let (lo, hi) = iter.size_hint();
        prop_assert_eq!(lo, items.len());
        prop_assert_eq!(hi, Some(items.len()));
    }

    /// from_iter produces same result as sequential push.
    #[test]
    fn vec_from_iter_equivalent(items in prop::collection::vec(any::<i32>(), 0..50)) {
        let v1 = PersistentVec::from_iter(items.clone());
        let mut v2 = PersistentVec::new();
        for item in &items {
            v2 = v2.push(*item);
        }
        prop_assert_eq!(v1, v2);
    }

    /// Sequence of arbitrary ops produces same result as Vec reference.
    #[test]
    fn vec_ops_match_reference(ops in arb_small_vec_ops()) {
        let mut persistent = PersistentVec::new();
        let mut reference: Vec<i64> = Vec::new();

        for op in ops {
            match op {
                VecOp::Push(v) => {
                    persistent = persistent.push(v);
                    reference.push(v);
                }
                VecOp::Set(idx, v) => {
                    if idx < reference.len() {
                        persistent = persistent.set(idx, v).unwrap();
                        reference[idx] = v;
                    }
                }
                VecOp::Pop => {
                    if !reference.is_empty() {
                        let (p, _) = persistent.pop().unwrap();
                        persistent = p;
                        reference.pop();
                    }
                }
            }
        }

        prop_assert_eq!(persistent.len(), reference.len());
        for (i, expected) in reference.iter().enumerate() {
            prop_assert_eq!(
                persistent.get(i), Some(expected),
                "mismatch at index {}", i
            );
        }
    }

    /// Equality is value-based, not identity-based.
    #[test]
    fn vec_equality_value_based(items in prop::collection::vec(any::<i32>(), 0..30)) {
        let v1 = PersistentVec::from_iter(items.clone());
        let v2 = PersistentVec::from_iter(items);
        prop_assert_eq!(v1, v2);
    }
}

// ── PersistentMap properties ────────────────────────────────────────

proptest! {
    /// Empty map has length 0 and contains no keys.
    #[test]
    fn map_empty_invariants(key in "[a-z]{1,8}") {
        let m: PersistentMap<String, i32> = PersistentMap::new();
        prop_assert!(m.is_empty());
        prop_assert_eq!(m.len(), 0);
        prop_assert!(m.get(&key).is_none());
        prop_assert!(!m.contains_key(&key));
    }

    /// Insert increments length (for new keys) and value is retrievable.
    #[test]
    fn map_insert_retrievable(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 1..40)
    ) {
        let mut m = PersistentMap::new();
        let mut expected: HashMap<String, i64> = HashMap::new();
        for (k, v) in entries {
            m = m.insert(k.clone(), v);
            expected.insert(k, v);
        }
        prop_assert_eq!(m.len(), expected.len());
        for (k, v) in &expected {
            prop_assert_eq!(m.get(k), Some(v), "key {} should have value {}", k, v);
        }
    }

    /// Old map version is unaffected by inserts on new version.
    #[test]
    fn map_old_version_preserved(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 1..20),
        new_key in "[a-z]{1,8}",
        new_val in any::<i64>()
    ) {
        let mut m = PersistentMap::new();
        for (k, v) in &entries {
            m = m.insert(k.clone(), *v);
        }
        let old = m.clone();
        let old_len = old.len();
        let new = m.insert(new_key, new_val);

        // Old version unchanged
        prop_assert_eq!(old.len(), old_len);
        // Verify old entries still accessible in old
        let expected: HashMap<String, i64> = entries.iter().cloned().collect();
        for (k, _) in &entries {
            if let Some(ev) = expected.get(k) {
                prop_assert_eq!(old.get(k), Some(ev));
            }
        }
        // New version may have +1 length if key was new
        prop_assert!(new.len() >= old_len);
    }

    /// Update (insert with existing key) doesn't change length.
    #[test]
    fn map_update_preserves_len(
        key in "[a-z]{1,8}",
        v1 in any::<i64>(),
        v2 in any::<i64>()
    ) {
        let m = PersistentMap::new().insert(key.clone(), v1);
        let m2 = m.insert(key.clone(), v2);
        prop_assert_eq!(m.len(), 1);
        prop_assert_eq!(m2.len(), 1);
        prop_assert_eq!(m.get(&key), Some(&v1));
        prop_assert_eq!(m2.get(&key), Some(&v2));
    }

    /// Remove decrements length and key is no longer found.
    #[test]
    fn map_remove_correctness(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 2..20)
    ) {
        let unique: HashMap<String, i64> = entries.into_iter().collect();
        if unique.len() < 2 {
            return Ok(());
        }
        let mut m = PersistentMap::new();
        for (k, v) in &unique {
            m = m.insert(k.clone(), *v);
        }
        let key_to_remove = unique.keys().next().unwrap().clone();
        let old = m.clone();
        let removed = m.remove(&key_to_remove);

        prop_assert_eq!(old.len(), unique.len());
        prop_assert_eq!(removed.len(), unique.len() - 1);
        prop_assert!(removed.get(&key_to_remove).is_none());
        // Other keys still present
        for k in unique.keys() {
            if k != &key_to_remove {
                prop_assert!(removed.contains_key(k));
            }
        }
    }

    /// Remove nonexistent key is a no-op.
    #[test]
    fn map_remove_nonexistent(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 1..10),
        absent in "[A-Z]{1,8}"  // uppercase, won't collide
    ) {
        let mut m = PersistentMap::new();
        for (k, v) in &entries {
            m = m.insert(k.clone(), *v);
        }
        let m2 = m.remove(&absent);
        prop_assert_eq!(m.len(), m2.len());
    }

    /// Sequence of arbitrary ops matches HashMap reference.
    #[test]
    fn map_ops_match_reference(ops in arb_map_ops()) {
        let mut persistent = PersistentMap::new();
        let mut reference: HashMap<String, i64> = HashMap::new();

        for op in ops {
            match op {
                MapOp::Insert(k, v) => {
                    persistent = persistent.insert(k.clone(), v);
                    reference.insert(k, v);
                }
                MapOp::Remove(k) => {
                    persistent = persistent.remove(&k);
                    reference.remove(&k);
                }
            }
        }

        prop_assert_eq!(persistent.len(), reference.len(),
            "len mismatch: persistent={}, reference={}", persistent.len(), reference.len());
        for (k, v) in &reference {
            prop_assert_eq!(
                persistent.get(k), Some(v),
                "key {} should have value {}", k, v
            );
        }
    }

    /// contains_key is consistent with get.
    #[test]
    fn map_contains_key_consistent(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 0..20),
        query in "[a-z]{1,8}"
    ) {
        let mut m = PersistentMap::new();
        for (k, v) in &entries {
            m = m.insert(k.clone(), *v);
        }
        prop_assert_eq!(m.contains_key(&query), m.get(&query).is_some());
    }

    /// entries() returns all key-value pairs.
    #[test]
    fn map_entries_complete(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 1..30)
    ) {
        let unique: HashMap<String, i64> = entries.into_iter().collect();
        let mut m = PersistentMap::new();
        for (k, v) in &unique {
            m = m.insert(k.clone(), *v);
        }
        let result = m.entries();
        prop_assert_eq!(result.len(), unique.len());
        for (k, v) in result {
            prop_assert_eq!(unique.get(k), Some(v));
        }
    }

    /// from_entries produces correct map.
    #[test]
    fn map_from_entries_correct(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 0..30)
    ) {
        let unique: HashMap<String, i64> = entries.clone().into_iter().collect();
        let m = PersistentMap::from_entries(entries);
        prop_assert_eq!(m.len(), unique.len());
        for (k, v) in &unique {
            prop_assert_eq!(m.get(k), Some(v));
        }
    }

    /// Insert then remove restores original (for new key).
    #[test]
    fn map_insert_remove_roundtrip(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 1..10),
        new_key in "[A-Z]{1,8}",  // won't collide with lowercase
        new_val in any::<i64>()
    ) {
        let mut m = PersistentMap::new();
        for (k, v) in &entries {
            m = m.insert(k.clone(), *v);
        }
        let original_len = m.len();
        let with_new = m.insert(new_key.clone(), new_val);
        let without_new = with_new.remove(&new_key);
        prop_assert_eq!(without_new.len(), original_len);
    }

    /// Equality is value-based.
    #[test]
    fn map_equality_value_based(
        entries in prop::collection::vec(("[a-z]{1,8}", any::<i64>()), 0..20)
    ) {
        let m1 = PersistentMap::from_entries(entries.clone());
        let m2 = PersistentMap::from_entries(entries);
        prop_assert_eq!(m1, m2);
    }

    /// Integer keys work correctly.
    #[test]
    fn map_integer_keys(entries in prop::collection::vec((0..1000u64, any::<i64>()), 1..30)) {
        let unique: HashMap<u64, i64> = entries.into_iter().collect();
        let mut m = PersistentMap::new();
        for (&k, &v) in &unique {
            m = m.insert(k, v);
        }
        prop_assert_eq!(m.len(), unique.len());
        for (&k, &v) in &unique {
            prop_assert_eq!(m.get(&k), Some(&v));
        }
    }
}

// ── VersionedStore properties ───────────────────────────────────────

proptest! {
    /// Current always points to the last pushed version.
    #[test]
    fn versioned_current_is_latest(
        values in prop::collection::vec(any::<i64>(), 1..20)
    ) {
        let mut store = VersionedStore::new(values[0], 0);
        for (i, &v) in values.iter().enumerate().skip(1) {
            store.push(v, (i as u64) * 1000);
        }
        prop_assert_eq!(store.current(), values.last().unwrap());
        prop_assert_eq!(store.version_number(), values.len() - 1);
    }

    /// at_version(i) returns the i-th value.
    #[test]
    fn versioned_at_version(values in prop::collection::vec(any::<i64>(), 1..20)) {
        let mut store = VersionedStore::new(values[0], 0);
        for (i, &v) in values.iter().enumerate().skip(1) {
            store.push(v, (i as u64) * 1000);
        }
        for (i, &v) in values.iter().enumerate() {
            prop_assert_eq!(store.at_version(i), Some(&v));
        }
    }

    /// version_count matches number of pushes + 1 (initial).
    #[test]
    fn versioned_count(count in 1..30usize) {
        let mut store = VersionedStore::new(0i32, 0);
        for i in 1..count {
            store.push(i as i32, i as u64);
        }
        prop_assert_eq!(store.version_count(), count);
    }

    /// at_timestamp finds the exact match.
    #[test]
    fn versioned_exact_timestamp(
        values in prop::collection::vec(any::<i64>(), 2..10)
    ) {
        let mut store = VersionedStore::new(values[0], 1000);
        for (i, &v) in values.iter().enumerate().skip(1) {
            store.push(v, 1000 + (i as u64) * 1000);
        }
        // Exact timestamp lookup
        for (i, &v) in values.iter().enumerate() {
            let ts = 1000 + (i as u64) * 1000;
            prop_assert_eq!(store.at_timestamp(ts), Some(&v));
        }
    }

    /// evict_before reduces version count but keeps current.
    #[test]
    fn versioned_evict_keeps_current(
        values in prop::collection::vec(any::<i64>(), 3..15)
    ) {
        let mut store = VersionedStore::new(values[0], 0);
        for (i, &v) in values.iter().enumerate().skip(1) {
            store.push(v, (i as u64) * 1000);
        }
        let before = store.version_count();
        store.evict_before(((values.len() / 2) as u64) * 1000);
        prop_assert!(store.version_count() <= before);
        // Current version still accessible
        prop_assert_eq!(store.current(), values.last().unwrap());
    }

    /// iter_versions returns all versions in order.
    #[test]
    fn versioned_iter_ordered(values in prop::collection::vec(any::<i64>(), 1..15)) {
        let mut store = VersionedStore::new(values[0], 0);
        for (i, &v) in values.iter().enumerate().skip(1) {
            store.push(v, (i as u64) * 100);
        }
        let versions: Vec<_> = store.iter_versions().collect();
        // Timestamps are non-decreasing
        for i in 1..versions.len() {
            prop_assert!(versions[i].0 >= versions[i - 1].0);
        }
        // Values match
        for (i, (_, v)) in versions.iter().enumerate() {
            prop_assert_eq!(*v, &values[i]);
        }
    }

    /// timestamp_at returns correct timestamps.
    #[test]
    fn versioned_timestamp_at(count in 1..10usize) {
        let mut store = VersionedStore::new(0i32, 100);
        for i in 1..count {
            store.push(i as i32, 100 + (i as u64) * 100);
        }
        for i in 0..count {
            prop_assert_eq!(store.timestamp_at(i), Some(100 + (i as u64) * 100));
        }
        prop_assert!(store.timestamp_at(count + 1).is_none());
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// PersistentVec: push N items then pop N items yields empty.
    #[test]
    fn vec_push_pop_symmetry(items in prop::collection::vec(any::<i64>(), 1..30)) {
        let mut v = PersistentVec::new();
        for &item in &items {
            v = v.push(item);
        }
        for _ in 0..items.len() {
            let (new_v, _) = v.pop().unwrap();
            v = new_v;
        }
        prop_assert!(v.is_empty());
    }

    /// PersistentMap with VersionedStore: versions preserve correct state.
    #[test]
    fn map_versioned_integration(
        ops in prop::collection::vec(("[a-z]{1,4}", any::<i64>()), 1..20)
    ) {
        let initial = PersistentMap::<String, i64>::new();
        let mut store = VersionedStore::new(initial, 0);
        let mut current = store.current().clone();

        for (i, (k, v)) in ops.iter().enumerate() {
            current = current.insert(k.clone(), *v);
            store.push(current.clone(), (i as u64 + 1) * 1000);
        }

        // Each version should have correct cumulative state
        let mut expected = PersistentMap::new();
        for (i, (k, v)) in ops.iter().enumerate() {
            expected = expected.insert(k.clone(), *v);
            let version_state = store.at_version(i + 1).unwrap();
            prop_assert_eq!(version_state.len(), expected.len(),
                "version {} len mismatch", i + 1);
        }
    }

    /// Diff between two versions correctly identifies all changes.
    #[test]
    fn map_diff_completeness(
        base_entries in prop::collection::vec(("[a-z]{1,4}", any::<i64>()), 1..15),
        extra_entries in prop::collection::vec(("[a-z]{1,4}", any::<i64>()), 1..10)
    ) {
        let m1 = PersistentMap::from_entries(base_entries);
        let mut m2 = m1.clone();
        for (k, v) in &extra_entries {
            m2 = m2.insert(k.clone(), *v);
        }
        let diff = m1.diff(&m2);
        // Every added key should be in m2 but not m1
        for (k, v) in &diff.added {
            prop_assert!(m2.get(k) == Some(v));
            prop_assert!(m1.get(k).is_none());
        }
        // Every removed key should be in m1 but not m2
        for k in &diff.removed {
            prop_assert!(m1.contains_key(k));
            prop_assert!(!m2.contains_key(k));
        }
        // Every changed key has different values in m1 and m2
        for (k, new_v) in &diff.changed {
            let old_v = m1.get(k);
            prop_assert!(old_v.is_some());
            prop_assert!(old_v != Some(new_v));
            prop_assert_eq!(m2.get(k), Some(new_v));
        }
    }
}
