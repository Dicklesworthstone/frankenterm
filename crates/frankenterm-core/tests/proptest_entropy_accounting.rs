//! Property-based tests for entropy accounting invariants.
//!
//! Bead: wa-86q7
//!
//! Validates:
//! 1. Entropy bounds: H ∈ [0, 8] bits/byte for any input
//! 2. Entropy extremes: constant data → H ≈ 0, uniform → H ≈ 8
//! 3. Incremental matches batch: streaming update ≈ one-shot compute
//! 4. Information cost bounds: 0 ≤ I ≤ raw_bytes
//! 5. Compression ratio: CR ≥ 1.0 for all valid entropy
//! 6. Eviction score decay: score decreases with age
//! 7. Eviction order: sorting by score yields ascending order
//! 8. Budget invariants: add/remove consistent, utilization bounded
//! 9. Reset: estimator reset yields zero entropy
//! 10. Window decay: entropy adapts to recent data after decay

use proptest::prelude::*;

use frankenterm_core::entropy_accounting::{
    EntropyEstimator, EvictionConfig, InformationBudget, compute_entropy, eviction_order,
    eviction_score, information_cost,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_bytes(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 1..max_len)
}

fn arb_constant_bytes() -> impl Strategy<Value = (u8, usize)> {
    (any::<u8>(), 100_usize..5000)
}

fn arb_two_symbol_data() -> impl Strategy<Value = Vec<u8>> {
    // Data with exactly 2 distinct byte values in equal proportion.
    (any::<u8>(), any::<u8>(), 500_usize..2000)
        .prop_filter("need distinct symbols", |(a, b, _)| a != b)
        .prop_map(|(a, b, half)| {
            let mut data = vec![a; half];
            data.extend(vec![b; half]);
            data
        })
}

fn arb_uniform_data() -> impl Strategy<Value = Vec<u8>> {
    // Data with all 256 byte values in equal counts.
    (50_usize..200).prop_map(|repeats| {
        let mut data = Vec::with_capacity(256 * repeats);
        for _ in 0..repeats {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        data
    })
}

fn arb_window_size() -> impl Strategy<Value = usize> {
    1000_usize..50_000
}

fn arb_eviction_config() -> impl Strategy<Value = EvictionConfig> {
    (1_u64..600_000, 0.0_f64..10_000.0).prop_map(|(half_life, threshold)| EvictionConfig {
        recency_half_life_ms: half_life,
        min_cost_threshold: threshold,
    })
}

fn arb_info_cost() -> impl Strategy<Value = f64> {
    (0.0_f64..100_000.0).prop_map(|v| v)
}

fn arb_age_ms() -> impl Strategy<Value = u64> {
    0_u64..1_000_000
}

// =============================================================================
// Property: Entropy bounds [0, 8]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn entropy_always_in_valid_range(
        data in arb_bytes(5000),
    ) {
        let h = compute_entropy(&data);
        prop_assert!(h >= 0.0, "entropy should be >= 0, got {}", h);
        prop_assert!(h <= 8.0, "entropy should be <= 8, got {}", h);
    }
}

// =============================================================================
// Property: Constant data → H ≈ 0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn constant_data_zero_entropy(
        (byte_val, count) in arb_constant_bytes(),
    ) {
        let data = vec![byte_val; count];
        let h = compute_entropy(&data);
        prop_assert!(h < 0.001,
            "constant data (byte={}, n={}) should have H ≈ 0, got {}",
            byte_val, count, h);
    }
}

// =============================================================================
// Property: Two equal-frequency symbols → H ≈ 1.0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn two_symbols_entropy_near_one(
        data in arb_two_symbol_data(),
    ) {
        let h = compute_entropy(&data);
        prop_assert!((h - 1.0).abs() < 0.05,
            "two equal-frequency symbols should have H ≈ 1.0, got {}", h);
    }
}

// =============================================================================
// Property: Uniform 256-symbol data → H ≈ 8.0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn uniform_data_entropy_near_eight(
        data in arb_uniform_data(),
    ) {
        let h = compute_entropy(&data);
        prop_assert!((h - 8.0).abs() < 0.01,
            "uniform 256-symbol data should have H ≈ 8.0, got {}", h);
    }
}

