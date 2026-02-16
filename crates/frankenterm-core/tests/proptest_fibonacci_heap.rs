//! Property-based tests for `fibonacci_heap` module.
//!
//! Verifies correctness invariants:
//! - Pop order matches sorted BinaryHeap reference
//! - Peek returns minimum
//! - Merge preserves all elements
//! - Decrease-key maintains heap property
//! - Length tracking
//! - Serde roundtrip
//! - Clone, duplicate keys, interleaved ops, stress tests

use frankenterm_core::fibonacci_heap::FibonacciHeap;
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

    // ── 1. Pop order matches sorted ───────────────────────────────

    #[test]
    fn pop_order_matches_sorted(vals in values_strategy(50)) {
        let mut heap = FibonacciHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        for &v in &vals {
            heap.insert(v, v);
            reference.push(Reverse(v));
        }

        while let Some((k, _)) = heap.extract_min() {
            let Reverse(expected) = reference.pop().unwrap();
            prop_assert_eq!(k, expected);
        }
        prop_assert!(reference.is_empty());
    }

    // ── 2. Peek returns minimum ───────────────────────────────────

    #[test]
    fn peek_returns_minimum(vals in values_strategy(50)) {
        let mut heap = FibonacciHeap::new();
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

    // ── 3. Length matches insertion count ──────────────────────────

    #[test]
    fn length_matches(vals in values_strategy(50)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        prop_assert_eq!(heap.len(), vals.len());
    }

    // ── 4. Extract min decrements length ──────────────────────────

    #[test]
    fn extract_min_decrements_length(vals in values_strategy(30)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        for i in 0..vals.len() {
            let expected_len = vals.len() - i;
            prop_assert_eq!(heap.len(), expected_len);
            heap.extract_min();
        }
        prop_assert!(heap.is_empty());
    }

    // ── 5. Merge preserves all elements ───────────────────────────

    #[test]
    fn merge_preserves_elements(
        vals1 in values_strategy(25),
        vals2 in values_strategy(25)
    ) {
        let mut h1 = FibonacciHeap::new();
        let mut h2 = FibonacciHeap::new();

        for &v in &vals1 {
            h1.insert(v, v);
        }
        for &v in &vals2 {
            h2.insert(v, v);
        }

        h1.merge(&mut h2);
        prop_assert!(h2.is_empty());
        prop_assert_eq!(h1.len(), vals1.len() + vals2.len());

        let mut all_vals = vals1.clone();
        all_vals.extend_from_slice(&vals2);
        all_vals.sort();

        let sorted = h1.into_sorted();
        let sorted_keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(sorted_keys, all_vals);
    }

    // ── 6. Decrease key maintains order ───────────────────────────

    #[test]
    fn decrease_key_maintains_order(vals in values_strategy(30)) {
        prop_assume!(!vals.is_empty());

        let mut heap = FibonacciHeap::new();
        let handles: Vec<usize> = vals.iter().map(|&v| heap.insert(v, v)).collect();

        // Extract min to force consolidation
        heap.extract_min();

        if handles.len() > 1 {
            // Decrease the last handle's key to be very small
            let last = *handles.last().unwrap();
            // Check if the handle is still valid (not the one we extracted)
            if heap.get_key(last).is_some() {
                let new_key = -2000;
                heap.decrease_key(last, new_key);
                let (k, _) = heap.peek().unwrap();
                prop_assert!(*k <= new_key);
            }
        }
    }

    // ── 7. Sorted output matches ──────────────────────────────────

    #[test]
    fn sorted_output_matches(vals in values_strategy(50)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let mut expected = vals.clone();
        expected.sort();

        let sorted = heap.sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(keys, expected);
    }

    // ── 8. Sorted doesn't consume ─────────────────────────────────

    #[test]
    fn sorted_doesnt_consume(vals in values_strategy(30)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let _ = heap.sorted();
        prop_assert_eq!(heap.len(), vals.len());
    }

    // ── 9. Serde roundtrip ────────────────────────────────────────

    #[test]
    fn serde_roundtrip(vals in values_strategy(30)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let json = serde_json::to_string(&heap).unwrap();
        let restored: FibonacciHeap<i32, i32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), heap.len());

        let original_sorted = heap.sorted();
        let restored_sorted = restored.sorted();
        prop_assert_eq!(original_sorted, restored_sorted);
    }

    // ── 10. Values preserved correctly ────────────────────────────

    #[test]
    fn values_preserved(vals in values_strategy(30)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v * 100);
        }

        let sorted = heap.into_sorted();
        for (k, v) in &sorted {
            prop_assert_eq!(*v, *k * 100, "value mismatch for key {}", k);
        }
    }

    // ── 11. Get key/value consistency ─────────────────────────────

    #[test]
    fn get_key_value_consistent(vals in values_strategy(30)) {
        let mut heap = FibonacciHeap::new();
        let handles: Vec<(usize, i32)> = vals.iter()
            .map(|&v| (heap.insert(v, v * 10), v))
            .collect();

        for &(h, v) in &handles {
            prop_assert_eq!(heap.get_key(h), Some(&v));
            prop_assert_eq!(heap.get_value(h), Some(&(v * 10)));
        }
    }

    // ── 12. Empty operations ──────────────────────────────────────

    #[test]
    fn empty_operations(val in -1000..1000i32) {
        let mut heap: FibonacciHeap<i32, i32> = FibonacciHeap::new();
        prop_assert!(heap.is_empty());
        prop_assert!(heap.peek().is_none());
        prop_assert!(heap.extract_min().is_none());

        heap.insert(val, val);
        prop_assert_eq!(heap.len(), 1);
        let (k, _) = heap.extract_min().unwrap();
        prop_assert_eq!(k, val);
        prop_assert!(heap.is_empty());
    }

    // ── 13. Insert then extract identity ──────────────────────────

    #[test]
    fn insert_extract_identity(vals in values_strategy(50)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v * 10);
        }

        let mut expected = vals.clone();
        expected.sort();

        for &e in &expected {
            let (k, v) = heap.extract_min().unwrap();
            prop_assert_eq!(k, e);
            prop_assert_eq!(v, e * 10);
        }
    }

    // ── 14. Merge commutativity ───────────────────────────────────
    // Merging a->b vs b->a produces the same sorted elements.

    #[test]
    fn merge_commutativity(
        vals1 in values_strategy(25),
        vals2 in values_strategy(25)
    ) {
        // Build first pair: merge h1 <- h2
        let mut h1a = FibonacciHeap::new();
        let mut h2a = FibonacciHeap::new();
        for &v in &vals1 { h1a.insert(v, v); }
        for &v in &vals2 { h2a.insert(v, v); }
        h1a.merge(&mut h2a);
        let sorted_ab: Vec<(i32, i32)> = h1a.into_sorted();

        // Build second pair: merge h2 <- h1
        let mut h1b = FibonacciHeap::new();
        let mut h2b = FibonacciHeap::new();
        for &v in &vals1 { h1b.insert(v, v); }
        for &v in &vals2 { h2b.insert(v, v); }
        h2b.merge(&mut h1b);
        let sorted_ba: Vec<(i32, i32)> = h2b.into_sorted();

        prop_assert_eq!(sorted_ab, sorted_ba);
    }

    // ── 15. Merge multiple heaps ──────────────────────────────────
    // Chaining 3 merges preserves all elements.

    #[test]
    fn merge_multiple_heaps(
        vals1 in values_strategy(20),
        vals2 in values_strategy(20),
        vals3 in values_strategy(20)
    ) {
        let mut h1 = FibonacciHeap::new();
        let mut h2 = FibonacciHeap::new();
        let mut h3 = FibonacciHeap::new();

        for &v in &vals1 { h1.insert(v, v); }
        for &v in &vals2 { h2.insert(v, v); }
        for &v in &vals3 { h3.insert(v, v); }

        h1.merge(&mut h2);
        h1.merge(&mut h3);

        let total = vals1.len() + vals2.len() + vals3.len();
        prop_assert_eq!(h1.len(), total, "len mismatch: expected {}", total);
        prop_assert!(h2.is_empty());
        prop_assert!(h3.is_empty());

        let mut all_vals = vals1.clone();
        all_vals.extend_from_slice(&vals2);
        all_vals.extend_from_slice(&vals3);
        all_vals.sort();

        let sorted_keys: Vec<i32> = h1.into_sorted().iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(sorted_keys, all_vals);
    }

    // ── 16. Clone preserves contents ──────────────────────────────

    #[test]
    fn clone_preserves_contents(vals in values_strategy(40)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let cloned = heap.clone();

        prop_assert_eq!(cloned.len(), heap.len());
        let original_sorted = heap.sorted();
        let cloned_sorted = cloned.sorted();
        prop_assert_eq!(original_sorted, cloned_sorted);
    }

    // ── 17. Duplicate keys preserved ──────────────────────────────
    // N copies of the same key should all be extractable.

    #[test]
    fn duplicate_keys_preserved(
        key in -500..500i32,
        count in 1usize..30
    ) {
        let mut heap = FibonacciHeap::new();
        for i in 0..count {
            heap.insert(key, i as i32);
        }

        prop_assert_eq!(heap.len(), count);

        let mut extracted = 0usize;
        while let Some((k, _)) = heap.extract_min() {
            prop_assert_eq!(k, key, "unexpected key {}", k);
            extracted += 1;
        }
        prop_assert_eq!(extracted, count, "expected {} extractions, got {}", count, extracted);
    }

    // ── 18. Interleaved insert/extract matches reference ──────────
    // Random mix of insert and extract_min operations, cross-checked
    // against a BinaryHeap<Reverse<_>> reference.

    #[test]
    fn interleaved_insert_extract(
        ops in prop::collection::vec(
            prop_oneof![
                (0..1000i32).prop_map(|v| (true, v)),
                Just((false, 0i32)),
            ],
            0..80
        )
    ) {
        let mut heap = FibonacciHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        for (is_insert, val) in &ops {
            if *is_insert {
                heap.insert(*val, *val);
                reference.push(Reverse(*val));
            } else {
                let got = heap.extract_min();
                let expected = reference.pop().map(|Reverse(v)| v);
                match (got, expected) {
                    (Some((k, _)), Some(e)) => {
                        prop_assert_eq!(k, e, "extract mismatch: got {}, expected {}", k, e);
                    }
                    (None, None) => {}
                    (g, e) => {
                        prop_assert!(false, "mismatch: got {:?}, expected {:?}", g, e);
                    }
                }
            }
        }

        // Drain remaining
        while let Some((k, _)) = heap.extract_min() {
            let Reverse(expected) = reference.pop().unwrap();
            prop_assert_eq!(k, expected, "drain mismatch: got {}, expected {}", k, expected);
        }
        prop_assert!(reference.is_empty());
    }

    // ── 19. Peek tracks minimum after each insert ─────────────────

    #[test]
    fn peek_tracks_minimum(vals in values_strategy(50)) {
        let mut heap = FibonacciHeap::new();
        let mut running_min = i32::MAX;

        for &v in &vals {
            heap.insert(v, v);
            if v < running_min {
                running_min = v;
            }
            let (peek_key, _) = heap.peek().unwrap();
            prop_assert_eq!(*peek_key, running_min,
                "after inserting {}, peek should be {} but got {}",
                v, running_min, *peek_key);
        }
    }

    // ── 20. Peek correct after each extract ───────────────────────

    #[test]
    fn peek_correct_after_extract(vals in values_strategy(40)) {
        prop_assume!(vals.len() >= 2);

        let mut heap = FibonacciHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        for &v in &vals {
            heap.insert(v, v);
            reference.push(Reverse(v));
        }

        // Extract all but the last, checking peek after each
        for _ in 0..(vals.len() - 1) {
            heap.extract_min();
            reference.pop();

            let heap_peek = heap.peek().map(|(k, _)| *k);
            let ref_peek = reference.peek().map(|Reverse(v)| *v);
            prop_assert_eq!(heap_peek, ref_peek,
                "peek mismatch: heap says {:?}, reference says {:?}",
                heap_peek, ref_peek);
        }
    }

    // ── 21. Merge peek is global minimum ──────────────────────────

    #[test]
    fn merge_peek_global_min(
        vals1 in values_strategy(25),
        vals2 in values_strategy(25)
    ) {
        prop_assume!(!vals1.is_empty() || !vals2.is_empty());

        let mut h1 = FibonacciHeap::new();
        let mut h2 = FibonacciHeap::new();
        for &v in &vals1 { h1.insert(v, v); }
        for &v in &vals2 { h2.insert(v, v); }

        h1.merge(&mut h2);

        let mut all_vals = vals1.clone();
        all_vals.extend_from_slice(&vals2);
        let global_min = *all_vals.iter().min().unwrap();

        let (peek_key, _) = h1.peek().unwrap();
        prop_assert_eq!(*peek_key, global_min,
            "after merge, peek should be {} but got {}",
            global_min, *peek_key);
    }

    // ── 22. Large heap stress (LCG-based) ─────────────────────────
    // Deterministic pseudo-random sequence via linear congruential generator.

    #[test]
    fn large_heap_stress(seed in 1u64..100000) {
        let mut heap = FibonacciHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        // LCG: x_{n+1} = (a * x_n + c) mod m
        let mut state = seed;
        let count = 200;

        for _ in 0..count {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let val = ((state >> 33) as i32) % 10000;
            heap.insert(val, val);
            reference.push(Reverse(val));
        }

        prop_assert_eq!(heap.len(), count, "heap len should be {}", count);

        while let Some((k, _)) = heap.extract_min() {
            let Reverse(expected) = reference.pop().unwrap();
            prop_assert_eq!(k, expected, "stress: got {}, expected {}", k, expected);
        }
        prop_assert!(reference.is_empty());
    }

    // ── 23. sorted() preserves heap for subsequent extract_min ────
    // Calling sorted() (non-consuming) should not affect the heap state.

    #[test]
    fn sorted_preserves_heap(vals in values_strategy(40)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let sorted_snapshot = heap.sorted();
        prop_assert_eq!(heap.len(), vals.len(),
            "sorted() changed heap len from {} to {}", vals.len(), heap.len());

        // Now extract all and verify they match the sorted snapshot
        let mut extracted = Vec::new();
        while let Some(pair) = heap.extract_min() {
            extracted.push(pair);
        }
        prop_assert_eq!(extracted, sorted_snapshot,
            "extract_min sequence after sorted() differs from sorted() result");
    }

    // ── 24. into_sorted matches sequential extract_min ────────────

    #[test]
    fn into_sorted_matches_extract(vals in values_strategy(40)) {
        // Build two identical heaps
        let mut heap1 = FibonacciHeap::new();
        let mut heap2 = FibonacciHeap::new();
        for &v in &vals {
            heap1.insert(v, v);
            heap2.insert(v, v);
        }

        // into_sorted on one
        let from_into_sorted = heap1.into_sorted();

        // sequential extract_min on the other
        let mut from_extract = Vec::new();
        while let Some(pair) = heap2.extract_min() {
            from_extract.push(pair);
        }

        prop_assert_eq!(from_into_sorted, from_extract,
            "into_sorted and extract_min sequence differ");
    }

    // ── 25. decrease_key maintains global ordering ────────────────
    // Decrease several keys and verify the final extract order is correct.

    #[test]
    fn decrease_key_maintains_global_order(vals in values_strategy(30)) {
        prop_assume!(vals.len() >= 4);

        let mut heap = FibonacciHeap::new();
        let handles: Vec<(usize, i32)> = vals.iter()
            .map(|&v| (heap.insert(v, v), v))
            .collect();

        // Force consolidation
        heap.extract_min();

        // Decrease every other remaining valid handle to a very negative value
        // The first extract_min removed the minimum
        let mut remaining: Vec<i32> = vals.clone();
        remaining.sort();
        remaining.remove(0); // removed by extract_min

        let mut decreased_indices = Vec::new();
        for (idx, &(h, _original_key)) in handles.iter().enumerate() {
            if heap.get_key(h).is_some() && idx % 2 == 0 {
                decreased_indices.push(idx);
            }
        }

        // Build the reference: start with remaining keys, then apply decreases
        let mut reference_keys: Vec<i32> = Vec::new();
        for (idx, &(h, original_key)) in handles.iter().enumerate() {
            if heap.get_key(h).is_none() {
                continue; // already extracted
            }
            if decreased_indices.contains(&idx) {
                let new_key = -5000 - (idx as i32);
                heap.decrease_key(h, new_key);
                reference_keys.push(new_key);
            } else {
                reference_keys.push(original_key);
            }
        }

        reference_keys.sort();

        let mut extracted_keys = Vec::new();
        while let Some((k, _)) = heap.extract_min() {
            extracted_keys.push(k);
        }

        prop_assert_eq!(extracted_keys, reference_keys,
            "after decrease_key, extraction order is wrong");
    }

    // ── 26. is_empty agrees with len throughout lifecycle ────────
    // At every step of insert/extract, is_empty() == (len() == 0).

    #[test]
    fn is_empty_agrees_with_len(vals in values_strategy(40)) {
        let mut heap = FibonacciHeap::new();

        // Initially empty
        let len_zero = heap.len() == 0;
        let empty = heap.is_empty();
        prop_assert_eq!(empty, len_zero, "initial: is_empty={}, len==0={}", empty, len_zero);

        // After each insert
        for &v in &vals {
            heap.insert(v, v);
            let len_zero = heap.len() == 0;
            let empty = heap.is_empty();
            prop_assert_eq!(empty, len_zero,
                "after insert: is_empty={}, len==0={}, len={}", empty, len_zero, heap.len());
        }

        // After each extract
        while heap.extract_min().is_some() {
            let len_zero = heap.len() == 0;
            let empty = heap.is_empty();
            prop_assert_eq!(empty, len_zero,
                "after extract: is_empty={}, len==0={}, len={}", empty, len_zero, heap.len());
        }

        // Final state
        prop_assert!(heap.is_empty());
        prop_assert_eq!(heap.len(), 0);
    }

    // ── 27. Default produces same state as new ──────────────────
    // FibonacciHeap::default() should behave identically to FibonacciHeap::new().

    #[test]
    fn default_equivalent_to_new(vals in values_strategy(30)) {
        let mut heap_new: FibonacciHeap<i32, i32> = FibonacciHeap::new();
        let mut heap_default: FibonacciHeap<i32, i32> = FibonacciHeap::default();

        // Both start empty
        prop_assert_eq!(heap_new.len(), heap_default.len());
        prop_assert_eq!(heap_new.is_empty(), heap_default.is_empty());

        // Insert same values into both
        for &v in &vals {
            heap_new.insert(v, v);
            heap_default.insert(v, v);
        }

        // Same sorted output
        let sorted_new = heap_new.into_sorted();
        let sorted_default = heap_default.into_sorted();
        prop_assert_eq!(sorted_new, sorted_default);
    }

    // ── 28. Clone independence — mutations don't cross ──────────
    // Modifying a clone does not affect the original and vice versa.

    #[test]
    fn clone_independence(vals in values_strategy(30)) {
        prop_assume!(vals.len() >= 2);

        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let mut cloned = heap.clone();

        // Extract from original
        let original_min = heap.extract_min();
        prop_assert!(original_min.is_some());

        // Clone should still have all elements
        let cloned_len = cloned.len();
        let original_len = heap.len();
        prop_assert_eq!(cloned_len, vals.len(),
            "clone len should be {} but is {}", vals.len(), cloned_len);
        prop_assert_eq!(original_len, vals.len() - 1,
            "original len should be {} but is {}", vals.len() - 1, original_len);

        // Insert into clone
        cloned.insert(-9999, -9999);
        let cloned_len_after = cloned.len();
        let original_len_after = heap.len();
        prop_assert_eq!(cloned_len_after, vals.len() + 1);
        prop_assert_eq!(original_len_after, vals.len() - 1,
            "original affected by clone insert: len={}", original_len_after);

        // Clone's min should now be -9999
        let (peek_key, _) = cloned.peek().unwrap();
        prop_assert_eq!(*peek_key, -9999);
    }

    // ── 29. Display format tracks count correctly ───────────────
    // Display output should always reflect current element count.

    #[test]
    fn display_tracks_count(vals in values_strategy(30)) {
        let mut heap = FibonacciHeap::new();

        // Empty
        let display_str = format!("{}", heap);
        let expected = format!("FibonacciHeap({} elements)", 0);
        prop_assert_eq!(display_str, expected);

        // After insertions
        for &v in &vals {
            heap.insert(v, v);
        }
        let display_str = format!("{}", heap);
        let expected = format!("FibonacciHeap({} elements)", vals.len());
        prop_assert_eq!(display_str, expected);

        // After some extractions
        let extract_count = vals.len() / 2;
        for _ in 0..extract_count {
            heap.extract_min();
        }
        let remaining = vals.len() - extract_count;
        let display_str = format!("{}", heap);
        let expected = format!("FibonacciHeap({} elements)", remaining);
        prop_assert_eq!(display_str, expected);
    }

    // ── 30. Serde roundtrip preserves state after decrease_key ──
    // Serialize/deserialize after decrease_key operations still yields
    // correct extraction order.

    #[test]
    fn serde_roundtrip_after_decrease_key(vals in values_strategy(20)) {
        prop_assume!(vals.len() >= 3);

        let mut heap = FibonacciHeap::new();
        let handles: Vec<usize> = vals.iter().map(|&v| heap.insert(v, v)).collect();

        // Force consolidation so decrease_key triggers cuts
        heap.extract_min();

        // Decrease the last valid handle to a very small key
        let last_handle = *handles.last().unwrap();
        if heap.get_key(last_handle).is_some() {
            heap.decrease_key(last_handle, -5000);
        }

        // Serialize and restore
        let json = serde_json::to_string(&heap).unwrap();
        let restored: FibonacciHeap<i32, i32> = serde_json::from_str(&json).unwrap();

        let heap_len = heap.len();
        let restored_len = restored.len();
        prop_assert_eq!(restored_len, heap_len,
            "restored len {} != original len {}", restored_len, heap_len);

        // Both should produce the same sorted order
        let original_sorted = heap.into_sorted();
        let restored_sorted = restored.into_sorted();
        prop_assert_eq!(original_sorted, restored_sorted,
            "sorted mismatch after serde roundtrip with decrease_key");
    }

    // ── 31. Pop alias matches extract_min exactly ───────────────
    // pop() and extract_min() should behave identically on equal heaps.

    #[test]
    fn pop_matches_extract_min(vals in values_strategy(40)) {
        let mut heap_pop = FibonacciHeap::new();
        let mut heap_extract = FibonacciHeap::new();

        for &v in &vals {
            heap_pop.insert(v, v);
            heap_extract.insert(v, v);
        }

        loop {
            let from_pop = heap_pop.pop();
            let from_extract = heap_extract.extract_min();
            prop_assert_eq!(from_pop, from_extract,
                "pop and extract_min diverged");
            if from_pop.is_none() {
                break;
            }
        }
    }

    // ── 32. Debug format is non-empty and doesn't panic ─────────
    // The Debug impl should produce a non-empty string for any heap state.

    #[test]
    fn debug_format_valid(vals in values_strategy(30)) {
        let mut heap = FibonacciHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let debug_str = format!("{:?}", heap);
        prop_assert!(!debug_str.is_empty(), "Debug output should not be empty");

        // Should contain "FibonacciHeap" since it derives Debug on the struct
        let contains_name = debug_str.contains("FibonacciHeap");
        prop_assert!(contains_name,
            "Debug output should contain 'FibonacciHeap', got: {}", debug_str);

        // After extraction, Debug should still work
        while heap.extract_min().is_some() {}
        let debug_empty = format!("{:?}", heap);
        prop_assert!(!debug_empty.is_empty(), "Debug on empty heap should not be empty");
    }
}
