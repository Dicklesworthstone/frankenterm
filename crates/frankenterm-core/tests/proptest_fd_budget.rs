//! Property-based tests for FD budget tracking invariants.
//!
//! Bead: wa-2paa
//!
//! Validates:
//! 1. Register increases allocation: each register adds fds_per_pane
//! 2. Unregister decreases allocation: each unregister subtracts fds_per_pane
//! 3. Register/unregister roundtrip: register N then unregister N → 0
//! 4. Admission threshold: refuse when projected usage >= refuse_threshold
//! 5. Warning threshold: warn when projected usage >= warn_threshold
//! 6. Snapshot consistency: snapshot.total_allocated == fds_per_pane * pane_count
//! 7. Budget ratio bounded: budget_ratio in [0, 1] when under limit
//! 8. Pane breakdown consistent: breakdown has correct pane count and values
//! 9. Config defaults sensible: warn < refuse, fds_per_pane > 0
//! 10. AdmitDecision is_allowed: Allowed and Warned are allowed, Refused is not

use proptest::prelude::*;

use frankenterm_core::fd_budget::{AdmitDecision, FdBudget, FdBudgetConfig};

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_id() -> impl Strategy<Value = u64> {
    1_u64..10_000
}

fn arb_fds_per_pane() -> impl Strategy<Value = u64> {
    1_u64..100
}

fn arb_limit() -> impl Strategy<Value = u64> {
    1000_u64..100_000
}

// =============================================================================
// Property: Register increases allocation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn register_increases_allocation(
        fds_per_pane in arb_fds_per_pane(),
        n in 1_usize..50,
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            ..FdBudgetConfig::default()
        };
        let budget = FdBudget::with_limit(config, 1_000_000);

        for i in 0..n as u64 {
            budget.register_pane(i);
        }

        let snap = budget.snapshot();
        prop_assert_eq!(snap.total_allocated, fds_per_pane * n as u64,
            "total should be {} * {} = {}", fds_per_pane, n, fds_per_pane * n as u64);
        prop_assert_eq!(snap.pane_count, n);
    }
}

// =============================================================================
// Property: Unregister decreases allocation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn unregister_decreases_allocation(
        fds_per_pane in arb_fds_per_pane(),
        n in 2_usize..30,
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            ..FdBudgetConfig::default()
        };
        let budget = FdBudget::with_limit(config, 1_000_000);

        for i in 0..n as u64 {
            budget.register_pane(i);
        }

        // Remove first pane
        budget.unregister_pane(0);
        let snap = budget.snapshot();
        prop_assert_eq!(snap.total_allocated, fds_per_pane * (n as u64 - 1));
        prop_assert_eq!(snap.pane_count, n - 1);
    }
}

// =============================================================================
// Property: Register/unregister roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn register_unregister_roundtrip(
        pane_ids in proptest::collection::hash_set(arb_pane_id(), 1..30),
    ) {
        let budget = FdBudget::with_limit(FdBudgetConfig::default(), 1_000_000);

        for &id in &pane_ids {
            budget.register_pane(id);
        }
        for &id in &pane_ids {
            budget.unregister_pane(id);
        }

        let snap = budget.snapshot();
        prop_assert_eq!(snap.total_allocated, 0,
            "total should be 0 after registering and unregistering all panes");
        prop_assert_eq!(snap.pane_count, 0);
    }
}

// =============================================================================
// Property: Admission threshold — refuse when over
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn refuse_when_over_threshold(
        fds_per_pane in 10_u64..50,
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            refuse_threshold: 0.95,
            warn_threshold: 0.80,
            ..FdBudgetConfig::default()
        };
        let limit = 1000_u64;
        let budget = FdBudget::with_limit(config, limit);

        // Register enough panes to exceed refuse threshold
        // projected = (current + fds_per_pane) / limit >= 0.95
        // So current >= 0.95 * limit - fds_per_pane = 950 - fds_per_pane
        let n_panes = (950 / fds_per_pane) + 1;
        for i in 0..n_panes {
            budget.register_pane(i);
        }

        let decision = budget.can_admit_pane();
        prop_assert!(!decision.is_allowed(),
            "should refuse when projected usage exceeds 95%, current_alloc={}, limit={}",
            budget.snapshot().total_allocated, limit);
    }
}

