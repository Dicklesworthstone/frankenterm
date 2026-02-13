//! Property-based tests for token_bucket module.
//!
//! Verifies the token bucket rate limiter invariants:
//! - Tokens never exceed capacity after any operation
//! - Monotonic consumption: total_consumed only increases
//! - Refill correctness: tokens accumulate at refill_rate, capped at capacity
//! - Wait time correctness: wait_time_ms consistent with deficit/rate
//! - Reset restores full capacity
//! - Stats consistency: fill_ratio in [0, 1], serde roundtrip
//! - Hierarchical atomicity: denied operations consume from neither bucket
//! - Config build: start_empty vs start_full behavior

use proptest::prelude::*;

use frankenterm_core::token_bucket::{
    BucketConfig, BucketStats, HierarchicalBucket, HierarchicalResult, TokenBucket,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_capacity() -> impl Strategy<Value = f64> {
    (1u32..100).prop_map(|c| c as f64)
}

fn arb_refill_rate() -> impl Strategy<Value = f64> {
    (1u32..50).prop_map(|r| r as f64)
}

fn arb_cost() -> impl Strategy<Value = u32> {
    1u32..20
}

fn arb_time_ms() -> impl Strategy<Value = u64> {
    0u64..100_000
}

/// Increasing time sequence (monotonic timestamps).
fn arb_time_sequence(len: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..5000, len).prop_map(|deltas| {
        let mut times = Vec::with_capacity(deltas.len());
        let mut t = 0u64;
        for d in deltas {
            t += d;
            times.push(t);
        }
        times
    })
}

