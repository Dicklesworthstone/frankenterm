//! Property-based tests for `van_emde_boas` module.
//!
//! Verifies correctness invariants against BTreeSet reference:
//! - Contains matches reference
//! - Min/max match reference
//! - Successor/predecessor match reference
//! - Insert/remove semantics
//! - Iteration order
//! - Serde roundtrip
//! - Clear, boundary, singleton, interleaved operations
//! - Display trait, universe_size preservation

use frankenterm_core::van_emde_boas::VanEmdeBoas;
use proptest::prelude::*;
use std::collections::BTreeSet;

// ── Strategies ─────────────────────────────────────────────────────────

fn values_strategy(universe: u32, max_len: usize) -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0..universe, 0..max_len)
}

fn build_reference(vals: &[u32]) -> BTreeSet<u32> {
    vals.iter().copied().collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Contains matches BTreeSet ─────────────────────────────────

    #[test]
    fn contains_matches(
        vals in values_strategy(256, 50),
        probe in 0u32..256
    ) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        prop_assert_eq!(veb.contains(probe), reference.contains(&probe));
    }

    // ── Length matches BTreeSet ────────────────────────────────────

    #[test]
    fn len_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        prop_assert_eq!(veb.len(), reference.len());
    }

    // ── Min matches BTreeSet ──────────────────────────────────────

    #[test]
    fn min_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.iter().next().copied();
        prop_assert_eq!(veb.min(), expected);
    }

    // ── Max matches BTreeSet ──────────────────────────────────────

    #[test]
    fn max_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.iter().next_back().copied();
        prop_assert_eq!(veb.max(), expected);
    }

    // ── Successor matches BTreeSet ────────────────────────────────

    #[test]
    fn successor_matches(
        vals in values_strategy(256, 30),
        probe in 0u32..256
    ) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.range((probe + 1)..).next().copied();
        prop_assert_eq!(veb.successor(probe), expected, "successor({}) mismatch", probe);
    }

    // ── Predecessor matches BTreeSet ──────────────────────────────

    #[test]
    fn predecessor_matches(
        vals in values_strategy(256, 30),
        probe in 0u32..256
    ) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.range(..probe).next_back().copied();
        prop_assert_eq!(veb.predecessor(probe), expected, "predecessor({}) mismatch", probe);
    }

    // ── Insert returns correct boolean ────────────────────────────

    #[test]
    fn insert_returns_correct(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        let mut reference = BTreeSet::new();

        for &v in &vals {
            let veb_new = veb.insert(v);
            let ref_new = reference.insert(v);
            prop_assert_eq!(veb_new, ref_new, "insert({}) mismatch", v);
        }
    }

    // ── Remove returns correct boolean ────────────────────────────

    #[test]
    fn remove_returns_correct(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        let mut reference = BTreeSet::new();

        // Insert all
        for &v in &vals {
            veb.insert(v);
            reference.insert(v);
        }

        // Remove in reverse order
        for &v in vals.iter().rev() {
            let veb_had = veb.remove(v);
            let ref_had = reference.remove(&v);
            prop_assert_eq!(veb_had, ref_had, "remove({}) mismatch", v);
        }
    }

    // ── Iteration order matches ───────────────────────────────────

    #[test]
    fn iter_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let veb_sorted = veb.iter();
        let ref_sorted: Vec<u32> = reference.iter().copied().collect();
        prop_assert_eq!(veb_sorted, ref_sorted);
    }

    // ── Remove preserves others ───────────────────────────────────

    #[test]
    fn remove_preserves_others(vals in values_strategy(256, 30)) {
        if vals.is_empty() {
            return Ok(());
        }

        let mut veb = VanEmdeBoas::new(256);
        let mut reference: BTreeSet<u32> = BTreeSet::new();
        for &v in &vals {
            veb.insert(v);
            reference.insert(v);
        }

        let key = vals[0];
        veb.remove(key);
        reference.remove(&key);

        for &v in &reference {
            prop_assert!(veb.contains(v), "missing {} after removing {}", v, key);
        }
    }

    // ── Serde roundtrip ───────────────────────────────────────────

    #[test]
    fn serde_roundtrip(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let json = serde_json::to_string(&veb).unwrap();
        let restored: VanEmdeBoas = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), veb.len());
        prop_assert_eq!(restored.min(), veb.min());
        prop_assert_eq!(restored.max(), veb.max());
        prop_assert_eq!(restored.iter(), veb.iter());
    }

    // ── Empty operations ──────────────────────────────────────────

    #[test]
    fn empty_operations(probe in 0u32..256) {
        let veb = VanEmdeBoas::new(256);
        prop_assert!(veb.is_empty());
        prop_assert!(!veb.contains(probe));
        prop_assert!(veb.successor(probe).is_none());
        prop_assert!(veb.predecessor(probe).is_none());
        prop_assert!(veb.min().is_none());
        prop_assert!(veb.max().is_none());
    }

    // ── Successor chain covers all elements ───────────────────────

    #[test]
    fn successor_chain(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        if reference.is_empty() {
            return Ok(());
        }

        // Walk successor chain from min
        let mut current = veb.min();
        let mut chain = Vec::new();
        while let Some(val) = current {
            chain.push(val);
            current = veb.successor(val);
        }

        let ref_sorted: Vec<u32> = reference.iter().copied().collect();
        prop_assert_eq!(chain, ref_sorted, "successor chain doesn't match sorted set");
    }

    // ── Predecessor chain covers all elements ─────────────────────

    #[test]
    fn predecessor_chain(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        if reference.is_empty() {
            return Ok(());
        }

        // Walk predecessor chain from max
        let mut current = veb.max();
        let mut chain = Vec::new();
        while let Some(val) = current {
            chain.push(val);
            current = veb.predecessor(val);
        }
        chain.reverse();

        let ref_sorted: Vec<u32> = reference.iter().copied().collect();
        prop_assert_eq!(chain, ref_sorted, "predecessor chain doesn't match sorted set");
    }

    // ── Larger universe ───────────────────────────────────────────

    #[test]
    fn larger_universe(vals in values_strategy(4096, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(4096);
        for &v in &vals {
            veb.insert(v);
        }

        prop_assert_eq!(veb.len(), reference.len());
        prop_assert_eq!(veb.min(), reference.iter().next().copied());
        prop_assert_eq!(veb.max(), reference.iter().next_back().copied());
    }

    // ══════════════════════════════════════════════════════════════
    // NEW TESTS (16-32)
    // ══════════════════════════════════════════════════════════════

    // ── Clear resets to empty ─────────────────────────────────────

    #[test]
    fn clear_resets_all(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        veb.clear();

        prop_assert!(veb.is_empty());
        prop_assert_eq!(veb.len(), 0);
        prop_assert!(veb.min().is_none());
        prop_assert!(veb.max().is_none());
        prop_assert!(veb.iter().is_empty());
    }

    // ── Double insert is idempotent for length ───────────────────

    #[test]
    fn double_insert_idempotent(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let len_after_first = veb.len();

        // Insert all again
        for &v in &vals {
            let was_new = veb.insert(v);
            prop_assert!(!was_new, "double insert of {} returned true", v);
        }

        prop_assert_eq!(veb.len(), len_after_first, "len changed after double insert");
    }

    // ── Double remove returns false on second call ───────────────

    #[test]
    fn double_remove_second_is_noop(vals in values_strategy(256, 30)) {
        let unique: BTreeSet<u32> = vals.iter().copied().collect();
        let mut veb = VanEmdeBoas::new(256);
        for &v in &unique {
            veb.insert(v);
        }

        for &v in &unique {
            let first = veb.remove(v);
            let second = veb.remove(v);
            prop_assert!(first, "first remove of {} should return true", v);
            prop_assert!(!second, "second remove of {} should return false", v);
        }
    }

    // ── Insert then remove cancels out ───────────────────────────

    #[test]
    fn insert_remove_cancel(vals in values_strategy(256, 20)) {
        let mut veb = VanEmdeBoas::new(256);
        let unique: BTreeSet<u32> = vals.iter().copied().collect();

        for &v in &unique {
            veb.insert(v);
        }
        for &v in &unique {
            veb.remove(v);
        }

        prop_assert!(veb.is_empty(), "veb not empty after inserting and removing all");
        prop_assert_eq!(veb.len(), 0);
    }

    // ── Universe size preserved ──────────────────────────────────

    #[test]
    fn universe_size_preserved(
        universe in prop::sample::select(vec![2u32, 4, 16, 64, 256, 1024, 4096]),
        vals in values_strategy(256, 20)
    ) {
        let u = universe as usize;
        let mut veb = VanEmdeBoas::new(u);
        prop_assert_eq!(veb.universe_size(), u);

        for &v in &vals {
            if v < universe {
                veb.insert(v);
            }
        }
        // universe_size should not change
        prop_assert_eq!(veb.universe_size(), u);
    }

    // ── Singleton: min == max == element ─────────────────────────

    #[test]
    fn singleton_min_max_equal(val in 0u32..256) {
        let mut veb = VanEmdeBoas::new(256);
        veb.insert(val);

        prop_assert_eq!(veb.min(), Some(val));
        prop_assert_eq!(veb.max(), Some(val));
        prop_assert_eq!(veb.len(), 1);
        prop_assert!(veb.successor(val).is_none());
        prop_assert!(veb.predecessor(val).is_none());
    }

    // ── Remove min updates min correctly ─────────────────────────

    #[test]
    fn remove_min_updates_min(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        if reference.len() < 2 {
            return Ok(());
        }

        let mut veb = VanEmdeBoas::new(256);
        let mut ref_copy = reference.clone();
        for &v in &reference {
            veb.insert(v);
        }

        let old_min = *ref_copy.iter().next().unwrap();
        veb.remove(old_min);
        ref_copy.remove(&old_min);

        let new_min = ref_copy.iter().next().copied();
        prop_assert_eq!(veb.min(), new_min, "min not updated after removing old min {}", old_min);
    }

    // ── Remove max updates max correctly ─────────────────────────

    #[test]
    fn remove_max_updates_max(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        if reference.len() < 2 {
            return Ok(());
        }

        let mut veb = VanEmdeBoas::new(256);
        let mut ref_copy = reference.clone();
        for &v in &reference {
            veb.insert(v);
        }

        let old_max = *ref_copy.iter().next_back().unwrap();
        veb.remove(old_max);
        ref_copy.remove(&old_max);

        let new_max = ref_copy.iter().next_back().copied();
        prop_assert_eq!(veb.max(), new_max, "max not updated after removing old max {}", old_max);
    }

    // ── Contains after remove is false ──────────────────────────

    #[test]
    fn contains_after_remove_false(vals in values_strategy(256, 30)) {
        let unique: BTreeSet<u32> = vals.iter().copied().collect();
        let mut veb = VanEmdeBoas::new(256);
        for &v in &unique {
            veb.insert(v);
        }

        for &v in &unique {
            veb.remove(v);
            prop_assert!(!veb.contains(v), "contains({}) true after remove", v);
        }
    }

    // ── Successor of max is None ────────────────────────────────

    #[test]
    fn successor_of_max_is_none(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        if reference.is_empty() {
            return Ok(());
        }

        let mut veb = VanEmdeBoas::new(256);
        for &v in &reference {
            veb.insert(v);
        }

        let max = veb.max().unwrap();
        prop_assert!(veb.successor(max).is_none(), "successor of max {} should be None", max);
    }

    // ── Predecessor of min is None ──────────────────────────────

    #[test]
    fn predecessor_of_min_is_none(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        if reference.is_empty() {
            return Ok(());
        }

        let mut veb = VanEmdeBoas::new(256);
        for &v in &reference {
            veb.insert(v);
        }

        let min = veb.min().unwrap();
        prop_assert!(veb.predecessor(min).is_none(), "predecessor of min {} should be None", min);
    }

    // ── Interleaved insert/remove stays consistent ──────────────

    #[test]
    fn interleaved_insert_remove(
        inserts in values_strategy(128, 30),
        removes in values_strategy(128, 15)
    ) {
        let mut veb = VanEmdeBoas::new(128);
        let mut reference = BTreeSet::new();

        for &v in &inserts {
            veb.insert(v);
            reference.insert(v);
        }
        for &v in &removes {
            veb.remove(v);
            reference.remove(&v);
        }

        prop_assert_eq!(veb.len(), reference.len());
        prop_assert_eq!(veb.min(), reference.iter().next().copied());
        prop_assert_eq!(veb.max(), reference.iter().next_back().copied());
        prop_assert_eq!(veb.iter(), reference.iter().copied().collect::<Vec<_>>());
    }

    // ── Display produces non-empty string for non-empty tree ────

    #[test]
    fn display_format(vals in values_strategy(64, 20)) {
        let mut veb = VanEmdeBoas::new(64);
        for &v in &vals {
            veb.insert(v);
        }

        let displayed = format!("{}", veb);
        prop_assert!(!displayed.is_empty(), "Display should produce non-empty output");
    }

    // ── Clear then reinsert works correctly ─────────────────────

    #[test]
    fn clear_then_reinsert(
        first_vals in values_strategy(256, 20),
        second_vals in values_strategy(256, 20)
    ) {
        let mut veb = VanEmdeBoas::new(256);

        for &v in &first_vals {
            veb.insert(v);
        }
        veb.clear();

        for &v in &second_vals {
            veb.insert(v);
        }

        let reference = build_reference(&second_vals);
        prop_assert_eq!(veb.len(), reference.len());
        prop_assert_eq!(veb.iter(), reference.iter().copied().collect::<Vec<_>>());
    }

    // ── Larger universe successor/predecessor correctness ───────

    #[test]
    fn larger_universe_successor_predecessor(
        vals in values_strategy(4096, 30),
        probe in 0u32..4096
    ) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(4096);
        for &v in &vals {
            veb.insert(v);
        }

        let expected_succ = reference.range((probe + 1)..).next().copied();
        let expected_pred = reference.range(..probe).next_back().copied();

        prop_assert_eq!(veb.successor(probe), expected_succ, "succ({}) in u=4096", probe);
        prop_assert_eq!(veb.predecessor(probe), expected_pred, "pred({}) in u=4096", probe);
    }

    // ── Boundary values: insert 0 and universe-1 ────────────────

    #[test]
    fn boundary_values(
        universe in prop::sample::select(vec![2u32, 4, 16, 64, 256]),
        mid_vals in values_strategy(256, 10)
    ) {
        let u = universe as usize;
        let mut veb = VanEmdeBoas::new(u);

        veb.insert(0);
        veb.insert(universe - 1);

        prop_assert!(veb.contains(0));
        prop_assert!(veb.contains(universe - 1));
        prop_assert_eq!(veb.min(), Some(0));
        prop_assert_eq!(veb.max(), Some(universe - 1));

        // Insert some middle values and verify boundaries still hold
        for &v in &mid_vals {
            if v < universe {
                veb.insert(v);
            }
        }

        prop_assert_eq!(veb.min(), Some(0));
        prop_assert_eq!(veb.max(), Some(universe - 1));
    }

    // ── Serde roundtrip preserves successor/predecessor ─────────

    #[test]
    fn serde_preserves_navigation(
        vals in values_strategy(256, 30),
        probe in 0u32..256
    ) {
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let json = serde_json::to_string(&veb).unwrap();
        let restored: VanEmdeBoas = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.successor(probe), veb.successor(probe));
        prop_assert_eq!(restored.predecessor(probe), veb.predecessor(probe));
        prop_assert_eq!(restored.contains(probe), veb.contains(probe));
    }

    // ── Stress: many inserts then selective removes ─────────────

    #[test]
    fn stress_insert_selective_remove(
        vals in values_strategy(256, 50),
        remove_indices in prop::collection::vec(0usize..50, 0..25)
    ) {
        let mut veb = VanEmdeBoas::new(256);
        let mut reference = BTreeSet::new();

        for &v in &vals {
            veb.insert(v);
            reference.insert(v);
        }

        // Remove elements at selected indices (if they exist in vals)
        for &idx in &remove_indices {
            if idx < vals.len() {
                let v = vals[idx];
                veb.remove(v);
                reference.remove(&v);
            }
        }

        prop_assert_eq!(veb.len(), reference.len());
        for &v in &reference {
            prop_assert!(veb.contains(v), "missing {} after selective removes", v);
        }
    }
}
