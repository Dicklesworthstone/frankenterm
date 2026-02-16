//! Property-based tests for `binomial_heap` module.
//!
//! Verifies correctness invariants:
//! - Pop order matches sorted BinaryHeap reference
//! - Peek returns minimum
//! - Merge preserves all elements
//! - Length tracking
//! - Serde roundtrip

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
}
