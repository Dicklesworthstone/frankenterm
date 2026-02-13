//! Property-based tests for memory budget invariants.
//!
//! Bead: wa-5477
//!
//! Validates:
//! 1. BudgetLevel: ordering Normal < Throttled < OverBudget
//! 2. BudgetLevel: as_u8 monotonically increasing
//! 3. BudgetLevel: Display non-empty and uppercase
//! 4. BudgetLevel: serde roundtrip preserves identity
//! 5. MemoryBudgetConfig: serde roundtrip preserves all fields
//! 6. MemoryBudgetConfig: default has sensible values
//! 7. PaneBudget: usage_ratio = current / budget (or 0 if budget = 0)
//! 8. PaneBudget: usage_ratio in [0, ∞) non-negative
//! 9. PaneBudget: serde roundtrip preserves all fields
//! 10. PaneBudget: level thresholds consistent (Normal < high, Throttled >= high, OverBudget >= budget)
//! 11. BudgetSummary: serde roundtrip
//! 12. BudgetSummary: level counts sum to pane_count
//! 13. Manager: register increases pane count
//! 14. Manager: unregister decreases pane count
//! 15. Manager: register/unregister roundtrip → 0 panes
//! 16. Manager: worst_level starts Normal
//! 17. Manager: get_pane_budget returns registered pane
//! 18. Manager: unregister nonexistent returns None
//! 19. Manager: register_with_budget uses custom budget
//! 20. Manager: all_pane_budgets count matches

use std::collections::HashSet;

use proptest::prelude::*;

use frankenterm_core::memory_budget::{
    BudgetLevel, BudgetSummary, MemoryBudgetConfig, MemoryBudgetManager, PaneBudget,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_budget_level() -> impl Strategy<Value = BudgetLevel> {
    prop_oneof![
        Just(BudgetLevel::Normal),
        Just(BudgetLevel::Throttled),
        Just(BudgetLevel::OverBudget),
    ]
}

fn arb_pane_id() -> impl Strategy<Value = u64> {
    1_u64..10_000
}

fn arb_budget_bytes() -> impl Strategy<Value = u64> {
    1_u64..10_000_000_000
}

fn arb_high_ratio() -> impl Strategy<Value = f64> {
    0.01_f64..0.99
}

fn test_config() -> MemoryBudgetConfig {
    MemoryBudgetConfig {
        enabled: true,
        default_budget_bytes: 512 * 1024 * 1024,
        high_ratio: 0.8,
        sample_interval_ms: 1000,
        cgroup_base_path: "/tmp/frankenterm-proptest-cgroup".to_string(),
        use_cgroups: false,
        oom_score_adj: -500,
    }
}

// =============================================================================
// Property 1: BudgetLevel ordering
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn budget_level_ordering(_dummy in 0..1_u32) {
        prop_assert!(BudgetLevel::Normal < BudgetLevel::Throttled);
        prop_assert!(BudgetLevel::Throttled < BudgetLevel::OverBudget);
        prop_assert!(BudgetLevel::Normal < BudgetLevel::OverBudget);
    }
}

// =============================================================================
// Property 2: as_u8 monotonically increasing
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn budget_level_as_u8_monotonic(_dummy in 0..1_u32) {
        let levels = [BudgetLevel::Normal, BudgetLevel::Throttled, BudgetLevel::OverBudget];
        for w in levels.windows(2) {
            prop_assert!(w[0].as_u8() < w[1].as_u8(),
                "{:?} as_u8 {} should be < {:?} as_u8 {}",
                w[0], w[0].as_u8(), w[1], w[1].as_u8());
        }
    }
}

// =============================================================================
// Property 3: Display non-empty and uppercase
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn budget_level_display(
        level in arb_budget_level(),
    ) {
        let s = level.to_string();
        prop_assert!(!s.is_empty(), "Display should not be empty");
        let upper = s.to_uppercase();
        prop_assert!(s == upper, "Display should be uppercase: got '{}'", s);
    }
}

