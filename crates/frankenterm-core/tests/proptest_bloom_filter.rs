//! Property-based tests for bloom filter invariants.
//!
//! Bead: wa-9nyo
//!
//! Validates:
//! 1. No false negatives: inserted elements always found
//! 2. FP rate bounded: false positive rate stays within theoretical bounds
//! 3. Counting filter: insert/remove roundtrip preserves membership
//! 4. Union correctness: union of two filters contains all elements from both
//! 5. Clear resets: after clear, no elements are found
//! 6. Sizing: optimal_num_bits/optimal_num_hashes produce reasonable values
//! 7. Memory: memory_bytes scales with num_bits

use proptest::prelude::*;

use frankenterm_core::bloom_filter::{
    BloomFilter, CountingBloomFilter, optimal_num_bits, optimal_num_hashes, theoretical_fp_rate,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_item() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 1..32)
}

fn arb_items(count: usize) -> impl Strategy<Value = Vec<Vec<u8>>> {
    proptest::collection::vec(arb_item(), count)
}

fn arb_capacity() -> impl Strategy<Value = usize> {
    100_usize..5000
}

fn arb_fp_rate() -> impl Strategy<Value = f64> {
    // FP rates between 0.001 (0.1%) and 0.2 (20%).
    (1_u32..200).prop_map(|n| n as f64 / 1000.0)
}

// =============================================================================
// Property: No false negatives
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn no_false_negatives(
        items in arb_items(50),
    ) {
        let mut bf = BloomFilter::with_capacity(100, 0.01);

        // Insert all items.
        for item in &items {
            bf.insert(item);
        }

        // Every inserted item MUST be found.
        for item in &items {
            prop_assert!(bf.contains(item),
                "inserted item should always be found (no false negatives)");
        }
    }

    #[test]
    fn no_false_negatives_counting(
        items in arb_items(50),
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);

        for item in &items {
            cbf.insert(item);
        }

        for item in &items {
            prop_assert!(cbf.contains(item),
                "inserted item should always be found in counting filter");
        }
    }
}

// =============================================================================
// Property: FP rate bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn fp_rate_within_bounds(
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let mut bf = BloomFilter::with_capacity(capacity, fp_rate);

        // Insert exactly `capacity` unique items.
        for i in 0..capacity {
            bf.insert(&i.to_le_bytes());
        }

        // Test 10000 items that were NOT inserted.
        let test_count = 10_000;
        let mut false_positives = 0;
        for i in capacity..(capacity + test_count) {
            if bf.contains(&i.to_le_bytes()) {
                false_positives += 1;
            }
        }

        let observed_fp = false_positives as f64 / test_count as f64;

        // Allow 3x the target FP rate to account for randomness.
        // This is generous but still catches gross miscalculation.
        let tolerance = (fp_rate * 3.0).max(0.05);
        prop_assert!(observed_fp <= tolerance,
            "observed FP rate {:.4} exceeds tolerance {:.4} (target {:.4}, cap={}, hashes={})",
            observed_fp, tolerance, fp_rate, bf.num_bits(), bf.num_hashes());
    }
}

// =============================================================================
// Property: Counting filter insert/remove roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn counting_insert_remove_roundtrip(
        items in arb_items(30),
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);

        // Insert all.
        for item in &items {
            cbf.insert(item);
        }

        // Remove all.
        for item in &items {
            cbf.remove(item);
        }

        // Count should be 0 (assuming unique items, which proptest may not guarantee).
        // With potential duplicates, count should be 0 since we remove the same set.
        prop_assert_eq!(cbf.count(), 0,
            "count should be 0 after inserting and removing the same items");
    }

    #[test]
    fn counting_remove_preserves_others(
        items_a in arb_items(15),
        items_b in arb_items(15),
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(200, 0.01);

        // Insert both sets.
        for item in &items_a {
            cbf.insert(item);
        }
        for item in &items_b {
            cbf.insert(item);
        }

        // Remove set A.
        for item in &items_a {
            cbf.remove(item);
        }

        // Set B items should still be present (with possible false positives
        // from hash collisions, but no false negatives).
        for item in &items_b {
            prop_assert!(cbf.contains(item),
                "items from set B should still be found after removing set A");
        }
    }
}

// =============================================================================
// Property: Union correctness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn union_contains_all_elements(
        items_a in arb_items(20),
        items_b in arb_items(20),
    ) {
        let mut bf_a = BloomFilter::with_capacity(100, 0.01);
        let bf_b_clone;

        {
            let mut bf_b = BloomFilter::with_capacity(100, 0.01);

            for item in &items_a {
                bf_a.insert(item);
            }
            for item in &items_b {
                bf_b.insert(item);
            }

            bf_b_clone = bf_b;
        }

        // Union A ∪ B.
        bf_a.union(&bf_b_clone);

        // All items from both sets should be found.
        for item in &items_a {
            prop_assert!(bf_a.contains(item),
                "union should contain items from set A");
        }
        for item in &items_b {
            prop_assert!(bf_a.contains(item),
                "union should contain items from set B");
        }
    }
}

