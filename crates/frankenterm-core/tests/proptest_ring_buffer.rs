//! Property-based tests for ring_buffer module.
//!
//! Verifies the fixed-capacity circular buffer invariants:
//! - Capacity bound: len() <= capacity() at all times
//! - FIFO order: iteration yields oldest to newest
//! - Overwrite semantics: push returns evicted item when full
//! - front/back: front = oldest, back = newest
//! - Logical indexing: get(0) = oldest, get(len-1) = newest
//! - Total tracking: total_pushed and total_evicted consistency
//! - Clear resets all state
//! - Drain: extracts all items oldest→newest and empties buffer
//! - Stats serde roundtrip

use proptest::prelude::*;
use std::collections::VecDeque;

use frankenterm_core::ring_buffer::{RingBuffer, RingBufferStats};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_capacity() -> impl Strategy<Value = usize> {
    1usize..=20
}

fn arb_items(max_len: usize) -> impl Strategy<Value = Vec<i32>> {
    prop::collection::vec(any::<i32>(), 1..max_len)
}

// ────────────────────────────────────────────────────────────────────
// Reference model: VecDeque with bounded capacity
// ────────────────────────────────────────────────────────────────────

/// A reference model for the ring buffer using VecDeque.
struct RefModel {
    capacity: usize,
    buf: VecDeque<i32>,
    total: u64,
}

impl RefModel {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            buf: VecDeque::with_capacity(capacity),
            total: 0,
        }
    }

    fn push(&mut self, item: i32) -> Option<i32> {
        self.total += 1;
        if self.buf.len() == self.capacity {
            let evicted = self.buf.pop_front();
            self.buf.push_back(item);
            evicted
        } else {
            self.buf.push_back(item);
            None
        }
    }

    fn front(&self) -> Option<&i32> {
        self.buf.front()
    }

    fn back(&self) -> Option<&i32> {
        self.buf.back()
    }

    fn get(&self, index: usize) -> Option<&i32> {
        self.buf.get(index)
    }

    fn iter(&self) -> impl Iterator<Item = &i32> {
        self.buf.iter()
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

// ────────────────────────────────────────────────────────────────────
// Reference model checking
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Ring buffer matches reference VecDeque model through any push sequence.
    #[test]
    fn prop_matches_reference_model(
        capacity in arb_capacity(),
        items in arb_items(80),
    ) {
        let mut rb = RingBuffer::new(capacity);
        let mut model = RefModel::new(capacity);

        for &item in &items {
            let rb_evicted = rb.push(item);
            let model_evicted = model.push(item);
            prop_assert_eq!(
                rb_evicted, model_evicted,
                "Eviction mismatch on push({})", item
            );
        }

        // Check all accessors match
        prop_assert_eq!(rb.len(), model.len());
        prop_assert_eq!(rb.front(), model.front());
        prop_assert_eq!(rb.back(), model.back());

        // Check iteration order
        let rb_items: Vec<&i32> = rb.iter().collect();
        let model_items: Vec<&i32> = model.iter().collect();
        prop_assert_eq!(rb_items, model_items, "Iteration order mismatch");

        // Check logical indexing
        for i in 0..rb.len() {
            prop_assert_eq!(rb.get(i), model.get(i), "get({}) mismatch", i);
        }
        prop_assert_eq!(rb.get(rb.len()), None);
    }
}