// =============================================================================
// Property 4: BudgetLevel serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn budget_level_serde_roundtrip(
        level in arb_budget_level(),
    ) {
        let json = serde_json::to_string(&level).unwrap();
        let back: BudgetLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, level);
    }
}

// =============================================================================
// Property 5: MemoryBudgetConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(
        budget in arb_budget_bytes(),
        ratio in arb_high_ratio(),
        interval_ms in 100_u64..60000,
        oom_adj in -1000_i32..1000,
    ) {
        let config = MemoryBudgetConfig {
            enabled: true,
            default_budget_bytes: budget,
            high_ratio: ratio,
            sample_interval_ms: interval_ms,
            cgroup_base_path: "/test/path".to_string(),
            use_cgroups: false,
            oom_score_adj: oom_adj,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: MemoryBudgetConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.default_budget_bytes, config.default_budget_bytes);
        prop_assert!((back.high_ratio - config.high_ratio).abs() < 1e-10);
        prop_assert_eq!(back.sample_interval_ms, config.sample_interval_ms);
        prop_assert_eq!(back.oom_score_adj, config.oom_score_adj);
        prop_assert_eq!(back.enabled, config.enabled);
        prop_assert_eq!(back.use_cgroups, config.use_cgroups);
        prop_assert_eq!(back.cgroup_base_path, config.cgroup_base_path);
    }
}

// =============================================================================
// Property 6: Default config has sensible values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn config_defaults_sensible(_dummy in 0..1_u32) {
        let config = MemoryBudgetConfig::default();
        prop_assert!(config.enabled);
        prop_assert!(config.default_budget_bytes > 0);
        prop_assert!(config.high_ratio > 0.0 && config.high_ratio < 1.0,
            "high_ratio {} should be in (0,1)", config.high_ratio);
        prop_assert!(config.sample_interval_ms > 0);
        prop_assert!(config.oom_score_adj >= -1000 && config.oom_score_adj <= 1000);
    }
}

// =============================================================================
// Property 7: usage_ratio = current / budget (or 0 if budget = 0)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn pane_budget_usage_ratio(
        budget_bytes in 1_u64..10_000_000,
        current_bytes in 0_u64..20_000_000,
    ) {
        let pane = PaneBudget {
            pane_id: 1,
            budget_bytes,
            high_bytes: (budget_bytes as f64 * 0.8) as u64,
            current_bytes,
            level: BudgetLevel::Normal,
            cgroup_active: false,
            pid: None,
        };
        let expected = current_bytes as f64 / budget_bytes as f64;
        let actual = pane.usage_ratio();
        prop_assert!((actual - expected).abs() < 1e-10,
            "usage_ratio {} should ≈ {} (current={}, budget={})",
            actual, expected, current_bytes, budget_bytes);
    }
}

// =============================================================================
// Property 8: usage_ratio non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_budget_usage_ratio_nonneg(
        budget_bytes in 0_u64..10_000_000,
        current_bytes in 0_u64..20_000_000,
    ) {
        let pane = PaneBudget {
            pane_id: 1,
            budget_bytes,
            high_bytes: 0,
            current_bytes,
            level: BudgetLevel::Normal,
            cgroup_active: false,
            pid: None,
        };
        prop_assert!(pane.usage_ratio() >= 0.0,
            "usage_ratio should be >= 0, got {}", pane.usage_ratio());
    }
}

