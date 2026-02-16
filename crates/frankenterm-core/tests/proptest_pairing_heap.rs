//! Property-based tests for `pairing_heap` module.
//!
//! Verifies correctness invariants:
//! - Pop order matches sorted BinaryHeap reference
//! - Peek returns minimum
//! - Merge preserves all elements
//! - Length tracking
//! - Serde roundtrip

use frankenterm_core::pairing_heap::PairingHeap;
use proptest::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

// ── Strategies ─────────────────────────────────────────────────────────

fn values_strategy(max_len: usize) -> impl Strategy<Value = Vec<i32>> {
    prop::collection::vec(-1000..1000i32, 0..max_len)
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Pop order matches sorted ─────────────────────────────────

    #[test]
    fn pop_order_matches_sorted(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        for &v in &vals {
            heap.insert(v, v);
            reference.push(Reverse(v));
        }

        while let Some((k, _)) = heap.pop() {
            let Reverse(expected) = reference.pop().unwrap();
            prop_assert_eq!(k, expected);
        }
        prop_assert!(reference.is_empty());
    }

    // ── Peek returns minimum ─────────────────────────────────────

    #[test]
    fn peek_returns_minimum(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        if vals.is_empty() {
            prop_assert!(heap.peek().is_none());
        } else {
            let min_val = *vals.iter().min().unwrap();
            let (peek_key, _) = heap.peek().unwrap();
            prop_assert_eq!(*peek_key, min_val);
        }
    }

    // ── Length matches insertion count ────────────────────────────

    #[test]
    fn length_matches(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        prop_assert_eq!(heap.len(), vals.len());
    }

    // ── Pop decrements length ────────────────────────────────────

    #[test]
    fn pop_decrements_length(vals in values_strategy(30)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        for i in 0..vals.len() {
            let expected_len = vals.len() - i;
            prop_assert_eq!(heap.len(), expected_len);
            heap.pop();
        }
        prop_assert!(heap.is_empty());
    }

    // ── Merge preserves all elements ─────────────────────────────

    #[test]
    fn merge_preserves_elements(
        vals1 in values_strategy(25),
        vals2 in values_strategy(25)
    ) {
        let mut h1 = PairingHeap::new();
        let mut h2 = PairingHeap::new();

        for &v in &vals1 {
            h1.insert(v, v);
        }
        for &v in &vals2 {
            h2.insert(v, v);
        }

        h1.merge(&mut h2);
        prop_assert!(h2.is_empty());
        prop_assert_eq!(h1.len(), vals1.len() + vals2.len());

        // All elements should come out in sorted order
        let mut all_vals = vals1.clone();
        all_vals.extend_from_slice(&vals2);
        all_vals.sort();

        let sorted = h1.into_sorted();
        let sorted_keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(sorted_keys, all_vals);
    }

    // ── Merge with empty ─────────────────────────────────────────

    #[test]
    fn merge_with_empty(vals in values_strategy(30)) {
        let mut h1 = PairingHeap::new();
        for &v in &vals {
            h1.insert(v, v);
        }

        let mut h2: PairingHeap<i32, i32> = PairingHeap::new();
        h1.merge(&mut h2);

        prop_assert_eq!(h1.len(), vals.len());
    }

    // ── Sorted output ────────────────────────────────────────────

    #[test]
    fn sorted_output_matches(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let mut expected = vals.clone();
        expected.sort();

        let sorted = heap.sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(keys, expected);
    }

    // ── Sorted doesn't consume ───────────────────────────────────

    #[test]
    fn sorted_doesnt_consume(vals in values_strategy(30)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let _ = heap.sorted();
        prop_assert_eq!(heap.len(), vals.len());
    }

    // ── Serde roundtrip ──────────────────────────────────────────

    #[test]
    fn serde_roundtrip(vals in values_strategy(30)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let json = serde_json::to_string(&heap).unwrap();
        let restored: PairingHeap<i32, i32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), heap.len());

        let original_sorted = heap.sorted();
        let restored_sorted = restored.sorted();
        prop_assert_eq!(original_sorted, restored_sorted);
    }

    // ── Empty operations ─────────────────────────────────────────

    #[test]
    fn empty_operations(val in -1000..1000i32) {
        let mut heap: PairingHeap<i32, i32> = PairingHeap::new();
        prop_assert!(heap.is_empty());
        prop_assert!(heap.peek().is_none());
        prop_assert!(heap.pop().is_none());

        heap.insert(val, val);
        prop_assert_eq!(heap.len(), 1);
        let (k, _) = heap.pop().unwrap();
        prop_assert_eq!(k, val);
        prop_assert!(heap.is_empty());
    }

    // ── Insert then pop identity ─────────────────────────────────

    #[test]
    fn insert_pop_identity(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v * 10);
        }

        let mut expected = vals.clone();
        expected.sort();

        for &e in &expected {
            let (k, v) = heap.pop().unwrap();
            prop_assert_eq!(k, e);
            prop_assert_eq!(v, e * 10);
        }
    }

    // ── Values preserved correctly ───────────────────────────────

    #[test]
    fn values_preserved(vals in values_strategy(30)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v * 100);
        }

        let sorted = heap.into_sorted();
        for (k, v) in &sorted {
            prop_assert_eq!(*v, *k * 100, "value mismatch for key {}", k);
        }
    }

    // ═══════════════════════════════════════════════════════════════
    // NEW TESTS (13 additional property tests)
    // ═══════════════════════════════════════════════════════════════

    // ── Merge commutativity ──────────────────────────────────────

    #[test]
    fn merge_commutativity(
        vals1 in values_strategy(25),
        vals2 in values_strategy(25)
    ) {
        // Build heaps for a→b direction
        let mut h1a = PairingHeap::new();
        let mut h2a = PairingHeap::new();
        for &v in &vals1 { h1a.insert(v, v); }
        for &v in &vals2 { h2a.insert(v, v); }
        h1a.merge(&mut h2a);
        let sorted_ab: Vec<(i32, i32)> = h1a.into_sorted();

        // Build heaps for b→a direction
        let mut h1b = PairingHeap::new();
        let mut h2b = PairingHeap::new();
        for &v in &vals1 { h1b.insert(v, v); }
        for &v in &vals2 { h2b.insert(v, v); }
        h2b.merge(&mut h1b);
        let sorted_ba: Vec<(i32, i32)> = h2b.into_sorted();

        // Sorted key sequences must be identical
        let keys_ab: Vec<i32> = sorted_ab.iter().map(|(k, _)| *k).collect();
        let keys_ba: Vec<i32> = sorted_ba.iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(keys_ab, keys_ba, "merge commutativity violated");
    }

    // ── Merge multiple heaps ─────────────────────────────────────

    #[test]
    fn merge_multiple_heaps(
        vals1 in values_strategy(20),
        vals2 in values_strategy(20),
        vals3 in values_strategy(20)
    ) {
        let mut h1 = PairingHeap::new();
        let mut h2 = PairingHeap::new();
        let mut h3 = PairingHeap::new();

        for &v in &vals1 { h1.insert(v, v); }
        for &v in &vals2 { h2.insert(v, v); }
        for &v in &vals3 { h3.insert(v, v); }

        h1.merge(&mut h2);
        h1.merge(&mut h3);

        prop_assert!(h2.is_empty());
        prop_assert!(h3.is_empty());

        let total = vals1.len() + vals2.len() + vals3.len();
        prop_assert_eq!(h1.len(), total);

        let mut all_vals = vals1.clone();
        all_vals.extend_from_slice(&vals2);
        all_vals.extend_from_slice(&vals3);
        all_vals.sort();

        let sorted_keys: Vec<i32> = h1.into_sorted().iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(sorted_keys, all_vals);
    }

    // ── Clone preserves contents ─────────────────────────────────

    #[test]
    fn clone_preserves_contents(vals in values_strategy(40)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let cloned = heap.clone();

        let original_sorted = heap.sorted();
        let cloned_sorted = cloned.sorted();
        prop_assert_eq!(original_sorted, cloned_sorted, "clone does not match original");
    }

    // ── Duplicate keys all preserved ─────────────────────────────

    #[test]
    fn duplicate_keys_all_preserved(
        key in -500..500i32,
        count in 1usize..50
    ) {
        let mut heap = PairingHeap::new();
        for i in 0..count {
            heap.insert(key, i as i32);
        }

        prop_assert_eq!(heap.len(), count);

        let mut popped = 0usize;
        while let Some((k, _)) = heap.pop() {
            prop_assert_eq!(k, key, "unexpected key after pop");
            popped += 1;
        }
        prop_assert_eq!(popped, count, "did not recover all duplicate-key entries");
    }

    // ── Interleaved insert and pop ───────────────────────────────

    #[test]
    fn interleaved_insert_pop(
        ops in prop::collection::vec(
            prop_oneof![
                (-1000..1000i32).prop_map(|v| (true, v)),
                Just((false, 0i32)),
            ],
            0..80
        )
    ) {
        let mut heap = PairingHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        for (is_insert, val) in &ops {
            if *is_insert {
                heap.insert(*val, *val);
                reference.push(Reverse(*val));
            } else {
                let got = heap.pop();
                let expected = reference.pop().map(|Reverse(v)| v);
                match (got, expected) {
                    (Some((k, _)), Some(e)) => {
                        prop_assert_eq!(k, e, "pop mismatch during interleaved ops");
                    }
                    (None, None) => {} // both empty
                    (g, e) => {
                        let got_str = format!("{:?}", g);
                        let exp_str = format!("{:?}", e);
                        prop_assert!(false, "pop divergence: got={} expected={}", got_str, exp_str);
                    }
                }
            }
        }

        // Drain remaining
        while let Some((k, _)) = heap.pop() {
            let Reverse(expected) = reference.pop().unwrap();
            prop_assert_eq!(k, expected, "drain mismatch after interleaved ops");
        }
        prop_assert!(reference.is_empty(), "reference still has elements");
    }

    // ── Peek tracks minimum after each insert ────────────────────

    #[test]
    fn peek_tracks_minimum(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        let mut running_min = i32::MAX;

        for &v in &vals {
            heap.insert(v, v);
            if v < running_min {
                running_min = v;
            }
            let (pk, _) = heap.peek().unwrap();
            prop_assert_eq!(*pk, running_min, "peek does not track running minimum");
        }
    }

    // ── Peek correct after pop ───────────────────────────────────

    #[test]
    fn peek_correct_after_pop(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let mut sorted_vals = vals.clone();
        sorted_vals.sort();

        for (i, &expected_min) in sorted_vals.iter().enumerate() {
            // Before pop, peek should show current min
            let (pk, _) = heap.peek().unwrap();
            prop_assert_eq!(*pk, expected_min, "peek wrong before pop #{}", i);

            let (popped_k, _) = heap.pop().unwrap();
            prop_assert_eq!(popped_k, expected_min, "pop wrong at index {}", i);

            // After pop, if elements remain, peek should show next min
            if i + 1 < sorted_vals.len() {
                let (next_pk, _) = heap.peek().unwrap();
                prop_assert_eq!(*next_pk, sorted_vals[i + 1], "peek wrong after pop #{}", i);
            } else {
                prop_assert!(heap.peek().is_none(), "heap should be empty after final pop");
            }
        }
    }

    // ── Merge peek is global minimum ─────────────────────────────

    #[test]
    fn merge_peek_global_min(
        vals1 in values_strategy(25),
        vals2 in values_strategy(25)
    ) {
        let mut h1 = PairingHeap::new();
        let mut h2 = PairingHeap::new();

        for &v in &vals1 { h1.insert(v, v); }
        for &v in &vals2 { h2.insert(v, v); }

        h1.merge(&mut h2);

        let mut all_vals = vals1.clone();
        all_vals.extend_from_slice(&vals2);

        if all_vals.is_empty() {
            prop_assert!(h1.peek().is_none());
        } else {
            let global_min = *all_vals.iter().min().unwrap();
            let (pk, _) = h1.peek().unwrap();
            prop_assert_eq!(*pk, global_min, "peek after merge is not global min");
        }
    }

    // ── Large heap ordering (LCG pseudo-random) ──────────────────

    #[test]
    fn large_heap_ordering(seed in 0u64..100_000) {
        let count = 200usize;
        let mut heap = PairingHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        // LCG: x_{n+1} = (a*x_n + c) mod m
        let mut lcg = seed;
        for _ in 0..count {
            lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let val = (lcg >> 33) as i32 % 10_000;
            heap.insert(val, val);
            reference.push(Reverse(val));
        }

        prop_assert_eq!(heap.len(), count);

        while let Some((k, _)) = heap.pop() {
            let Reverse(expected) = reference.pop().unwrap();
            prop_assert_eq!(k, expected, "large heap pop order mismatch");
        }
        prop_assert!(reference.is_empty());
    }

    // ── sorted() preserves heap for subsequent pops ──────────────

    #[test]
    fn sorted_preserves_heap(vals in values_strategy(40)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        // Call sorted() (non-consuming)
        let sorted_snapshot = heap.sorted();

        // Now pop all and verify order matches the sorted snapshot
        let mut pop_results = Vec::new();
        while let Some(item) = heap.pop() {
            pop_results.push(item);
        }

        prop_assert_eq!(
            pop_results.len(), sorted_snapshot.len(),
            "pop count differs from sorted count"
        );

        let sorted_keys: Vec<i32> = sorted_snapshot.iter().map(|(k, _)| *k).collect();
        let pop_keys: Vec<i32> = pop_results.iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(pop_keys, sorted_keys, "pop order differs from sorted() output");
    }

    // ── into_sorted matches sequential pop ───────────────────────

    #[test]
    fn into_sorted_matches_pop(vals in values_strategy(40)) {
        // Build two identical heaps
        let mut heap_pop = PairingHeap::new();
        let mut heap_sorted = PairingHeap::new();
        for &v in &vals {
            heap_pop.insert(v, v);
            heap_sorted.insert(v, v);
        }

        // Get order via sequential pop
        let mut pop_order = Vec::new();
        while let Some(item) = heap_pop.pop() {
            pop_order.push(item);
        }

        // Get order via into_sorted
        let sorted_order = heap_sorted.into_sorted();

        prop_assert_eq!(pop_order, sorted_order, "into_sorted differs from pop sequence");
    }

    // ── Merge with empty is identity ─────────────────────────────

    #[test]
    fn merge_with_empty_identity(vals in values_strategy(40)) {
        let mut heap = PairingHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        // Snapshot before merge
        let before = heap.sorted();

        // Merge with empty heap
        let mut empty: PairingHeap<i32, i32> = PairingHeap::new();
        heap.merge(&mut empty);

        // Snapshot after merge
        let after = heap.sorted();

        prop_assert_eq!(before, after, "merge with empty changed heap contents");
        prop_assert_eq!(heap.len(), vals.len(), "merge with empty changed length");
    }

    // ── Insert returns valid unique indices ───────────────────────

    #[test]
    fn insert_returns_valid_indices(vals in values_strategy(50)) {
        let mut heap = PairingHeap::new();
        let mut indices = Vec::new();

        for &v in &vals {
            let idx = heap.insert(v, v);
            indices.push(idx);
        }

        // All indices within a single insertion batch should be unique
        let mut unique_indices = indices.clone();
        unique_indices.sort();
        unique_indices.dedup();
        prop_assert_eq!(
            unique_indices.len(), indices.len(),
            "insert returned duplicate node indices"
        );

        // Each index should be a valid arena slot (< nodes.len())
        // The heap len tells us we inserted that many; indices should be < total arena capacity
        // Since arena grows, each index < vals.len() + possible free slots
        // At minimum, each index should be < number of nodes ever allocated
        let max_arena = vals.len(); // no pops, so arena = vals.len()
        for &idx in &indices {
            prop_assert!(
                idx < max_arena,
                "index {} out of arena bounds (max {})", idx, max_arena
            );
        }
    }
}