// =============================================================================
// Property: Incremental estimator matches batch computation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn incremental_matches_batch(
        data in arb_bytes(5000),
    ) {
        let batch_h = compute_entropy(&data);

        let mut est = EntropyEstimator::new(data.len());
        for &b in &data {
            est.update(b);
        }
        let inc_h = est.entropy();

        prop_assert!((batch_h - inc_h).abs() < 0.05,
            "incremental ({}) should match batch ({})", inc_h, batch_h);
    }

    #[test]
    fn block_update_matches_byte_update(
        data in arb_bytes(5000),
    ) {
        let mut est_byte = EntropyEstimator::new(data.len());
        for &b in &data {
            est_byte.update(b);
        }

        let mut est_block = EntropyEstimator::new(data.len());
        est_block.update_block(&data);

        let h_byte = est_byte.entropy();
        let h_block = est_block.entropy();

        prop_assert!((h_byte - h_block).abs() < 0.001,
            "byte-by-byte ({}) should match block ({})", h_byte, h_block);
    }
}

// =============================================================================
// Property: Estimator entropy always in valid range
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn estimator_entropy_in_range(
        data in arb_bytes(10000),
        window in arb_window_size(),
    ) {
        let mut est = EntropyEstimator::new(window);
        est.update_block(&data);
        let h = est.entropy();
        prop_assert!(h >= 0.0, "estimator entropy should be >= 0, got {}", h);
        prop_assert!(h <= 8.0, "estimator entropy should be <= 8, got {}", h);
    }
}

// =============================================================================
// Property: Information cost bounds
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn info_cost_bounded(
        data in arb_bytes(5000),
    ) {
        let h = compute_entropy(&data);
        let cost = information_cost(data.len(), h);

        prop_assert!(cost >= 0.0,
            "information cost should be >= 0, got {}", cost);
        prop_assert!(cost <= data.len() as f64 + 0.01,
            "information cost ({}) should be <= raw size ({})",
            cost, data.len());
    }

    #[test]
    fn info_cost_zero_for_zero_entropy(
        raw_bytes in 1_usize..100_000,
    ) {
        let cost = information_cost(raw_bytes, 0.0);
        prop_assert!((cost - 0.0).abs() < 0.001,
            "info cost at H=0 should be 0, got {}", cost);
    }

    #[test]
    fn info_cost_equals_raw_at_max_entropy(
        raw_bytes in 1_usize..100_000,
    ) {
        let cost = information_cost(raw_bytes, 8.0);
        prop_assert!((cost - raw_bytes as f64).abs() < 0.01,
            "info cost at H=8 should equal raw_bytes, got {} vs {}",
            cost, raw_bytes);
    }

    #[test]
    fn info_cost_monotonic_with_entropy(
        raw_bytes in 100_usize..10_000,
    ) {
        let mut prev_cost = 0.0;
        for h_int in 0..=80 {
            let h = h_int as f64 / 10.0;
            let cost = information_cost(raw_bytes, h);
            prop_assert!(cost >= prev_cost,
                "info cost should increase with entropy: h={}, cost={}, prev={}",
                h, cost, prev_cost);
            prev_cost = cost;
        }
    }
}

// =============================================================================
// Property: Compression ratio bound ≥ 1.0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn compression_ratio_at_least_one(
        data in arb_bytes(5000),
    ) {
        let mut est = EntropyEstimator::new(data.len());
        est.update_block(&data);
        let cr = est.compression_ratio_bound();
        prop_assert!(cr >= 1.0,
            "compression ratio bound should be >= 1.0, got {}", cr);
    }

    #[test]
    fn compression_ratio_infinite_for_constant(
        (byte_val, count) in arb_constant_bytes(),
    ) {
        let mut est = EntropyEstimator::new(count);
        for _ in 0..count {
            est.update(byte_val);
        }
        prop_assert!(est.compression_ratio_bound().is_infinite(),
            "constant data should have infinite compression ratio");
    }
}