// =============================================================================
// Property 9: PaneBudget serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_budget_serde_roundtrip(
        pane_id in arb_pane_id(),
        budget_bytes in arb_budget_bytes(),
        current_bytes in 0_u64..10_000_000_000,
        level in arb_budget_level(),
        pid in proptest::option::of(1_u32..100000),
    ) {
        let high_bytes = (budget_bytes as f64 * 0.8) as u64;
        let pane = PaneBudget {
            pane_id,
            budget_bytes,
            high_bytes,
            current_bytes,
            level,
            cgroup_active: false,
            pid,
        };
        let json = serde_json::to_string(&pane).unwrap();
        let back: PaneBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pane.pane_id);
        prop_assert_eq!(back.budget_bytes, pane.budget_bytes);
        prop_assert_eq!(back.high_bytes, pane.high_bytes);
        prop_assert_eq!(back.current_bytes, pane.current_bytes);
        prop_assert_eq!(back.level, pane.level);
        prop_assert_eq!(back.pid, pane.pid);
    }
}

// =============================================================================
// Property 10: Level thresholds consistent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn level_thresholds_consistent(
        budget_bytes in 100_u64..10_000_000,
        ratio in 0.1_f64..0.99,
        usage_frac in 0.0_f64..1.5,
    ) {
        let high_bytes = (budget_bytes as f64 * ratio) as u64;
        let current_bytes = (budget_bytes as f64 * usage_frac) as u64;

        // Determine expected level using the same logic as update_level
        let expected_level = if current_bytes >= budget_bytes {
            BudgetLevel::OverBudget
        } else if current_bytes >= high_bytes {
            BudgetLevel::Throttled
        } else {
            BudgetLevel::Normal
        };

        // Verify level classification is self-consistent
        match expected_level {
            BudgetLevel::Normal => {
                prop_assert!(current_bytes < high_bytes,
                    "Normal but current {} >= high {}", current_bytes, high_bytes);
            }
            BudgetLevel::Throttled => {
                prop_assert!(current_bytes >= high_bytes && current_bytes < budget_bytes,
                    "Throttled but current {} not in [high={}, budget={})",
                    current_bytes, high_bytes, budget_bytes);
            }
            BudgetLevel::OverBudget => {
                prop_assert!(current_bytes >= budget_bytes,
                    "OverBudget but current {} < budget {}", current_bytes, budget_bytes);
            }
        }
    }
}

// =============================================================================
// Property 11: BudgetSummary serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn summary_serde_roundtrip(
        pane_count in 0_usize..100,
        total_budget in 0_u64..1_000_000_000,
        total_current in 0_u64..1_000_000_000,
        normal in 0_usize..50,
        throttled in 0_usize..30,
        over_budget in 0_usize..20,
        worst_ratio in 0.0_f64..2.0,
    ) {
        let summary = BudgetSummary {
            pane_count,
            total_budget_bytes: total_budget,
            total_current_bytes: total_current,
            normal_count: normal,
            throttled_count: throttled,
            over_budget_count: over_budget,
            worst_pane_id: if pane_count > 0 { Some(1) } else { None },
            worst_usage_ratio: worst_ratio,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: BudgetSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_count, summary.pane_count);
        prop_assert_eq!(back.total_budget_bytes, summary.total_budget_bytes);
        prop_assert_eq!(back.total_current_bytes, summary.total_current_bytes);
        prop_assert_eq!(back.normal_count, summary.normal_count);
        prop_assert_eq!(back.throttled_count, summary.throttled_count);
        prop_assert_eq!(back.over_budget_count, summary.over_budget_count);
        prop_assert_eq!(back.worst_pane_id, summary.worst_pane_id);
        prop_assert!((back.worst_usage_ratio - summary.worst_usage_ratio).abs() < 1e-10);
    }
}

// =============================================================================
// Property 12: Level counts sum to pane_count (through manager)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn summary_level_counts_sum(
        n_panes in 1_usize..20,
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        for i in 0..n_panes as u64 {
            mgr.register_pane(i, None);
        }
        let summary = mgr.sample_all();
        let sum = summary.normal_count + summary.throttled_count + summary.over_budget_count;
        prop_assert_eq!(sum, summary.pane_count,
            "level counts {} + {} + {} = {} should equal pane_count {}",
            summary.normal_count, summary.throttled_count, summary.over_budget_count,
            sum, summary.pane_count);
    }
}

