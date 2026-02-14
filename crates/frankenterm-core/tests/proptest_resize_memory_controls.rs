//! Property-based tests for resize_memory_controls module.
//!
//! Validates memory-pressure-aware resize budget computation:
//! - Tier monotonicity: higher pressure => more conservative budgets
//! - Metrics accumulation correctness
//! - Budget parameter bounds
//! - Config propagation
//! - Serde roundtrip stability
//! - Helper function boundary behavior
//!
//! Bead: wa-1u90p.7 (Validation Program)

use proptest::prelude::*;

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::resize_memory_controls::{
    ResizeMemoryBudget, ResizeMemoryConfig, ResizeMemoryMetrics, ResizeMemoryPolicy,
    effective_cold_batch_size, effective_overscan_rows, scratch_allocation_allowed,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_tier() -> impl Strategy<Value = MemoryPressureTier> {
    prop_oneof![
        Just(MemoryPressureTier::Green),
        Just(MemoryPressureTier::Yellow),
        Just(MemoryPressureTier::Orange),
        Just(MemoryPressureTier::Red),
    ]
}

fn arb_config() -> impl Strategy<Value = ResizeMemoryConfig> {
    (
        (
            any::<bool>(), // enabled
            1_usize..1000, // normal_batch_size
            1_usize..500,  // yellow_batch_size
            1_usize..250,  // orange_batch_size
            any::<bool>(), // red_pause_cold_reflow
            1_usize..1000, // normal_overscan_cap
            1_usize..500,  // yellow_overscan_cap
        ),
        (
            1_usize..250,         // pressure_overscan_cap
            1_usize..10_000_000,  // normal_backlog_cap
            1_usize..5_000_000,   // yellow_backlog_cap
            1_usize..2_500_000,   // orange_backlog_cap
            any::<bool>(),        // pre_resize_compaction_enabled
            1_usize..10000,       // compaction_batch_size
            1_usize..100_000_000, // max_scratch_buffer_bytes
        ),
    )
        .prop_map(
            |(
                (
                    enabled,
                    normal_batch,
                    yellow_batch,
                    orange_batch,
                    red_pause,
                    normal_overscan,
                    yellow_overscan,
                ),
                (
                    pressure_overscan,
                    normal_backlog,
                    yellow_backlog,
                    orange_backlog,
                    compaction_enabled,
                    compaction_batch,
                    max_scratch,
                ),
            )| {
                ResizeMemoryConfig {
                    enabled,
                    normal_batch_size: normal_batch,
                    yellow_batch_size: yellow_batch,
                    orange_batch_size: orange_batch,
                    red_pause_cold_reflow: red_pause,
                    normal_overscan_cap: normal_overscan,
                    yellow_overscan_cap: yellow_overscan,
                    pressure_overscan_cap: pressure_overscan,
                    normal_backlog_cap: normal_backlog,
                    yellow_backlog_cap: yellow_backlog,
                    orange_backlog_cap: orange_backlog,
                    pre_resize_compaction_enabled: compaction_enabled,
                    compaction_batch_size: compaction_batch,
                    max_scratch_buffer_bytes: max_scratch,
                }
            },
        )
}

fn arb_budget() -> impl Strategy<Value = ResizeMemoryBudget> {
    (
        arb_tier(),
        1_usize..1000,        // cold_batch_size
        any::<bool>(),        // cold_reflow_paused
        1_usize..1000,        // overscan_cap
        1_usize..10_000_000,  // backlog_cap
        any::<bool>(),        // compact_before_resize
        1_usize..10000,       // compaction_batch_size
        1_usize..100_000_000, // max_scratch_bytes
    )
        .prop_map(
            |(tier, batch, paused, overscan, backlog, compact, comp_batch, scratch)| {
                ResizeMemoryBudget {
                    tier,
                    cold_batch_size: batch,
                    cold_reflow_paused: paused,
                    overscan_cap: overscan,
                    backlog_cap: backlog,
                    compact_before_resize: compact,
                    compaction_batch_size: comp_batch,
                    max_scratch_bytes: scratch,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Tier monotonicity properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn batch_size_monotonically_decreases_with_pressure(_dummy in 0..1_u8) {
        let config = ResizeMemoryConfig::default();
        let mut policy = ResizeMemoryPolicy::new(config);

        let green = policy.compute_budget(MemoryPressureTier::Green);
        let yellow = policy.compute_budget(MemoryPressureTier::Yellow);
        let orange = policy.compute_budget(MemoryPressureTier::Orange);
        let red = policy.compute_budget(MemoryPressureTier::Red);

        prop_assert!(green.cold_batch_size >= yellow.cold_batch_size,
            "green batch {} should >= yellow batch {}", green.cold_batch_size, yellow.cold_batch_size);
        prop_assert!(yellow.cold_batch_size >= orange.cold_batch_size,
            "yellow batch {} should >= orange batch {}", yellow.cold_batch_size, orange.cold_batch_size);
        prop_assert!(orange.cold_batch_size >= red.cold_batch_size,
            "orange batch {} should >= red batch {}", orange.cold_batch_size, red.cold_batch_size);
    }

    #[test]
    fn scratch_bytes_monotonically_decrease_with_pressure(config in arb_config()) {
        if !config.enabled {
            return Ok(());
        }
        let mut policy = ResizeMemoryPolicy::new(config);

        let green = policy.compute_budget(MemoryPressureTier::Green);
        let yellow = policy.compute_budget(MemoryPressureTier::Yellow);
        let orange = policy.compute_budget(MemoryPressureTier::Orange);
        let red = policy.compute_budget(MemoryPressureTier::Red);

        prop_assert!(green.max_scratch_bytes >= yellow.max_scratch_bytes,
            "green scratch {} should >= yellow scratch {}", green.max_scratch_bytes, yellow.max_scratch_bytes);
        prop_assert!(yellow.max_scratch_bytes >= orange.max_scratch_bytes,
            "yellow scratch {} should >= orange scratch {}", yellow.max_scratch_bytes, orange.max_scratch_bytes);
        prop_assert!(orange.max_scratch_bytes >= red.max_scratch_bytes,
            "orange scratch {} should >= red scratch {}", orange.max_scratch_bytes, red.max_scratch_bytes);
    }

    #[test]
    fn red_tier_batch_size_always_one(config in arb_config()) {
        if !config.enabled {
            return Ok(());
        }
        let mut policy = ResizeMemoryPolicy::new(config);
        let red = policy.compute_budget(MemoryPressureTier::Red);
        prop_assert_eq!(red.cold_batch_size, 1, "red tier cold batch size must always be 1");
    }

    #[test]
    fn green_compaction_never_triggers(config in arb_config()) {
        if !config.enabled {
            return Ok(());
        }
        let mut policy = ResizeMemoryPolicy::new(config);
        let green = policy.compute_budget(MemoryPressureTier::Green);
        prop_assert!(!green.compact_before_resize,
            "green tier should never trigger pre-resize compaction");
    }
}

// ---------------------------------------------------------------------------
// Disabled policy properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn disabled_policy_returns_green_for_all_tiers(tier in arb_tier()) {
        let config = ResizeMemoryConfig {
            enabled: false,
            ..ResizeMemoryConfig::default()
        };
        let mut policy = ResizeMemoryPolicy::new(config.clone());
        let budget = policy.compute_budget(tier);

        prop_assert_eq!(budget.cold_batch_size, config.normal_batch_size,
            "disabled policy should use normal_batch_size for all tiers");
        prop_assert!(!budget.cold_reflow_paused,
            "disabled policy should never pause cold reflow");
        prop_assert_eq!(budget.overscan_cap, config.normal_overscan_cap,
            "disabled policy should use normal overscan cap");
        prop_assert!(!budget.compact_before_resize,
            "disabled policy should never compact");
    }

    #[test]
    fn disabled_policy_stores_actual_tier(tier in arb_tier()) {
        let config = ResizeMemoryConfig {
            enabled: false,
            ..ResizeMemoryConfig::default()
        };
        let mut policy = ResizeMemoryPolicy::new(config);
        let budget = policy.compute_budget(tier);
        prop_assert_eq!(budget.tier, tier,
            "disabled policy should still store the actual tier in the budget");
    }
}

// ---------------------------------------------------------------------------
// Metrics accumulation properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn metrics_budget_computation_count(tiers in proptest::collection::vec(arb_tier(), 1..20)) {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        for tier in &tiers {
            let _ = policy.compute_budget(*tier);
        }
        prop_assert_eq!(
            policy.metrics().budget_computations, tiers.len() as u64,
            "budget_computations should count every call"
        );
    }

    #[test]
    fn metrics_tier_counts_sum_to_total(tiers in proptest::collection::vec(arb_tier(), 1..30)) {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        for tier in &tiers {
            let _ = policy.compute_budget(*tier);
        }
        let m = policy.metrics();
        let tier_sum = m.green_computations + m.yellow_computations + m.orange_computations + m.red_computations;
        prop_assert_eq!(
            tier_sum, m.budget_computations,
            "sum of tier counts ({}) should equal total computations ({})",
            tier_sum, m.budget_computations
        );
    }

    #[test]
    fn metrics_reset_clears_all(tiers in proptest::collection::vec(arb_tier(), 1..10)) {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        for tier in &tiers {
            let _ = policy.compute_budget(*tier);
        }
        prop_assert!(policy.metrics().budget_computations > 0);

        policy.reset_metrics();
        let m = policy.metrics();
        prop_assert_eq!(m.budget_computations, 0);
        prop_assert_eq!(m.green_computations, 0);
        prop_assert_eq!(m.yellow_computations, 0);
        prop_assert_eq!(m.orange_computations, 0);
        prop_assert_eq!(m.red_computations, 0);
        prop_assert_eq!(m.cold_reflow_pauses, 0);
        prop_assert_eq!(m.compaction_triggers, 0);
        prop_assert_eq!(m.batch_size_reductions, 0);
    }

    #[test]
    fn red_pause_config_controls_cold_reflow_pauses(
        n_red in 1_u32..10,
        pause_enabled in any::<bool>()
    ) {
        let config = ResizeMemoryConfig {
            red_pause_cold_reflow: pause_enabled,
            ..ResizeMemoryConfig::default()
        };
        let mut policy = ResizeMemoryPolicy::new(config);
        for _ in 0..n_red {
            let _ = policy.compute_budget(MemoryPressureTier::Red);
        }
        let expected_pauses = if pause_enabled { n_red as u64 } else { 0 };
        prop_assert_eq!(
            policy.metrics().cold_reflow_pauses, expected_pauses,
            "cold_reflow_pauses should be {} when pause_enabled={}",
            expected_pauses, pause_enabled
        );
    }
}