// =============================================================================
// Property: Clear resets
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn clear_resets_filter(
        items in arb_items(30),
    ) {
        let mut bf = BloomFilter::with_capacity(100, 0.01);

        for item in &items {
            bf.insert(item);
        }

        prop_assert!(bf.count() > 0);

        bf.clear();

        prop_assert_eq!(bf.count(), 0, "count should be 0 after clear");

        // After clear, unique items should generally not be found
        // (unless they're degenerate hash collisions, which is extremely unlikely).
        let unique_items: std::collections::HashSet<&Vec<u8>> = items.iter().collect();
        let mut found = 0;
        for item in &unique_items {
            if bf.contains(item) {
                found += 1;
            }
        }

        // With 0 items inserted, theoretical FP rate is 0.
        prop_assert_eq!(found, 0, "no items should be found after clear");
    }

    #[test]
    fn clear_resets_counting_filter(
        items in arb_items(30),
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(100, 0.01);

        for item in &items {
            cbf.insert(item);
        }

        cbf.clear();
        prop_assert_eq!(cbf.count(), 0, "counting filter count should be 0 after clear");
    }
}

// =============================================================================
// Property: Sizing functions produce reasonable values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn optimal_sizing_reasonable(
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let bits = optimal_num_bits(capacity, fp_rate);
        let hashes = optimal_num_hashes(bits, capacity);

        // Bits should be positive and larger than capacity (at least ~7 bits per item for 1% FP).
        prop_assert!(bits > 0, "num_bits should be positive");
        prop_assert!(bits >= capacity,
            "num_bits ({}) should be >= capacity ({})", bits, capacity);

        // Hash count should be reasonable (1 to ~20).
        prop_assert!(hashes >= 1, "num_hashes should be >= 1");
        prop_assert!(hashes <= 30,
            "num_hashes ({}) seems too high for capacity={}, fp_rate={}",
            hashes, capacity, fp_rate);
    }

    #[test]
    fn theoretical_fp_consistent_with_sizing(
        capacity in arb_capacity(),
        fp_rate in arb_fp_rate(),
    ) {
        let bits = optimal_num_bits(capacity, fp_rate);
        let hashes = optimal_num_hashes(bits, capacity);

        // Theoretical FP rate at capacity should be close to the target.
        let theoretical = theoretical_fp_rate(bits, hashes, capacity);

        // Should be within 2x of the target (accounting for integer rounding in bits/hashes).
        let tolerance = (fp_rate * 2.0).max(0.01);
        prop_assert!(theoretical <= tolerance,
            "theoretical FP rate {:.4} exceeds tolerance {:.4} for target {:.4}",
            theoretical, tolerance, fp_rate);
    }
}

// =============================================================================
// Property: Memory scales with bits
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn memory_scales_with_capacity(
        cap_a in 100_usize..500,
        cap_b in 1000_usize..5000,
    ) {
        let bf_small = BloomFilter::with_capacity(cap_a, 0.01);
        let bf_large = BloomFilter::with_capacity(cap_b, 0.01);

        prop_assert!(bf_large.memory_bytes() >= bf_small.memory_bytes(),
            "larger capacity should use more memory: {} (cap={}) >= {} (cap={})",
            bf_large.memory_bytes(), cap_b, bf_small.memory_bytes(), cap_a);
    }

    #[test]
    fn lower_fp_rate_uses_more_memory(
        capacity in arb_capacity(),
    ) {
        let bf_loose = BloomFilter::with_capacity(capacity, 0.1);
        let bf_tight = BloomFilter::with_capacity(capacity, 0.001);

        prop_assert!(bf_tight.memory_bytes() >= bf_loose.memory_bytes(),
            "tighter FP rate should use more memory: {} (0.001) >= {} (0.1)",
            bf_tight.memory_bytes(), bf_loose.memory_bytes());
    }
}

// =============================================================================
// Property: Count tracks insertions
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn count_tracks_insertions(
        n in 1_usize..100,
    ) {
        let mut bf = BloomFilter::with_capacity(200, 0.01);

        for i in 0..n {
            bf.insert(&i.to_le_bytes());
        }

        prop_assert_eq!(bf.count(), n,
            "count should equal number of insertions");
    }

    #[test]
    fn counting_count_tracks_net(
        n in 1_usize..50,
    ) {
        let mut cbf = CountingBloomFilter::with_capacity(200, 0.01);

        for i in 0..n {
            cbf.insert(&i.to_le_bytes());
        }
        prop_assert_eq!(cbf.count(), n);

        // Remove half.
        let half = n / 2;
        for i in 0..half {
            cbf.remove(&i.to_le_bytes());
        }
        prop_assert_eq!(cbf.count(), n - half,
            "count after removing {} of {} should be {}", half, n, n - half);
    }
}

// =============================================================================
// Property: Estimated FP rate monotonically increases with fill
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn estimated_fp_increases_with_fill(
        capacity in 500_usize..2000,
    ) {
        let mut bf = BloomFilter::with_capacity(capacity, 0.01);

        let mut prev_fp = 0.0;
        let step = capacity / 10;

        for chunk in 0..10 {
            for i in (chunk * step)..((chunk + 1) * step) {
                bf.insert(&i.to_le_bytes());
            }

            let fp = bf.estimated_fp_rate();
            prop_assert!(fp >= prev_fp,
                "estimated FP rate should not decrease: {} -> {} at count {}",
                prev_fp, fp, bf.count());
            prev_fp = fp;
        }
    }
}
