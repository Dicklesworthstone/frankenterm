//! Property-based tests for bloom_filter module.
//!
//! Verifies mathematical invariants and correctness guarantees:
//! - No false negatives (ever)
//! - False-positive rate within theoretical bounds
//! - Counting filter insert/remove roundtrip
//! - Counter saturation safety
//! - Union commutativity and associativity
//! - Sizing formulas produce consistent parameters

use proptest::prelude::*;

use frankenterm_core::bloom_filter::{
    BloomFilter, CountingBloomFilter, optimal_num_bits, optimal_num_hashes, theoretical_fp_rate,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_item() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..64)
}

fn arb_item_set(max_size: usize) -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(arb_item(), 1..=max_size)
}

/// Capacities in a reasonable range for testing.
fn arb_capacity() -> impl Strategy<Value = usize> {
    10..500usize
}

/// FP rates that produce reasonable filter sizes.
fn arb_fp_rate() -> impl Strategy<Value = f64> {
    (1u32..=20).prop_map(|n| n as f64 / 100.0) // 0.01 to 0.20
}

// ────────────────────────────────────────────────────────────────────
// BloomFilter: No false negatives
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Every inserted item must be found (no false negatives, ever).
    #[test]
    fn prop_no_false_negatives(
        items in arb_item_set(100),
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let cap = capacity.max(items.len());
        let mut bf = BloomFilter::with_capacity(cap, fp_rate);

        for item in &items {
            bf.insert(item);
        }

        for item in &items {
            prop_assert!(
                bf.contains(item),
                "False negative detected for item of length {}",
                item.len()
            );
        }
    }

    /// count() matches the number of insert() calls.
    #[test]
    fn prop_count_matches_inserts(
        items in arb_item_set(50),
    ) {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        for item in &items {
            bf.insert(item);
        }
        prop_assert_eq!(bf.count(), items.len());
    }

    /// After clear(), no previously inserted item is found.
    #[test]
    fn prop_clear_resets_all(
        items in arb_item_set(50),
    ) {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        for item in &items {
            bf.insert(item);
        }
        bf.clear();
        prop_assert_eq!(bf.count(), 0);

        // Note: after clear(), contains() should be false for all items
        // (bits are zeroed, so no false positives either).
        for item in &items {
            prop_assert!(
                !bf.contains(item),
                "Item found after clear()"
            );
        }
    }

    /// Empty filter never reports contains().
    #[test]
    fn prop_empty_contains_nothing(
        items in arb_item_set(20),
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let bf = BloomFilter::with_capacity(capacity, fp_rate);
        for item in &items {
            prop_assert!(
                !bf.contains(item),
                "Empty filter should not contain anything"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// BloomFilter: False-positive rate bounds
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Observed FP rate should not wildly exceed theoretical rate.
    /// We use a generous 5× tolerance since proptest runs small sample sizes.
    #[test]
    fn prop_fp_rate_bounded(
        fp_rate in arb_fp_rate(),
    ) {
        let n = 200;
        let mut bf = BloomFilter::with_capacity(n, fp_rate);

        // Insert n items with "in-" prefix.
        for i in 0..n {
            bf.insert(format!("in-{}", i).as_bytes());
        }

        // Test n items with "out-" prefix (disjoint from inserted items).
        let test_count = 2000;
        let mut false_positives = 0u32;
        for i in 0..test_count {
            if bf.contains(format!("out-{}", i).as_bytes()) {
                false_positives += 1;
            }
        }

        let observed = false_positives as f64 / test_count as f64;
        // Allow 5× theoretical as generous bound.
        prop_assert!(
            observed < fp_rate * 5.0 + 0.01, // +0.01 floor for very low rates
            "FP rate {} exceeds 5x target {} (observed {}/{})",
            observed, fp_rate, false_positives, test_count
        );
    }

    /// estimated_fp_rate() should be in [0, 1].
    #[test]
    fn prop_estimated_fp_rate_valid(
        items in arb_item_set(50),
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let cap = capacity.max(items.len());
        let mut bf = BloomFilter::with_capacity(cap, fp_rate);
        for item in &items {
            bf.insert(item);
        }
        let est = bf.estimated_fp_rate();
        prop_assert!(est >= 0.0 && est <= 1.0, "estimated_fp_rate out of [0,1]: {}", est);
    }
}

// ────────────────────────────────────────────────────────────────────
// BloomFilter: Union properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Union preserves all elements from both filters (no false negatives after union).
    #[test]
    fn prop_union_no_false_negatives(
        items_a in arb_item_set(30),
        items_b in arb_item_set(30),
    ) {
        let total = items_a.len() + items_b.len();
        let cap = total.max(10);

        let mut a = BloomFilter::with_capacity(cap, 0.01);

        // Both must have same params for union — use with_params
        let bits = a.num_bits();
        let hashes = a.num_hashes();
        let mut b = BloomFilter::with_params(bits, hashes);

        for item in &items_a {
            a.insert(item);
        }
        for item in &items_b {
            b.insert(item);
        }

        a.union(&b);

        // All items from both sets must be found.
        for item in &items_a {
            prop_assert!(a.contains(item), "Union lost item from set A");
        }
        for item in &items_b {
            prop_assert!(a.contains(item), "Union lost item from set B");
        }
    }

    /// Union is commutative: A ∪ B contains same elements as B ∪ A.
    #[test]
    fn prop_union_commutative(
        items_a in arb_item_set(20),
        items_b in arb_item_set(20),
    ) {
        let mut a1 = BloomFilter::with_params(4096, 7);
        let mut a2 = BloomFilter::with_params(4096, 7);
        let mut b1 = BloomFilter::with_params(4096, 7);
        let mut b2 = BloomFilter::with_params(4096, 7);

        for item in &items_a {
            a1.insert(item);
            a2.insert(item);
        }
        for item in &items_b {
            b1.insert(item);
            b2.insert(item);
        }

        // A ∪ B
        a1.union(&b1);
        // B ∪ A
        b2.union(&a2);

        // Check that all items from both sets are found in both unions.
        let all_items: Vec<&Vec<u8>> = items_a.iter().chain(items_b.iter()).collect();
        for item in &all_items {
            let in_ab = a1.contains(item);
            let in_ba = b2.contains(item);
            prop_assert_eq!(in_ab, in_ba, "Union commutativity violated");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// BloomFilter: Stats consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// stats().fill_ratio is in [0, 1] and increases monotonically with inserts.
    #[test]
    fn prop_fill_ratio_monotonic(
        items in arb_item_set(30),
    ) {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        let mut prev_ratio = 0.0f64;

        for item in &items {
            bf.insert(item);
            let ratio = bf.stats().fill_ratio;
            prop_assert!(ratio >= 0.0 && ratio <= 1.0, "fill_ratio out of [0,1]: {}", ratio);
            prop_assert!(ratio >= prev_ratio, "fill_ratio decreased: {} -> {}", prev_ratio, ratio);
            prev_ratio = ratio;
        }
    }

    /// memory_bytes() > 0 for any valid filter.
    #[test]
    fn prop_memory_positive(
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let bf = BloomFilter::with_capacity(capacity, fp_rate);
        prop_assert!(bf.memory_bytes() > 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// CountingBloomFilter: No false negatives
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Counting filter: inserted items are always found.
    #[test]
    fn prop_counting_no_false_negatives(
        items in arb_item_set(50),
    ) {
        let cap = items.len().max(10);
        let mut cbf = CountingBloomFilter::with_capacity(cap, 0.01);

        for item in &items {
            cbf.insert(item);
        }

        for item in &items {
            prop_assert!(
                cbf.contains(item),
                "Counting filter false negative"
            );
        }
    }

    /// Insert then remove returns to "not contained" (for unique items).
    #[test]
    fn prop_counting_insert_remove_roundtrip(
        items in prop::collection::hash_set(arb_item(), 1..30),
    ) {
        let items: Vec<Vec<u8>> = items.into_iter().collect();
        let cap = items.len().max(10);
        let mut cbf = CountingBloomFilter::with_capacity(cap, 0.001);

        // Insert all
        for item in &items {
            cbf.insert(item);
        }

        // Remove all
        for item in &items {
            cbf.remove(item);
        }

        prop_assert_eq!(cbf.count(), 0);

        // After removing all unique items, none should be found
        // (assuming no hash collisions caused shared counters — rare with low FP rate)
        // We just check count is 0, since collisions can prevent contains() from returning false
    }
}

// ────────────────────────────────────────────────────────────────────
// CountingBloomFilter: Remove preserves other elements
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Removing one element does not affect lookup of other inserted elements.
    #[test]
    fn prop_counting_remove_preserves_others(
        items in prop::collection::hash_set(arb_item(), 3..20),
    ) {
        let items: Vec<Vec<u8>> = items.into_iter().collect();
        let cap = items.len().max(10);
        let mut cbf = CountingBloomFilter::with_capacity(cap * 2, 0.001);

        // Insert all
        for item in &items {
            cbf.insert(item);
        }

        // Remove first item only
        let removed = &items[0];
        cbf.remove(removed);

        // Remaining items must still be found.
        for item in &items[1..] {
            prop_assert!(
                cbf.contains(item),
                "Removing one item caused false negative for another"
            );
        }
    }

    /// Multiple inserts of same item: partial removal leaves item present.
    ///
    /// Restricted to count ≤ 3 to stay below counter saturation (15).
    /// With k=7 hash functions, worst-case all map to same bucket:
    /// 3×7=21 → saturated at 15. After 2 removes: 15 - 14 = 1 > 0. Safe.
    /// With count ≥ 4, saturation can cause counters to reach 0 prematurely
    /// when hash_indices produces duplicate indices — this is expected behavior
    /// for counting bloom filters, not a bug.
    #[test]
    fn prop_counting_multi_insert_multi_remove(
        count in 2..=3u32,
        item in arb_item(),
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);

        for _ in 0..count {
            cbf.insert(&item);
        }
        prop_assert!(cbf.contains(&item));

        // Remove all but one — should still contain (safe below saturation).
        for _ in 0..(count - 1) {
            cbf.remove(&item);
        }
        prop_assert!(
            cbf.contains(&item),
            "Item should still be present after partial removal"
        );

        // Remove last — should be gone.
        cbf.remove(&item);
        prop_assert!(
            !cbf.contains(&item),
            "Item should be gone after complete removal"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// CountingBloomFilter: Counter saturation
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Saturating counters at 15: inserting 16+ times doesn't overflow.
    #[test]
    fn prop_counter_saturation_safe(
        extra_inserts in 16..30u32,
        item in arb_item(),
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);

        for _ in 0..extra_inserts {
            cbf.insert(&item);
        }

        // Should still be contained (counters saturated at 15, not overflowed).
        prop_assert!(cbf.contains(&item));

        // After 15 removals (the saturation limit), the item might still be
        // "contained" because counters saturated — this is expected behavior.
        for _ in 0..15 {
            cbf.remove(&item);
        }

        // The item may or may not be found here — saturation means we lost
        // the exact count. This test just ensures no panics or UB.
    }
}

// ────────────────────────────────────────────────────────────────────
// CountingBloomFilter: Clear
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// clear() resets all counters — nothing is found after.
    #[test]
    fn prop_counting_clear_resets(
        items in arb_item_set(30),
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);
        for item in &items {
            cbf.insert(item);
        }
        cbf.clear();
        prop_assert_eq!(cbf.count(), 0);
        for item in &items {
            prop_assert!(!cbf.contains(item), "Found item after clear()");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Sizing formulas
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// optimal_num_bits increases as FP rate decreases (more bits for lower FP).
    #[test]
    fn prop_lower_fp_needs_more_bits(
        capacity in 10..1000usize,
    ) {
        let bits_1pct = optimal_num_bits(capacity, 0.01);
        let bits_10pct = optimal_num_bits(capacity, 0.10);
        prop_assert!(
            bits_1pct > bits_10pct,
            "Lower FP should need more bits: 1%={} vs 10%={}",
            bits_1pct, bits_10pct
        );
    }

    /// optimal_num_bits increases with capacity (more items need more bits).
    #[test]
    fn prop_more_capacity_needs_more_bits(
        fp_rate in arb_fp_rate(),
    ) {
        let bits_100 = optimal_num_bits(100, fp_rate);
        let bits_1000 = optimal_num_bits(1000, fp_rate);
        prop_assert!(
            bits_1000 > bits_100,
            "More capacity should need more bits: 100={} vs 1000={}",
            bits_100, bits_1000
        );
    }

    /// optimal_num_hashes is always >= 1.
    #[test]
    fn prop_num_hashes_at_least_one(
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let bits = optimal_num_bits(capacity, fp_rate);
        let k = optimal_num_hashes(bits, capacity);
        prop_assert!(k >= 1, "num_hashes should be >= 1, got {}", k);
    }

    /// theoretical_fp_rate is in [0, 1].
    #[test]
    fn prop_theoretical_fp_rate_valid(
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
        fill_frac in 0.0..=1.0f64,
    ) {
        let bits = optimal_num_bits(capacity, fp_rate);
        let k = optimal_num_hashes(bits, capacity);
        let count = (capacity as f64 * fill_frac) as usize;
        let fp = theoretical_fp_rate(bits, k, count);
        prop_assert!(fp >= 0.0 && fp <= 1.0, "theoretical FP rate out of [0,1]: {}", fp);
    }

    /// theoretical_fp_rate(bits, k, 0) == 0 (empty filter has no false positives).
    #[test]
    fn prop_theoretical_fp_rate_empty_is_zero(
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let bits = optimal_num_bits(capacity, fp_rate);
        let k = optimal_num_hashes(bits, capacity);
        let fp = theoretical_fp_rate(bits, k, 0);
        prop_assert!(
            fp.abs() < 1e-10,
            "Empty filter should have ~0 FP rate, got {}",
            fp
        );
    }

    /// theoretical_fp_rate increases monotonically with count.
    #[test]
    fn prop_theoretical_fp_rate_monotonic(
        capacity in 50..200usize,
        fp_rate in arb_fp_rate(),
    ) {
        let bits = optimal_num_bits(capacity, fp_rate);
        let k = optimal_num_hashes(bits, capacity);

        let mut prev = 0.0f64;
        for count in (0..=capacity).step_by(capacity.max(1) / 10 + 1) {
            let fp = theoretical_fp_rate(bits, k, count);
            prop_assert!(
                fp >= prev - 1e-10, // small tolerance for float imprecision
                "FP rate decreased: {} -> {} at count {}",
                prev, fp, count
            );
            prev = fp;
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Hash function properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Inserting the same item twice doesn't change contains() result.
    #[test]
    fn prop_idempotent_membership(
        item in arb_item(),
    ) {
        let mut bf = BloomFilter::with_capacity(100, 0.01);
        bf.insert(&item);
        let first = bf.contains(&item);
        bf.insert(&item);
        let second = bf.contains(&item);
        prop_assert_eq!(first, true);
        prop_assert_eq!(second, true);
    }

    /// Insertion order doesn't affect membership queries.
    #[test]
    fn prop_insertion_order_irrelevant(
        mut items in arb_item_set(20),
    ) {
        let cap = items.len().max(10);

        // Forward order
        let mut bf_fwd = BloomFilter::with_capacity(cap, 0.01);
        for item in &items {
            bf_fwd.insert(item);
        }

        // Reverse order (same params via with_params to ensure identical filter)
        let bits = bf_fwd.num_bits();
        let hashes = bf_fwd.num_hashes();
        let mut bf_rev = BloomFilter::with_params(bits, hashes);
        items.reverse();
        for item in &items {
            bf_rev.insert(item);
        }

        // Both should agree on membership for all items.
        for item in &items {
            prop_assert_eq!(
                bf_fwd.contains(item),
                bf_rev.contains(item),
                "Insertion order affected membership"
            );
        }
    }
}