// =============================================================================
// Property: Eviction score decay
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn eviction_score_monotonically_decreases_with_age(
        info_cost in arb_info_cost(),
        config in arb_eviction_config(),
    ) {
        let mut prev_score = f64::INFINITY;
        for age_step in 0..10 {
            let age_ms = age_step as u64 * config.recency_half_life_ms / 2;
            let score = eviction_score(info_cost, age_ms, &config);
            prop_assert!(score <= prev_score + 0.001,
                "score should not increase with age: age_ms={}, score={}, prev={}",
                age_ms, score, prev_score);
            prev_score = score;
        }
    }

    #[test]
    fn eviction_score_halves_at_halflife(
        info_cost in 100.0_f64..10_000.0,
        half_life in 1000_u64..600_000,
    ) {
        let config = EvictionConfig {
            recency_half_life_ms: half_life,
            min_cost_threshold: 0.0,
        };
        let score_fresh = eviction_score(info_cost, 0, &config);
        let score_at_hl = eviction_score(info_cost, half_life, &config);

        // Score at one half-life should be ~half the fresh score.
        let expected = score_fresh / 2.0;
        prop_assert!((score_at_hl - expected).abs() < 1.0,
            "score at half-life should be ~{}, got {}",
            expected, score_at_hl);
    }

    #[test]
    fn eviction_score_nonnegative(
        info_cost in arb_info_cost(),
        age_ms in arb_age_ms(),
        config in arb_eviction_config(),
    ) {
        let score = eviction_score(info_cost, age_ms, &config);
        prop_assert!(score >= 0.0,
            "eviction score should be non-negative, got {}", score);
    }
}

// =============================================================================
// Property: Eviction order is sorted
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn eviction_order_is_sorted_ascending(
        raw_scores in proptest::collection::vec(0.0_f64..100_000.0, 1..20),
    ) {
        // Assign unique IDs to each score entry.
        let scores: Vec<(u64, f64)> = raw_scores.iter()
            .enumerate()
            .map(|(i, &s)| (i as u64, s))
            .collect();

        let order = eviction_order(&scores);

        // Verify all IDs are present.
        prop_assert_eq!(order.len(), scores.len());

        // Build a lookup from ID → score.
        let lookup: std::collections::HashMap<u64, f64> =
            scores.iter().copied().collect();

        // Verify order is by ascending score.
        for window in order.windows(2) {
            let score_a = lookup[&window[0]];
            let score_b = lookup[&window[1]];
            prop_assert!(score_a <= score_b,
                "eviction order should be ascending: {} (score={}) before {} (score={})",
                window[0], score_a, window[1], score_b);
        }
    }
}

// =============================================================================
// Property: Budget add/remove consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn budget_add_remove_roundtrip(
        costs in proptest::collection::vec(0.0_f64..10_000.0, 1..20),
        budget_limit in 1000.0_f64..1_000_000.0,
    ) {
        let mut budget = InformationBudget::new(budget_limit);

        // Add all.
        for &cost in &costs {
            budget.add(cost);
        }
        prop_assert_eq!(budget.pane_count, costs.len());

        let expected_total: f64 = costs.iter().sum();
        prop_assert!((budget.current_cost - expected_total).abs() < 0.01,
            "total cost should be sum of adds: {} vs {}", budget.current_cost, expected_total);

        // Remove all.
        for &cost in &costs {
            budget.remove(cost);
        }
        prop_assert_eq!(budget.pane_count, 0);
        prop_assert!(budget.current_cost.abs() < 0.01,
            "cost after full removal should be ~0, got {}", budget.current_cost);
    }

    #[test]
    fn budget_utilization_consistent(
        adds in proptest::collection::vec(0.0_f64..5_000.0, 1..10),
        budget_limit in 1000.0_f64..100_000.0,
    ) {
        let mut budget = InformationBudget::new(budget_limit);
        for &cost in &adds {
            budget.add(cost);
        }

        let expected_util = budget.current_cost / budget.budget_bytes;
        prop_assert!((budget.utilization() - expected_util).abs() < 0.001,
            "utilization should be current/budget: {} vs {}", budget.utilization(), expected_util);

        if budget.utilization() > 1.0 {
            prop_assert!(budget.is_exceeded());
            prop_assert!(budget.overage() > 0.0);
        } else {
            prop_assert!(!budget.is_exceeded());
            prop_assert!((budget.overage() - 0.0).abs() < 0.01);
        }
    }

    #[test]
    fn budget_remove_clamps_at_zero(
        initial_cost in 0.0_f64..1000.0,
        remove_cost in 1000.0_f64..10_000.0,
    ) {
        let mut budget = InformationBudget::new(50_000.0);
        budget.add(initial_cost);
        budget.remove(remove_cost);
        prop_assert!(budget.current_cost >= 0.0,
            "cost should never go negative after remove, got {}", budget.current_cost);
    }
}