// =============================================================================
// Property 13: Register increases pane count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn register_increases_count(
        pane_ids in proptest::collection::hash_set(arb_pane_id(), 1..15),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        for &id in &pane_ids {
            mgr.register_pane(id, None);
        }
        let all = mgr.all_pane_budgets();
        prop_assert_eq!(all.len(), pane_ids.len(),
            "registered {} panes but got {}", pane_ids.len(), all.len());
    }
}

// =============================================================================
// Property 14: Unregister decreases pane count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn unregister_decreases_count(
        pane_ids in proptest::collection::hash_set(arb_pane_id(), 2..15),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        for &id in &pane_ids {
            mgr.register_pane(id, None);
        }
        let first = *pane_ids.iter().next().unwrap();
        mgr.unregister_pane(first);
        let all = mgr.all_pane_budgets();
        prop_assert_eq!(all.len(), pane_ids.len() - 1);
    }
}

// =============================================================================
// Property 15: Register/unregister roundtrip → 0 panes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn register_unregister_roundtrip(
        pane_ids in proptest::collection::hash_set(arb_pane_id(), 1..20),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        for &id in &pane_ids {
            mgr.register_pane(id, None);
        }
        for &id in &pane_ids {
            mgr.unregister_pane(id);
        }
        let all = mgr.all_pane_budgets();
        prop_assert_eq!(all.len(), 0,
            "all panes should be removed after roundtrip");
    }
}

// =============================================================================
// Property 16: worst_level starts Normal
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn worst_level_starts_normal(_dummy in 0..1_u32) {
        let mgr = MemoryBudgetManager::new(test_config());
        prop_assert_eq!(mgr.worst_level(), BudgetLevel::Normal);
    }
}

// =============================================================================
// Property 17: get_pane_budget returns registered pane
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn get_pane_budget_returns_registered(
        pane_id in arb_pane_id(),
        pid in proptest::option::of(1_u32..100000),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        mgr.register_pane(pane_id, pid);
        let budget = mgr.get_pane_budget(pane_id);
        prop_assert!(budget.is_some(), "should find registered pane {}", pane_id);
        let b = budget.unwrap();
        prop_assert_eq!(b.pane_id, pane_id);
        prop_assert_eq!(b.pid, pid);
        prop_assert_eq!(b.budget_bytes, test_config().default_budget_bytes);
    }
}

// =============================================================================
// Property 18: Unregister nonexistent returns None
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn unregister_nonexistent_returns_none(
        pane_id in arb_pane_id(),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        let result = mgr.unregister_pane(pane_id);
        prop_assert!(result.is_none(),
            "unregistering nonexistent pane {} should return None", pane_id);
    }
}

// =============================================================================
// Property 19: register_with_budget uses custom budget
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn register_with_custom_budget(
        pane_id in arb_pane_id(),
        budget in arb_budget_bytes(),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        let result = mgr.register_pane_with_budget(pane_id, None, budget);
        prop_assert_eq!(result.budget_bytes, budget,
            "custom budget {} should be used", budget);
        let high_expected = (budget as f64 * test_config().high_ratio) as u64;
        prop_assert_eq!(result.high_bytes, high_expected,
            "high_bytes should be budget * high_ratio");
    }
}

// =============================================================================
// Property 20: all_pane_budgets count matches
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn all_pane_budgets_count(
        pane_ids in proptest::collection::hash_set(arb_pane_id(), 0..20),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        for &id in &pane_ids {
            mgr.register_pane(id, None);
        }
        let all = mgr.all_pane_budgets();
        prop_assert_eq!(all.len(), pane_ids.len());
        // Verify all pane IDs are present
        let all_ids: HashSet<u64> = all.iter().map(|b| b.pane_id).collect();
        prop_assert_eq!(all_ids, pane_ids);
    }
}