// =============================================================================
// Property: Warning threshold
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn warn_near_threshold(
        fds_per_pane in 10_u64..50,
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            refuse_threshold: 0.99,
            warn_threshold: 0.50,
            ..FdBudgetConfig::default()
        };
        let limit = 1000_u64;
        let budget = FdBudget::with_limit(config, limit);

        // Register enough to exceed warn but not refuse
        // projected = (current + fds_per_pane) / limit >= 0.50
        // current >= 500 - fds_per_pane
        let n_panes = (500 / fds_per_pane) + 1;
        for i in 0..n_panes {
            budget.register_pane(i);
        }

        let decision = budget.can_admit_pane();
        // Should be either Warned or Refused (depending on how many panes)
        match decision {
            AdmitDecision::Warned { .. } | AdmitDecision::Refused { .. } => {}
            AdmitDecision::Allowed => {
                let snap = budget.snapshot();
                let projected = snap.total_allocated + fds_per_pane;
                let ratio = projected as f64 / limit as f64;
                prop_assert!(ratio < 0.50,
                    "should not be Allowed when ratio {} >= 0.50", ratio);
            }
        }
    }
}

// =============================================================================
// Property: Snapshot consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn snapshot_consistent(
        fds_per_pane in arb_fds_per_pane(),
        n in 1_usize..30,
        limit in arb_limit(),
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            ..FdBudgetConfig::default()
        };
        let budget = FdBudget::with_limit(config, limit);

        for i in 0..n as u64 {
            budget.register_pane(i);
        }

        let snap = budget.snapshot();
        prop_assert_eq!(snap.total_allocated, fds_per_pane * n as u64);
        prop_assert_eq!(snap.pane_count, n);
        prop_assert_eq!(snap.effective_limit, limit);
        prop_assert!(snap.budget_ratio >= 0.0);
    }
}

// =============================================================================
// Property: Budget ratio bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn budget_ratio_bounded(
        fds_per_pane in 1_u64..10,
        n in 0_usize..20,
        limit in 10000_u64..100_000,
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            ..FdBudgetConfig::default()
        };
        let budget = FdBudget::with_limit(config, limit);

        for i in 0..n as u64 {
            budget.register_pane(i);
        }

        let snap = budget.snapshot();
        prop_assert!(snap.budget_ratio >= 0.0,
            "budget_ratio should be >= 0, got {}", snap.budget_ratio);
        prop_assert!(snap.budget_ratio <= 1.0,
            "budget_ratio should be <= 1, got {} (allocated={}, limit={})",
            snap.budget_ratio, snap.total_allocated, limit);
    }
}

// =============================================================================
// Property: Pane breakdown consistent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_breakdown_consistent(
        fds_per_pane in arb_fds_per_pane(),
        pane_ids in proptest::collection::hash_set(arb_pane_id(), 1..20),
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            ..FdBudgetConfig::default()
        };
        let budget = FdBudget::with_limit(config, 1_000_000);

        for &id in &pane_ids {
            budget.register_pane(id);
        }

        let breakdown = budget.pane_breakdown();
        prop_assert_eq!(breakdown.len(), pane_ids.len());

        for &id in &pane_ids {
            prop_assert_eq!(*breakdown.get(&id).unwrap_or(&0), fds_per_pane,
                "pane {} should have {} FDs", id, fds_per_pane);
        }
    }
}

// =============================================================================
// Property: Config defaults sensible
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_defaults_sensible(
        _dummy in 0..1_u32,
    ) {
        let config = FdBudgetConfig::default();
        prop_assert!(config.warn_threshold < config.refuse_threshold,
            "warn {} should be < refuse {}", config.warn_threshold, config.refuse_threshold);
        prop_assert!(config.fds_per_pane > 0);
        prop_assert!(config.warn_threshold > 0.0 && config.warn_threshold < 1.0);
        prop_assert!(config.refuse_threshold > 0.0 && config.refuse_threshold <= 1.0);
    }
}

// =============================================================================
// Property: AdmitDecision is_allowed
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn admit_allowed_is_allowed(
        current in 0_u64..1000,
        limit in 1000_u64..10000,
    ) {
        let allowed = AdmitDecision::Allowed;
        prop_assert!(allowed.is_allowed());
        let warned = AdmitDecision::Warned {
            current_fds: current,
            limit,
            usage_ratio: current as f64 / limit as f64,
        };
        prop_assert!(warned.is_allowed());
        let refused = AdmitDecision::Refused {
            current_fds: current,
            limit,
            projected: current + 25,
        };
        prop_assert!(!refused.is_allowed());
    }
}

// =============================================================================
// Property: Unregister nonexistent is safe
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn unregister_nonexistent_safe(
        pane_id in arb_pane_id(),
    ) {
        let budget = FdBudget::with_limit(FdBudgetConfig::default(), 100_000);
        // Register one pane
        budget.register_pane(pane_id);
        // Try to unregister a different pane
        budget.unregister_pane(pane_id + 1);
        // Original pane should still be tracked
        let snap = budget.snapshot();
        prop_assert_eq!(snap.pane_count, 1);
        prop_assert_eq!(snap.total_allocated, FdBudgetConfig::default().fds_per_pane);
    }
}