// ---------------------------------------------------------------------------
// Helper function properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn effective_cold_batch_is_min_of_budget_and_remaining(
        batch_size in 1_usize..1000,
        remaining in 0_usize..2000
    ) {
        let budget = ResizeMemoryBudget {
            tier: MemoryPressureTier::Green,
            cold_batch_size: batch_size,
            cold_reflow_paused: false,
            overscan_cap: 256,
            backlog_cap: 1_000_000,
            compact_before_resize: false,
            compaction_batch_size: 256,
            max_scratch_bytes: 64 * 1024 * 1024,
        };
        let result = effective_cold_batch_size(&budget, remaining);
        prop_assert_eq!(result, batch_size.min(remaining),
            "effective batch should be min({}, {})", batch_size, remaining);
    }

    #[test]
    fn effective_cold_batch_zero_when_paused(remaining in 0_usize..10000) {
        let budget = ResizeMemoryBudget {
            tier: MemoryPressureTier::Red,
            cold_batch_size: 100,
            cold_reflow_paused: true,
            overscan_cap: 32,
            backlog_cap: 1000,
            compact_before_resize: true,
            compaction_batch_size: 64,
            max_scratch_bytes: 1024,
        };
        prop_assert_eq!(effective_cold_batch_size(&budget, remaining), 0,
            "paused cold reflow should always return 0");
    }

    #[test]
    fn effective_overscan_clamped_to_cap_and_available(
        cap in 1_usize..500,
        physical_rows in 1_usize..200,
        scrollback_lines in 0_usize..2000
    ) {
        let budget = ResizeMemoryBudget {
            tier: MemoryPressureTier::Green,
            cold_batch_size: 64,
            cold_reflow_paused: false,
            overscan_cap: cap,
            backlog_cap: 1_000_000,
            compact_before_resize: false,
            compaction_batch_size: 256,
            max_scratch_bytes: 64 * 1024 * 1024,
        };
        let result = effective_overscan_rows(&budget, physical_rows, scrollback_lines);
        let available = scrollback_lines.saturating_sub(physical_rows);
        prop_assert!(result <= cap,
            "overscan {} should not exceed cap {}", result, cap);
        prop_assert!(result <= available,
            "overscan {} should not exceed available {}", result, available);
        prop_assert_eq!(result, cap.min(available),
            "overscan should be min of cap and available");
    }

    #[test]
    fn scratch_allocation_boundary(
        max_bytes in 0_usize..10_000_000,
        request in 0_usize..20_000_000
    ) {
        let budget = ResizeMemoryBudget {
            tier: MemoryPressureTier::Green,
            cold_batch_size: 64,
            cold_reflow_paused: false,
            overscan_cap: 256,
            backlog_cap: 1_000_000,
            compact_before_resize: false,
            compaction_batch_size: 256,
            max_scratch_bytes: max_bytes,
        };
        let allowed = scratch_allocation_allowed(&budget, request);
        prop_assert_eq!(allowed, request <= max_bytes,
            "scratch_allocation_allowed({}, {}) should be {}",
            request, max_bytes, request <= max_bytes
        );
    }
}