// =============================================================================
// Property: Estimator reset
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn reset_zeroes_everything(
        data in arb_bytes(5000),
        window in arb_window_size(),
    ) {
        let mut est = EntropyEstimator::new(window);
        est.update_block(&data);

        // Verify non-trivial state before reset.
        prop_assert!(est.total_bytes() > 0);

        est.reset();
        prop_assert_eq!(est.total_bytes(), 0);
        prop_assert!((est.entropy() - 0.0).abs() < 0.001,
            "entropy after reset should be 0, got {}", est.entropy());
    }
}

// =============================================================================
// Property: Fill ratio correctness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fill_ratio_consistent(
        data_len in 1_usize..5000,
        window in arb_window_size(),
    ) {
        let mut est = EntropyEstimator::new(window);
        let data: Vec<u8> = (0..data_len).map(|i| (i % 256) as u8).collect();
        est.update_block(&data);

        // fill_ratio = total / window. But decay may have occurred.
        let ratio = est.fill_ratio();
        prop_assert!(ratio >= 0.0,
            "fill ratio should be >= 0, got {}", ratio);
        // After decay, ratio should be <= 2.0 (decay triggers at 2×).
        prop_assert!(ratio <= 2.1,
            "fill ratio should be <= ~2.0 (decay threshold), got {}", ratio);
    }
}

// =============================================================================
// Property: Adding more distinct symbols increases entropy
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn more_symbols_higher_entropy(
        count_per_symbol in 100_usize..500,
    ) {
        // 1 symbol → H = 0
        let data_1 = vec![0u8; count_per_symbol];
        let h_1 = compute_entropy(&data_1);

        // 2 symbols → H = 1
        let mut data_2 = vec![0u8; count_per_symbol];
        data_2.extend(vec![1u8; count_per_symbol]);
        let h_2 = compute_entropy(&data_2);

        // 4 symbols → H = 2
        let mut data_4 = Vec::new();
        for sym in 0..4u8 {
            data_4.extend(vec![sym; count_per_symbol]);
        }
        let h_4 = compute_entropy(&data_4);

        // 16 symbols → H = 4
        let mut data_16 = Vec::new();
        for sym in 0..16u8 {
            data_16.extend(vec![sym; count_per_symbol]);
        }
        let h_16 = compute_entropy(&data_16);

        prop_assert!(h_1 < h_2, "1 sym H ({}) should be < 2 sym H ({})", h_1, h_2);
        prop_assert!(h_2 < h_4, "2 sym H ({}) should be < 4 sym H ({})", h_2, h_4);
        prop_assert!(h_4 < h_16, "4 sym H ({}) should be < 16 sym H ({})", h_4, h_16);
    }
}

// =============================================================================
// Property: Window decay adapts to new data
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn decay_shifts_entropy_toward_recent(
        window in 500_usize..2000,
    ) {
        let mut est = EntropyEstimator::new(window);

        // Phase 1: feed constant data → H ≈ 0.
        for _ in 0..(window * 3) {
            est.update(0);
        }
        let h_after_constant = est.entropy();
        prop_assert!(h_after_constant < 0.5,
            "after constant data, H should be low, got {}", h_after_constant);

        // Phase 2: feed uniform data → H should rise.
        for i in 0..(window * 3) {
            est.update((i % 256) as u8);
        }
        let h_after_uniform = est.entropy();
        prop_assert!(h_after_uniform > 5.0,
            "after uniform data, H should be high, got {}", h_after_uniform);
    }
}

// =============================================================================
// Property: Eviction score proportional to info cost
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn higher_info_cost_higher_eviction_score(
        cost_low in 0.0_f64..5_000.0,
        cost_delta in 1.0_f64..5_000.0,
        age_ms in arb_age_ms(),
        config in arb_eviction_config(),
    ) {
        let cost_high = cost_low + cost_delta;
        let score_low = eviction_score(cost_low, age_ms, &config);
        let score_high = eviction_score(cost_high, age_ms, &config);
        prop_assert!(score_high >= score_low,
            "higher cost ({}) should have >= score than lower cost ({}): {} vs {}",
            cost_high, cost_low, score_high, score_low);
    }
}
