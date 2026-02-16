//! Property-based tests for `binomial_heap` module.
//!
//! Verifies correctness invariants:
//! - Pop order matches sorted BinaryHeap reference
//! - Peek always returns minimum key
//! - Merge preserves all elements
//! - Merge commutativity (same elements regardless of direction)
//! - Length tracking across insert/extract/merge
//! - No element loss (every insert is extractable)
//! - Serde roundtrip preserves elements
//! - Sorted output is monotonically non-decreasing
//! - Clone preserves heap contents
//! - Duplicate key handling
//! - Interleaved insert/extract sequences
//! - Multi-heap merge chains

use frankenterm_core::binomial_heap::BinomialHeap;
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
        let mut heap = BinomialHeap::new();
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

    // ── Peek returns minimum ─────────────────────────────────────

    #[test]
    fn peek_returns_minimum(vals in values_strategy(50)) {
        let mut heap = BinomialHeap::new();
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

    // ── Length matches ────────────────────────────────────────────

    #[test]
    fn length_matches(vals in values_strategy(50)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        prop_assert_eq!(heap.len(), vals.len());
    }

    // ── Extract min decrements length ────────────────────────────

    #[test]
    fn extract_min_decrements(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        for i in 0..vals.len() {
            prop_assert_eq!(heap.len(), vals.len() - i);
            heap.extract_min();
        }
        prop_assert!(heap.is_empty());
    }

    // ── Merge preserves all elements ─────────────────────────────

    #[test]
    fn merge_preserves(
        vals1 in values_strategy(25),
        vals2 in values_strategy(25)
    ) {
        let mut h1 = BinomialHeap::new();
        let mut h2 = BinomialHeap::new();

        for &v in &vals1 { h1.insert(v, v); }
        for &v in &vals2 { h2.insert(v, v); }

        h1.merge(&mut h2);
        prop_assert!(h2.is_empty());
        prop_assert_eq!(h1.len(), vals1.len() + vals2.len());

        let mut all = vals1.clone();
        all.extend_from_slice(&vals2);
        all.sort();

        let sorted = h1.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        prop_assert_eq!(keys, all);
    }

    // ── Sorted output ────────────────────────────────────────────

    #[test]
    fn sorted_output(vals in values_strategy(50)) {
        let mut heap = BinomialHeap::new();
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
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        let _ = heap.sorted();
        prop_assert_eq!(heap.len(), vals.len());
    }

    // ── Values preserved ─────────────────────────────────────────

    #[test]
    fn values_preserved(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v * 100);
        }
        let sorted = heap.into_sorted();
        for (k, v) in &sorted {
            prop_assert_eq!(*v, *k * 100, "value mismatch for key {}", k);
        }
    }

    // ── Serde roundtrip ──────────────────────────────────────────

    #[test]
    fn serde_roundtrip(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let json = serde_json::to_string(&heap).unwrap();
        let restored: BinomialHeap<i32, i32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), heap.len());
        let orig = heap.sorted();
        let rest = restored.sorted();
        prop_assert_eq!(orig, rest);
    }

    // ── Empty operations ─────────────────────────────────────────

    #[test]
    fn empty_operations(val in -1000..1000i32) {
        let mut heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        prop_assert!(heap.is_empty());
        prop_assert!(heap.peek().is_none());
        prop_assert!(heap.extract_min().is_none());

        heap.insert(val, val);
        prop_assert_eq!(heap.len(), 1);
        let (k, _) = heap.extract_min().unwrap();
        prop_assert_eq!(k, val);
        prop_assert!(heap.is_empty());
    }

    // ── Insert extract identity ──────────────────────────────────

    #[test]
    fn insert_extract_identity(vals in values_strategy(50)) {
        let mut heap = BinomialHeap::new();
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

    // ── Merge commutativity ─────────────────────────────────────

    #[test]
    fn merge_produces_same_elements(
        vals_a in values_strategy(25),
        vals_b in values_strategy(25)
    ) {
        let mut h1a = BinomialHeap::new();
        let mut h1b = BinomialHeap::new();
        let mut h2a = BinomialHeap::new();
        let mut h2b = BinomialHeap::new();

        for &v in &vals_a {
            h1a.insert(v, v);
            h2a.insert(v, v);
        }
        for &v in &vals_b {
            h1b.insert(v, v);
            h2b.insert(v, v);
        }

        h1a.merge(&mut h1b);
        h2b.merge(&mut h2a);

        let sorted1 = h1a.into_sorted();
        let sorted2 = h2b.into_sorted();
        let keys1: Vec<i32> = sorted1.iter().map(|&(k, _)| k).collect();
        let keys2: Vec<i32> = sorted2.iter().map(|&(k, _)| k).collect();
        prop_assert_eq!(keys1, keys2);
    }

    // ── Merge with empty is identity ────────────────────────────

    #[test]
    fn merge_with_empty_is_identity(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        let original_sorted = heap.sorted();

        let mut empty: BinomialHeap<i32, i32> = BinomialHeap::new();
        heap.merge(&mut empty);

        prop_assert_eq!(heap.len(), vals.len());
        let after_sorted = heap.sorted();
        prop_assert_eq!(original_sorted, after_sorted);
    }

    // ── Merge multiple heaps ────────────────────────────────────

    #[test]
    fn merge_multiple_heaps(
        vals_a in values_strategy(15),
        vals_b in values_strategy(15),
        vals_c in values_strategy(15)
    ) {
        let mut h1 = BinomialHeap::new();
        let mut h2 = BinomialHeap::new();
        let mut h3 = BinomialHeap::new();

        for &v in &vals_a { h1.insert(v, v); }
        for &v in &vals_b { h2.insert(v, v); }
        for &v in &vals_c { h3.insert(v, v); }

        h1.merge(&mut h2);
        h1.merge(&mut h3);

        let total = vals_a.len() + vals_b.len() + vals_c.len();
        prop_assert_eq!(h1.len(), total);

        let mut all: Vec<i32> = vals_a
            .iter()
            .chain(vals_b.iter())
            .chain(vals_c.iter())
            .copied()
            .collect();
        all.sort();

        let extracted: Vec<i32> = h1.into_sorted().into_iter().map(|(k, _)| k).collect();
        prop_assert_eq!(extracted, all);
    }

    // ── Clone preserves contents ────────────────────────────────

    #[test]
    fn clone_preserves_contents(vals in values_strategy(40)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }

        let cloned = heap.clone();
        let original_sorted = heap.into_sorted();
        let cloned_sorted = cloned.into_sorted();
        prop_assert_eq!(original_sorted, cloned_sorted);
    }

    // ── Duplicate keys all preserved ────────────────────────────

    #[test]
    fn duplicate_keys_all_preserved(
        key in -100..100i32,
        count in 1..20usize
    ) {
        let mut heap = BinomialHeap::new();
        for i in 0..count {
            heap.insert(key, i as i32);
        }

        prop_assert_eq!(heap.len(), count);

        let mut extracted = 0usize;
        while let Some((k, _)) = heap.extract_min() {
            prop_assert_eq!(k, key);
            extracted += 1;
        }
        prop_assert_eq!(extracted, count);
    }

    // ── Interleaved insert/extract ──────────────────────────────

    #[test]
    fn interleaved_insert_extract(
        ops in prop::collection::vec(
            prop_oneof![
                (-1000..1000i32).prop_map(|v| (true, v)),
                Just((false, 0i32)),
            ],
            0..80
        )
    ) {
        let mut heap = BinomialHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        for (is_insert, val) in ops {
            if is_insert {
                heap.insert(val, val);
                reference.push(Reverse(val));
            } else if !heap.is_empty() {
                let (hk, _) = heap.extract_min().unwrap();
                let Reverse(rk) = reference.pop().unwrap();
                prop_assert_eq!(hk, rk);
            }
        }

        while let Some((hk, _)) = heap.extract_min() {
            let Reverse(rk) = reference.pop().unwrap();
            prop_assert_eq!(hk, rk);
        }
        prop_assert!(reference.is_empty());
    }

    // ── Pop alias consistency ───────────────────────────────────

    #[test]
    fn pop_alias_same_as_extract_min(vals in values_strategy(30)) {
        let mut heap1 = BinomialHeap::new();
        let mut heap2 = BinomialHeap::new();
        for &v in &vals {
            heap1.insert(v, v);
            heap2.insert(v, v);
        }

        for _ in 0..vals.len() {
            let a = heap1.extract_min();
            let b = heap2.pop();
            prop_assert_eq!(a, b);
        }
    }

    // ── Peek correct after each extract ─────────────────────────

    #[test]
    fn peek_correct_after_each_extract(vals in values_strategy(40)) {
        let mut heap = BinomialHeap::new();
        let mut sorted_vals: Vec<i32> = vals.clone();
        sorted_vals.sort();

        for &v in &vals {
            heap.insert(v, v);
        }

        for &expected_min in &sorted_vals {
            if let Some((pk, _)) = heap.peek() {
                prop_assert_eq!(*pk, expected_min);
            }
            let (ek, _) = heap.extract_min().unwrap();
            prop_assert_eq!(ek, expected_min);
        }
    }

    // ── Merge peek returns global minimum ───────────────────────

    #[test]
    fn merge_peek_returns_global_min(
        vals_a in values_strategy(20),
        vals_b in values_strategy(20)
    ) {
        if vals_a.is_empty() && vals_b.is_empty() {
            return Ok(());
        }

        let mut h1 = BinomialHeap::new();
        let mut h2 = BinomialHeap::new();
        for &v in &vals_a { h1.insert(v, v); }
        for &v in &vals_b { h2.insert(v, v); }

        h1.merge(&mut h2);

        if let Some((pk, _)) = h1.peek() {
            let global_min = vals_a
                .iter()
                .chain(vals_b.iter())
                .copied()
                .min()
                .unwrap();
            prop_assert_eq!(*pk, global_min);
        }
    }

    // ── Large heap stress ───────────────────────────────────────

    #[test]
    fn large_heap_ordering(seed in 0..100u64) {
        let mut heap = BinomialHeap::new();
        let mut reference: BinaryHeap<Reverse<i32>> = BinaryHeap::new();

        let mut state = seed;
        for _ in 0..200 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let val = (state >> 33) as i32 % 10000;
            heap.insert(val, val);
            reference.push(Reverse(val));
        }

        while let Some((hk, _)) = heap.extract_min() {
            let Reverse(rk) = reference.pop().unwrap();
            prop_assert_eq!(hk, rk);
        }
    }

    // ── Peek tracks minimum during inserts ──────────────────────

    #[test]
    fn peek_tracks_minimum_during_inserts(vals in values_strategy(50)) {
        let mut heap = BinomialHeap::new();
        let mut min_so_far: Option<i32> = None;

        for &v in &vals {
            heap.insert(v, v);
            min_so_far = Some(min_so_far.map_or(v, |m| m.min(v)));
            let (pk, _) = heap.peek().unwrap();
            prop_assert_eq!(*pk, min_so_far.unwrap());
        }
    }

    // ── is_empty agrees with len ────────────────────────────────

    #[test]
    fn is_empty_agrees_with_len(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        prop_assert_eq!(heap.is_empty(), heap.len() == 0);
    }

    // ── Default is empty ────────────────────────────────────────

    #[test]
    fn default_is_empty(_dummy in 0..10u8) {
        let heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        prop_assert!(heap.is_empty());
        prop_assert_eq!(heap.len(), 0);
        prop_assert!(heap.peek().is_none());
    }

    // ── Clone independence ──────────────────────────────────────

    #[test]
    fn clone_independence(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        let original_len = heap.len();
        let mut cloned = heap.clone();
        cloned.insert(99999, 99999);
        prop_assert_eq!(heap.len(), original_len);
    }

    // ── Serde roundtrip after extractions ────────────────────────

    #[test]
    fn serde_after_extractions(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        // Extract half
        let extract_count = heap.len() / 2;
        for _ in 0..extract_count {
            heap.extract_min();
        }

        let json = serde_json::to_string(&heap).unwrap();
        let restored: BinomialHeap<i32, i32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), heap.len());
        let orig = heap.sorted();
        let rest = restored.sorted();
        prop_assert_eq!(orig, rest);
    }

    // ── into_sorted consumes all ────────────────────────────────

    #[test]
    fn into_sorted_length(vals in values_strategy(40)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        let sorted = heap.into_sorted();
        prop_assert_eq!(sorted.len(), vals.len());
    }

    // ── sorted is non-decreasing ────────────────────────────────

    #[test]
    fn sorted_is_non_decreasing(vals in values_strategy(50)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        let sorted = heap.sorted();
        for w in sorted.windows(2) {
            prop_assert!(w[0].0 <= w[1].0, "not sorted: {} > {}", w[0].0, w[1].0);
        }
    }

    // ── Extract min returns minimum each time ───────────────────

    #[test]
    fn extract_min_monotone(vals in values_strategy(40)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        let mut prev: Option<i32> = None;
        while let Some((k, _)) = heap.extract_min() {
            if let Some(p) = prev {
                prop_assert!(k >= p, "extract not monotone: {} < {}", k, p);
            }
            prev = Some(k);
        }
    }

    // ── Merge self with empty preserves ─────────────────────────

    #[test]
    fn merge_empty_into_full(vals in values_strategy(30)) {
        let mut heap = BinomialHeap::new();
        for &v in &vals {
            heap.insert(v, v);
        }
        let sorted_before = heap.sorted();
        let mut empty: BinomialHeap<i32, i32> = BinomialHeap::new();
        empty.merge(&mut heap);
        let sorted_after = empty.sorted();
        prop_assert_eq!(sorted_before, sorted_after);
    }
}