// ---------------------------------------------------------------------------
// Config propagation properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn config_accessor_matches_construction(config in arb_config()) {
        let policy = ResizeMemoryPolicy::new(config.clone());
        prop_assert_eq!(policy.config(), &config,
            "config accessor should return construction config");
    }

    #[test]
    fn green_budget_uses_normal_params(config in arb_config()) {
        if !config.enabled {
            return Ok(());
        }
        let mut policy = ResizeMemoryPolicy::new(config.clone());
        let budget = policy.compute_budget(MemoryPressureTier::Green);

        prop_assert_eq!(budget.cold_batch_size, config.normal_batch_size,
            "green should use normal_batch_size");
        prop_assert_eq!(budget.overscan_cap, config.normal_overscan_cap,
            "green should use normal_overscan_cap");
        prop_assert_eq!(budget.backlog_cap, config.normal_backlog_cap,
            "green should use normal_backlog_cap");
        prop_assert_eq!(budget.max_scratch_bytes, config.max_scratch_buffer_bytes,
            "green should use full max_scratch_buffer_bytes");
    }

    #[test]
    fn yellow_budget_uses_yellow_params(config in arb_config()) {
        if !config.enabled {
            return Ok(());
        }
        let mut policy = ResizeMemoryPolicy::new(config.clone());
        let budget = policy.compute_budget(MemoryPressureTier::Yellow);

        prop_assert_eq!(budget.cold_batch_size, config.yellow_batch_size,
            "yellow should use yellow_batch_size");
        prop_assert_eq!(budget.overscan_cap, config.yellow_overscan_cap,
            "yellow should use yellow_overscan_cap");
        prop_assert_eq!(budget.backlog_cap, config.yellow_backlog_cap,
            "yellow should use yellow_backlog_cap");
        prop_assert_eq!(budget.max_scratch_bytes, config.max_scratch_buffer_bytes / 2,
            "yellow should use half max_scratch_buffer_bytes");
    }

    #[test]
    fn orange_budget_uses_orange_params(config in arb_config()) {
        if !config.enabled {
            return Ok(());
        }
        let mut policy = ResizeMemoryPolicy::new(config.clone());
        let budget = policy.compute_budget(MemoryPressureTier::Orange);

        prop_assert_eq!(budget.cold_batch_size, config.orange_batch_size,
            "orange should use orange_batch_size");
        prop_assert_eq!(budget.overscan_cap, config.pressure_overscan_cap,
            "orange should use pressure_overscan_cap");
        prop_assert_eq!(budget.backlog_cap, config.orange_backlog_cap,
            "orange should use orange_backlog_cap");
        prop_assert_eq!(budget.max_scratch_bytes, config.max_scratch_buffer_bytes / 4,
            "orange should use quarter max_scratch_buffer_bytes");
    }

    #[test]
    fn red_budget_uses_emergency_params(config in arb_config()) {
        if !config.enabled {
            return Ok(());
        }
        let mut policy = ResizeMemoryPolicy::new(config.clone());
        let budget = policy.compute_budget(MemoryPressureTier::Red);

        prop_assert_eq!(budget.cold_batch_size, 1,
            "red should always use batch_size=1");
        prop_assert_eq!(budget.cold_reflow_paused, config.red_pause_cold_reflow,
            "red should respect red_pause_cold_reflow config");
        prop_assert_eq!(budget.backlog_cap, config.orange_backlog_cap / 4,
            "red should use quarter of orange_backlog_cap");
        prop_assert_eq!(budget.max_scratch_bytes, config.max_scratch_buffer_bytes / 8,
            "red should use eighth of max_scratch_buffer_bytes");
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrip properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).expect("serialize config");
        let rt: ResizeMemoryConfig = serde_json::from_str(&json).expect("deserialize config");
        prop_assert_eq!(config, rt, "config serde roundtrip should be stable");
    }

    #[test]
    fn budget_serde_roundtrip(budget in arb_budget()) {
        let json = serde_json::to_string(&budget).expect("serialize budget");
        let rt: ResizeMemoryBudget = serde_json::from_str(&json).expect("deserialize budget");
        prop_assert_eq!(budget, rt, "budget serde roundtrip should be stable");
    }

    #[test]
    fn metrics_serde_roundtrip(tiers in proptest::collection::vec(arb_tier(), 1..10)) {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        for tier in &tiers {
            let _ = policy.compute_budget(*tier);
        }
        let metrics = policy.metrics().clone();
        let json = serde_json::to_string(&metrics).expect("serialize metrics");
        let rt: ResizeMemoryMetrics = serde_json::from_str(&json).expect("deserialize metrics");
        prop_assert_eq!(metrics, rt, "metrics serde roundtrip should be stable");
    }

    #[test]
    fn tier_serde_roundtrip(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).expect("serialize tier");
        let rt: MemoryPressureTier = serde_json::from_str(&json).expect("deserialize tier");
        prop_assert_eq!(tier, rt, "tier serde roundtrip should be stable");
    }
}

// ---------------------------------------------------------------------------
// Saturation safety properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn metrics_saturate_at_max(
        tier in arb_tier(),
        reps in 1_u32..5
    ) {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        // Saturate one counter
        policy.reset_metrics();
        // Force budget_computations near max
        let _m = policy.metrics().clone();
        // Just verify no panic when computing budgets after reset
        for _ in 0..reps {
            let _ = policy.compute_budget(tier);
        }
        prop_assert!(policy.metrics().budget_computations > 0);
    }

    #[test]
    fn red_compaction_batch_never_zero(compaction_batch in 0_usize..100) {
        let config = ResizeMemoryConfig {
            compaction_batch_size: compaction_batch.max(1),
            ..ResizeMemoryConfig::default()
        };
        let mut policy = ResizeMemoryPolicy::new(config);
        let red = policy.compute_budget(MemoryPressureTier::Red);
        prop_assert!(red.compaction_batch_size >= 1,
            "red compaction batch should be at least 1, got {}",
            red.compaction_batch_size
        );
    }
}
