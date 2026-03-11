//! Property-based tests for the vendored mux_pool module.
//!
//! Tests serde roundtrips for MuxPoolStats and behavioral invariants
//! for counter relationships (recovery successes <= attempts, health
//! check failures <= checks, embedded PoolStats consistency).

use frankenterm_core::pool::PoolStats;
use frankenterm_core::vendored::mux_pool::MuxPoolStats;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_pool_stats() -> impl Strategy<Value = PoolStats> {
    (
        1_usize..=256,
        0_usize..=256,
        0_usize..=256,
        0_u64..=10_000,
        0_u64..=10_000,
        0_u64..=10_000,
        0_u64..=10_000,
    )
        .prop_map(
            |(max_size, idle_count, active_count, total_acquired, total_returned, total_evicted, total_timeouts)| {
                let idle = idle_count.min(max_size);
                let active = active_count.min(max_size.saturating_sub(idle));
                PoolStats {
                    max_size,
                    idle_count: idle,
                    active_count: active,
                    total_acquired,
                    total_returned: total_returned.min(total_acquired),
                    total_evicted,
                    total_timeouts,
                }
            },
        )
}

fn arb_mux_pool_stats() -> impl Strategy<Value = MuxPoolStats> {
    (
        arb_pool_stats(),
        0_u64..=10_000,
        0_u64..=10_000,
        0_u64..=10_000,
        0_u64..=10_000,
        0_u64..=10_000,
        0_u64..=10_000,
        0_u64..=10_000,
    )
        .prop_map(
            |(
                pool,
                connections_created,
                connections_failed,
                health_checks,
                health_check_failures,
                recovery_attempts,
                recovery_successes,
                permanent_failures,
            )| {
                MuxPoolStats {
                    pool,
                    connections_created,
                    connections_failed,
                    health_checks,
                    health_check_failures: health_check_failures.min(health_checks),
                    recovery_attempts,
                    recovery_successes: recovery_successes.min(recovery_attempts),
                    permanent_failures,
                }
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mux_pool_stats_json_roundtrip(stats in arb_mux_pool_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let back: MuxPoolStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pool, stats.pool);
        prop_assert_eq!(back.connections_created, stats.connections_created);
        prop_assert_eq!(back.connections_failed, stats.connections_failed);
        prop_assert_eq!(back.health_checks, stats.health_checks);
        prop_assert_eq!(back.health_check_failures, stats.health_check_failures);
        prop_assert_eq!(back.recovery_attempts, stats.recovery_attempts);
        prop_assert_eq!(back.recovery_successes, stats.recovery_successes);
        prop_assert_eq!(back.permanent_failures, stats.permanent_failures);
    }

    #[test]
    fn mux_pool_stats_pretty_json_roundtrip(stats in arb_mux_pool_stats()) {
        let json = serde_json::to_string_pretty(&stats).unwrap();
        let back: MuxPoolStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pool, stats.pool);
        prop_assert_eq!(back.connections_created, stats.connections_created);
        prop_assert_eq!(back.recovery_attempts, stats.recovery_attempts);
    }

    // =========================================================================
    // Behavioral invariants
    // =========================================================================

    #[test]
    fn recovery_successes_leq_attempts(stats in arb_mux_pool_stats()) {
        prop_assert!(
            stats.recovery_successes <= stats.recovery_attempts,
            "recovery_successes ({}) must not exceed recovery_attempts ({})",
            stats.recovery_successes, stats.recovery_attempts
        );
    }

    #[test]
    fn health_check_failures_leq_checks(stats in arb_mux_pool_stats()) {
        prop_assert!(
            stats.health_check_failures <= stats.health_checks,
            "health_check_failures ({}) must not exceed health_checks ({})",
            stats.health_check_failures, stats.health_checks
        );
    }

    #[test]
    fn embedded_pool_idle_active_within_max(stats in arb_mux_pool_stats()) {
        prop_assert!(
            stats.pool.idle_count + stats.pool.active_count <= stats.pool.max_size,
            "embedded pool: idle ({}) + active ({}) must not exceed max_size ({})",
            stats.pool.idle_count, stats.pool.active_count, stats.pool.max_size
        );
    }

    #[test]
    fn embedded_pool_returned_leq_acquired(stats in arb_mux_pool_stats()) {
        prop_assert!(
            stats.pool.total_returned <= stats.pool.total_acquired,
            "embedded pool: returned ({}) must not exceed acquired ({})",
            stats.pool.total_returned, stats.pool.total_acquired
        );
    }

    #[test]
    fn json_field_completeness(stats in arb_mux_pool_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = val.as_object().unwrap();
        prop_assert!(obj.contains_key("pool"), "missing 'pool' field");
        prop_assert!(obj.contains_key("connections_created"), "missing 'connections_created'");
        prop_assert!(obj.contains_key("connections_failed"), "missing 'connections_failed'");
        prop_assert!(obj.contains_key("health_checks"), "missing 'health_checks'");
        prop_assert!(obj.contains_key("health_check_failures"), "missing 'health_check_failures'");
        prop_assert!(obj.contains_key("recovery_attempts"), "missing 'recovery_attempts'");
        prop_assert!(obj.contains_key("recovery_successes"), "missing 'recovery_successes'");
        prop_assert!(obj.contains_key("permanent_failures"), "missing 'permanent_failures'");
    }

    #[test]
    fn json_nested_pool_has_expected_fields(stats in arb_mux_pool_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pool_obj = val.get("pool").unwrap().as_object().unwrap();
        prop_assert!(pool_obj.contains_key("max_size"), "missing pool.max_size");
        prop_assert!(pool_obj.contains_key("idle_count"), "missing pool.idle_count");
        prop_assert!(pool_obj.contains_key("active_count"), "missing pool.active_count");
        prop_assert!(pool_obj.contains_key("total_acquired"), "missing pool.total_acquired");
        prop_assert!(pool_obj.contains_key("total_returned"), "missing pool.total_returned");
        prop_assert!(pool_obj.contains_key("total_evicted"), "missing pool.total_evicted");
        prop_assert!(pool_obj.contains_key("total_timeouts"), "missing pool.total_timeouts");
    }

    #[test]
    fn zero_counters_valid(_dummy in 0..1_u8) {
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 1,
                idle_count: 0,
                active_count: 0,
                total_acquired: 0,
                total_returned: 0,
                total_evicted: 0,
                total_timeouts: 0,
            },
            connections_created: 0,
            connections_failed: 0,
            health_checks: 0,
            health_check_failures: 0,
            recovery_attempts: 0,
            recovery_successes: 0,
            permanent_failures: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: MuxPoolStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pool, stats.pool);
        prop_assert_eq!(back.connections_created, 0);
        prop_assert_eq!(back.recovery_attempts, 0);
    }

    #[test]
    fn max_u64_counters_roundtrip(max_val in prop::num::u64::ANY) {
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 1,
                idle_count: 0,
                active_count: 0,
                total_acquired: max_val,
                total_returned: 0,
                total_evicted: 0,
                total_timeouts: 0,
            },
            connections_created: max_val,
            connections_failed: max_val,
            health_checks: max_val,
            health_check_failures: 0,
            recovery_attempts: max_val,
            recovery_successes: 0,
            permanent_failures: max_val,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: MuxPoolStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.connections_created, max_val);
        prop_assert_eq!(back.recovery_attempts, max_val);
        prop_assert_eq!(back.permanent_failures, max_val);
    }
}