// ────────────────────────────────────────────────────────────────────
// Tokens never exceed capacity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// available() never exceeds capacity, regardless of time elapsed.
    #[test]
    fn prop_tokens_never_exceed_capacity(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        times in arb_time_sequence(20),
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        for &t in &times {
            let avail = b.available(t);
            prop_assert!(
                avail <= capacity + 1e-9,
                "available {} > capacity {} at t={}", avail, capacity, t
            );
        }
    }

    /// After consuming tokens and waiting, available still capped at capacity.
    #[test]
    fn prop_refill_capped_at_capacity(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        consume in arb_cost(),
        wait_ms in 0u64..50_000,
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        b.try_acquire(consume, 0);
        let avail = b.available(wait_ms);
        prop_assert!(
            avail <= capacity + 1e-9,
            "available {} > capacity {} after refill", avail, capacity
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Monotonic total_consumed and total_denied
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// total_consumed never decreases through a sequence of operations.
    #[test]
    fn prop_total_consumed_monotonic(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        costs in prop::collection::vec(arb_cost(), 1..30),
        times in arb_time_sequence(30),
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        let mut prev_consumed = 0u64;
        let mut prev_denied = 0u64;

        for (i, &cost) in costs.iter().enumerate() {
            let t = times.get(i).copied().unwrap_or(0);
            b.try_acquire(cost, t);

            prop_assert!(
                b.total_consumed() >= prev_consumed,
                "total_consumed decreased: {} < {}", b.total_consumed(), prev_consumed
            );
            prop_assert!(
                b.total_denied() >= prev_denied,
                "total_denied decreased: {} < {}", b.total_denied(), prev_denied
            );

            prev_consumed = b.total_consumed();
            prev_denied = b.total_denied();
        }
    }

    /// Each successful acquire increases total_consumed by exactly cost.
    #[test]
    fn prop_successful_acquire_increments_consumed(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        cost in arb_cost(),
        time in arb_time_ms(),
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        let before = b.total_consumed();
        let success = b.try_acquire(cost, time);
        if success {
            prop_assert_eq!(
                b.total_consumed(), before + cost as u64,
                "Consumed didn't increase by cost={}", cost
            );
        } else {
            prop_assert_eq!(
                b.total_consumed(), before,
                "Consumed changed on denied acquire"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Refill correctness
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After emptying and waiting, tokens = min(capacity, residual + rate * elapsed_sec).
    #[test]
    fn prop_refill_amount_correct(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        wait_ms in 1u64..10_000,
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        // Drain as much as possible (may leave fractional remainder < 1.0)
        while b.try_acquire_one(0) {}
        let residual = b.available(0);

        let avail = b.available(wait_ms);
        let expected = (residual + rate * wait_ms as f64 / 1000.0).min(capacity);

        prop_assert!(
            (avail - expected).abs() < 0.01,
            "available {} != expected {} (rate={}, wait={}ms, residual={})",
            avail, expected, rate, wait_ms, residual
        );
    }

    /// Refill with time=0 or time going backward doesn't add tokens.
    #[test]
    fn prop_no_refill_on_same_or_earlier_time(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        cost in 1u32..5,
        start_time in 1000u64..5000,
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, start_time);
        b.try_acquire(cost, start_time);
        let avail_after_consume = b.available(start_time);

        // Call available with an earlier time — should not change tokens
        let avail_backward = b.available(start_time - 1);
        prop_assert!(
            (avail_backward - avail_after_consume).abs() < 1e-9,
            "Tokens changed on backward time"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Wait time correctness
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// wait_time_ms == 0 when enough tokens are available.
    #[test]
    fn prop_wait_zero_when_available(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        cost in arb_cost(),
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        if cost as f64 <= capacity {
            let wait = b.wait_time_ms(cost, 0);
            prop_assert_eq!(wait, 0, "Wait should be 0 when bucket has enough tokens");
        }
    }

    /// After waiting the reported time, the acquire should succeed.
    #[test]
    fn prop_acquire_succeeds_after_wait(
        capacity in 5.0f64..50.0,
        rate in 1.0f64..20.0,
        cost in 1u32..10,
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        // Drain
        while b.try_acquire_one(0) {}

        let cost = cost.min(capacity as u32);
        if cost == 0 { return Ok(()); }

        let wait = b.wait_time_ms(cost, 0);
        // After waiting, we should be able to acquire
        let success = b.try_acquire(cost, wait);
        prop_assert!(
            success,
            "Acquire failed after waiting {}ms for cost={} (rate={}, cap={})",
            wait, cost, rate, capacity
        );
    }

    /// Wait time is proportional to deficit: higher cost → longer or equal wait.
    #[test]
    fn prop_wait_time_monotonic_in_cost(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        cost1 in 1u32..10,
        cost2 in 10u32..20,
    ) {
        // Use with_time(cap, rate, 0) to start full, then drain to create empty state
        let mut b1 = TokenBucket::with_time(capacity, rate, 0);
        while b1.try_acquire_one(0) {}
        let mut b2 = b1.clone();

        let wait1 = b1.wait_time_ms(cost1, 0);
        let wait2 = b2.wait_time_ms(cost2, 0);

        prop_assert!(
            wait2 >= wait1,
            "Higher cost {} has shorter wait ({}ms) than lower cost {} ({}ms)",
            cost2, wait2, cost1, wait1
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Reset
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// reset() restores tokens to full capacity.
    #[test]
    fn prop_reset_restores_capacity(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        n_consume in 1u32..20,
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        for _ in 0..n_consume {
            b.try_acquire_one(0);
        }

        b.reset(0);
        let avail = b.available(0);
        prop_assert!(
            (avail - capacity).abs() < 1e-9,
            "After reset, available {} != capacity {}", avail, capacity
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Stats consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// fill_ratio is always in [0.0, 1.0].
    #[test]
    fn prop_fill_ratio_bounded(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        costs in prop::collection::vec(arb_cost(), 0..20),
        times in arb_time_sequence(20),
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        for (i, &cost) in costs.iter().enumerate() {
            let t = times.get(i).copied().unwrap_or(0);
            b.try_acquire(cost, t);
        }

        let stats = b.stats();
        prop_assert!(
            stats.fill_ratio >= -1e-9 && stats.fill_ratio <= 1.0 + 1e-9,
            "fill_ratio {} out of [0, 1]", stats.fill_ratio
        );
    }

    /// BucketStats JSON roundtrip preserves all fields.
    #[test]
    fn prop_bucket_stats_serde_roundtrip(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        n_ops in 0u32..10,
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        for _ in 0..n_ops {
            b.try_acquire_one(0);
        }

        let stats = b.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: BucketStats = serde_json::from_str(&json).unwrap();

        prop_assert!((stats.capacity - back.capacity).abs() < 1e-9);
        prop_assert!((stats.refill_rate - back.refill_rate).abs() < 1e-9);
        prop_assert!((stats.current_tokens - back.current_tokens).abs() < 1e-9);
        prop_assert_eq!(stats.total_consumed, back.total_consumed);
        prop_assert_eq!(stats.total_denied, back.total_denied);
        prop_assert!((stats.fill_ratio - back.fill_ratio).abs() < 1e-9);
    }

    /// BucketConfig JSON roundtrip preserves all fields.
    #[test]
    fn prop_bucket_config_serde_roundtrip(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        start_empty in any::<bool>(),
    ) {
        let config = BucketConfig {
            capacity,
            refill_rate: rate,
            start_empty,
        };

        let json = serde_json::to_string(&config).unwrap();
        let back: BucketConfig = serde_json::from_str(&json).unwrap();

        prop_assert!((config.capacity - back.capacity).abs() < 1e-9);
        prop_assert!((config.refill_rate - back.refill_rate).abs() < 1e-9);
        prop_assert_eq!(config.start_empty, back.start_empty);
    }
}

// ────────────────────────────────────────────────────────────────────
// Config build
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// BucketConfig::build creates bucket matching config parameters.
    #[test]
    fn prop_config_build_matches_params(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        start_empty in any::<bool>(),
        now_ms in arb_time_ms(),
    ) {
        let config = BucketConfig {
            capacity,
            refill_rate: rate,
            start_empty,
        };

        let mut b = config.build(now_ms);
        let avail = b.available(now_ms);

        prop_assert!((b.capacity() - capacity).abs() < 1e-9);
        prop_assert!((b.refill_rate() - rate).abs() < 1e-9);

        if start_empty {
            prop_assert!(
                avail < 1e-9,
                "start_empty bucket has {} tokens", avail
            );
        } else {
            prop_assert!(
                (avail - capacity).abs() < 1e-9,
                "start_full bucket has {} tokens, expected {}", avail, capacity
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Hierarchical bucket atomicity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// If global denies, local tokens are not consumed (atomicity).
    #[test]
    fn prop_hierarchical_atomic_on_global_deny(
        local_cap in arb_capacity(),
        local_rate in arb_refill_rate(),
        cost in arb_cost(),
    ) {
        let local = TokenBucket::with_time(local_cap, local_rate, 0);
        let global = TokenBucket::new_empty(100.0, 10.0); // global empty
        let mut hb = HierarchicalBucket::new(local, global);

        let result = hb.try_acquire(cost, 0);

        if !result.is_allowed() {
            prop_assert_eq!(
                hb.local().total_consumed(), 0,
                "Local consumed tokens despite global deny"
            );
        }
    }

    /// If local denies, global tokens are not consumed (atomicity).
    #[test]
    fn prop_hierarchical_atomic_on_local_deny(
        global_cap in arb_capacity(),
        global_rate in arb_refill_rate(),
        cost in arb_cost(),
    ) {
        let local = TokenBucket::new_empty(10.0, 1.0); // local empty
        let global = TokenBucket::with_time(global_cap, global_rate, 0);
        let mut hb = HierarchicalBucket::new(local, global);

        let result = hb.try_acquire(cost, 0);

        if !result.is_allowed() {
            prop_assert_eq!(
                hb.global().total_consumed(), 0,
                "Global consumed tokens despite local deny"
            );
        }
    }

    /// When both have tokens, hierarchical acquire succeeds and both consumed.
    #[test]
    fn prop_hierarchical_both_consumed_on_success(
        local_cap in 10.0f64..50.0,
        local_rate in arb_refill_rate(),
        global_cap in 50.0f64..200.0,
        global_rate in arb_refill_rate(),
        cost in 1u32..5,
    ) {
        let local = TokenBucket::with_time(local_cap, local_rate, 0);
        let global = TokenBucket::with_time(global_cap, global_rate, 0);
        let mut hb = HierarchicalBucket::new(local, global);

        let result = hb.try_acquire(cost, 0);

        if result.is_allowed() {
            prop_assert_eq!(
                hb.local().total_consumed(), cost as u64,
                "Local not consumed on success"
            );
            prop_assert_eq!(
                hb.global().total_consumed(), cost as u64,
                "Global not consumed on success"
            );
        }
    }

    /// Hierarchical result type matches which bucket is the bottleneck.
    #[test]
    fn prop_hierarchical_result_identifies_bottleneck(
        local_cap in arb_capacity(),
        local_rate in arb_refill_rate(),
        global_cap in arb_capacity(),
        global_rate in arb_refill_rate(),
        cost in arb_cost(),
    ) {
        let local = TokenBucket::with_time(local_cap, local_rate, 0);
        let global = TokenBucket::with_time(global_cap, global_rate, 0);
        let mut hb = HierarchicalBucket::new(local, global);

        let local_has = local_cap >= cost as f64;
        let global_has = global_cap >= cost as f64;

        let result = hb.try_acquire(cost, 0);

        match (local_has, global_has) {
            (true, true) => {
                prop_assert!(result.is_allowed(), "Both have tokens but denied");
            }
            (false, _) => {
                prop_assert!(
                    matches!(result, HierarchicalResult::DeniedLocal { .. }),
                    "Local lacks tokens but result is {:?}", result
                );
            }
            (true, false) => {
                prop_assert!(
                    matches!(result, HierarchicalResult::DeniedGlobal { .. }),
                    "Global lacks tokens but result is {:?}", result
                );
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Dynamic rate change
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After set_refill_rate, the new rate takes effect on next refill.
    #[test]
    fn prop_dynamic_rate_takes_effect(
        capacity in 10.0f64..100.0,
        old_rate in arb_refill_rate(),
        new_rate in arb_refill_rate(),
        wait_ms in 100u64..5000,
    ) {
        let mut b = TokenBucket::with_time(capacity, old_rate, 0);
        // Drain as much as possible (may leave fractional remainder)
        while b.try_acquire_one(0) {}
        let residual = b.available(0);

        b.set_refill_rate(new_rate);
        let avail = b.available(wait_ms);
        let expected = (residual + new_rate * wait_ms as f64 / 1000.0).min(capacity);

        prop_assert!(
            (avail - expected).abs() < 0.01,
            "After rate change, available {} != expected {} (new_rate={}, wait={}ms, residual={})",
            avail, expected, new_rate, wait_ms, residual
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// try_acquire_one equivalence
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// try_acquire_one(t) is equivalent to try_acquire(1, t).
    #[test]
    fn prop_acquire_one_eq_acquire_1(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        time in arb_time_ms(),
    ) {
        let mut b1 = TokenBucket::with_time(capacity, rate, 0);
        let mut b2 = b1.clone();

        let r1 = b1.try_acquire_one(time);
        let r2 = b2.try_acquire(1, time);

        prop_assert_eq!(r1, r2, "try_acquire_one != try_acquire(1, ...)");
        prop_assert_eq!(b1.total_consumed(), b2.total_consumed());
        prop_assert_eq!(b1.total_denied(), b2.total_denied());
    }
}

// ────────────────────────────────────────────────────────────────────
// Conservation: consumed + available ≤ capacity + refilled
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Token conservation: tokens are neither created nor destroyed
    /// beyond the refill mechanism.
    #[test]
    fn prop_token_conservation(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        ops in prop::collection::vec((arb_cost(), 0u64..2000), 1..20),
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        let initial_tokens = capacity;
        let mut total_time_ms = 0u64;

        for &(cost, delta_ms) in &ops {
            total_time_ms += delta_ms;
            b.try_acquire(cost, total_time_ms);
        }

        let final_avail = b.available(total_time_ms);
        let total_consumed = b.total_consumed() as f64;
        let total_refilled = rate * total_time_ms as f64 / 1000.0;

        // Conservation: final_avail ≤ initial_tokens + total_refilled - total_consumed
        // But tokens are capped, so we just verify:
        // final_avail + total_consumed ≤ initial_tokens + total_refilled + epsilon
        let sum = final_avail + total_consumed;
        let budget = initial_tokens + total_refilled;

        prop_assert!(
            sum <= budget + 0.01,
            "Conservation violated: avail({}) + consumed({}) = {} > budget({})",
            final_avail, total_consumed, sum, budget
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Non-negative tokens
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Tokens are never negative after any sequence of operations.
    #[test]
    fn prop_tokens_non_negative(
        capacity in arb_capacity(),
        rate in arb_refill_rate(),
        ops in prop::collection::vec((arb_cost(), 0u64..1000), 1..30),
    ) {
        let mut b = TokenBucket::with_time(capacity, rate, 0);
        let mut t = 0u64;

        for &(cost, dt) in &ops {
            t += dt;
            b.try_acquire(cost, t);
            let avail = b.available(t);
            prop_assert!(
                avail >= -1e-9,
                "Negative tokens: {} at t={}", avail, t
            );
        }
    }
}
