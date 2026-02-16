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
use std::collections::BinaryHeap;
use std::cmp::Reverse;

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
}