// ────────────────────────────────────────────────────────────────────
// Capacity bound
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// len() never exceeds capacity(), regardless of how many items are pushed.
    #[test]
    fn prop_len_bounded_by_capacity(
        capacity in arb_capacity(),
        items in arb_items(100),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
            prop_assert!(
                rb.len() <= capacity,
                "len {} > capacity {}", rb.len(), capacity
            );
        }
    }

    /// is_full() iff len() == capacity().
    #[test]
    fn prop_is_full_correct(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
            prop_assert_eq!(
                rb.is_full(), rb.len() == rb.capacity(),
                "is_full() inconsistent"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Overwrite semantics
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// push returns None when not full, Some(oldest) when full.
    #[test]
    fn prop_push_eviction_semantics(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            let was_full = rb.is_full();
            let oldest_before = rb.front().copied();
            let evicted = rb.push(item);

            if was_full {
                prop_assert_eq!(
                    evicted, oldest_before,
                    "Full push should evict oldest"
                );
            } else {
                prop_assert_eq!(evicted, None, "Non-full push should not evict");
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// FIFO order: iteration oldest→newest
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After pushing N items into capacity-C buffer, iter yields the last min(N,C) items in order.
    #[test]
    fn prop_iter_yields_last_n(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let expected_start = if items.len() > capacity {
            items.len() - capacity
        } else {
            0
        };
        let expected: Vec<&i32> = items[expected_start..].iter().collect();
        let actual: Vec<&i32> = rb.iter().collect();

        prop_assert_eq!(actual, expected, "Iteration order mismatch");
    }

    /// front() == iter().next() and back() == iter().last().
    #[test]
    fn prop_front_back_match_iter(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        if !rb.is_empty() {
            let iter_first = rb.iter().next();
            let iter_last = rb.iter().last();
            prop_assert_eq!(rb.front(), iter_first, "front != iter.first");
            prop_assert_eq!(rb.back(), iter_last, "back != iter.last");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Logical indexing
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// get(i) matches the i-th item from iter().
    #[test]
    fn prop_get_matches_iter(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let iter_items: Vec<&i32> = rb.iter().collect();
        for (i, &expected) in iter_items.iter().enumerate() {
            prop_assert_eq!(
                rb.get(i), Some(expected),
                "get({}) != iter[{}]", i, i
            );
        }
    }

    /// get(i) returns None for i >= len().
    #[test]
    fn prop_get_out_of_bounds_none(
        capacity in arb_capacity(),
        items in arb_items(20),
        extra in 0usize..10,
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        prop_assert_eq!(rb.get(rb.len() + extra), None);
    }
}

// ────────────────────────────────────────────────────────────────────
// Total tracking
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// total_pushed == number of push() calls.
    #[test]
    fn prop_total_pushed_accurate(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }
        prop_assert_eq!(rb.total_pushed(), items.len() as u64);
    }

    /// total_evicted == max(0, total_pushed - capacity).
    #[test]
    fn prop_total_evicted_correct(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let expected = if items.len() > capacity {
            (items.len() - capacity) as u64
        } else {
            0
        };
        prop_assert_eq!(rb.total_evicted(), expected);
    }

    /// total_pushed = len + total_evicted.
    #[test]
    fn prop_total_conservation(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        prop_assert_eq!(
            rb.total_pushed(),
            rb.len() as u64 + rb.total_evicted(),
            "total_pushed != len + total_evicted"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Clear
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() empties the buffer completely.
    #[test]
    fn prop_clear_resets_all(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        rb.clear();

        prop_assert!(rb.is_empty());
        prop_assert_eq!(rb.len(), 0);
        prop_assert_eq!(rb.front(), None);
        prop_assert_eq!(rb.back(), None);
        prop_assert_eq!(rb.iter().count(), 0);
        // Note: total_pushed is NOT reset by clear (per API)
    }

    /// After clear, new pushes work normally.
    #[test]
    fn prop_clear_then_reuse(
        capacity in arb_capacity(),
        items1 in arb_items(20),
        items2 in arb_items(20),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items1 {
            rb.push(item);
        }

        rb.clear();

        for &item in &items2 {
            rb.push(item);
        }

        let expected_len = items2.len().min(capacity);
        prop_assert_eq!(rb.len(), expected_len);

        let expected_start = if items2.len() > capacity {
            items2.len() - capacity
        } else {
            0
        };
        let expected: Vec<&i32> = items2[expected_start..].iter().collect();
        let actual: Vec<&i32> = rb.iter().collect();
        prop_assert_eq!(actual, expected);
    }
}

// ────────────────────────────────────────────────────────────────────
// Drain
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// drain() returns all items oldest→newest and empties the buffer.
    #[test]
    fn prop_drain_returns_correct_order(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        // Snapshot expected order before drain
        let expected: Vec<i32> = rb.iter().copied().collect();
        let drained = rb.drain();

        prop_assert_eq!(drained, expected, "Drain order mismatch");
        prop_assert!(rb.is_empty());
        prop_assert_eq!(rb.len(), 0);
    }

    /// drain() on empty buffer returns empty vec.
    #[test]
    fn prop_drain_empty(
        capacity in arb_capacity(),
    ) {
        let mut rb: RingBuffer<i32> = RingBuffer::new(capacity);
        let drained = rb.drain();
        prop_assert!(drained.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// Iterator
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Iterator count == len().
    #[test]
    fn prop_iter_count_matches_len(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }
        prop_assert_eq!(rb.iter().count(), rb.len());
    }

    /// Iterator size_hint is exact.
    #[test]
    fn prop_iter_exact_size(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let iter = rb.iter();
        let (lo, hi) = iter.size_hint();
        let len = iter.len();

        prop_assert_eq!(lo, rb.len());
        prop_assert_eq!(hi, Some(rb.len()));
        prop_assert_eq!(len, rb.len());
    }

    /// to_vec matches iter collection.
    #[test]
    fn prop_to_vec_matches_iter(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let iter_vec: Vec<&i32> = rb.iter().collect();
        let to_vec = rb.to_vec();
        prop_assert_eq!(to_vec, iter_vec);
    }

    /// to_owned_vec matches iter().cloned().
    #[test]
    fn prop_to_owned_vec_matches_iter(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let iter_vec: Vec<i32> = rb.iter().copied().collect();
        let owned = rb.to_owned_vec();
        prop_assert_eq!(owned, iter_vec);
    }
}

// ────────────────────────────────────────────────────────────────────
// Stats
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Stats fill_ratio is in [0, 1] and equals len/capacity.
    #[test]
    fn prop_stats_fill_ratio(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let stats = rb.stats();
        let expected_ratio = rb.len() as f64 / rb.capacity() as f64;

        prop_assert!((stats.fill_ratio - expected_ratio).abs() < 1e-9);
        prop_assert!(stats.fill_ratio >= 0.0 && stats.fill_ratio <= 1.0);
    }

    /// Stats serde roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(
        capacity in arb_capacity(),
        items in arb_items(20),
    ) {
        let mut rb = RingBuffer::new(capacity);
        for &item in &items {
            rb.push(item);
        }

        let stats = rb.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: RingBufferStats = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(stats.capacity, back.capacity);
        prop_assert_eq!(stats.len, back.len);
        prop_assert_eq!(stats.total_pushed, back.total_pushed);
        prop_assert_eq!(stats.total_evicted, back.total_evicted);
        prop_assert!((stats.fill_ratio - back.fill_ratio).abs() < 1e-9);
    }
}

// ────────────────────────────────────────────────────────────────────
// Capacity-one edge case
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A capacity-1 buffer always holds exactly the last pushed item.
    #[test]
    fn prop_capacity_one_last_item(
        items in arb_items(20),
    ) {
        let mut rb = RingBuffer::new(1);
        for &item in &items {
            rb.push(item);
        }

        let last = items.last().unwrap();
        prop_assert_eq!(rb.front(), Some(last));
        prop_assert_eq!(rb.back(), Some(last));
        prop_assert_eq!(rb.len(), 1);
    }
}
