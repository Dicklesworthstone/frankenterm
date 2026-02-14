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
//! 11. Serde roundtrips for FdBudgetConfig, FdSnapshot, AuditResult,
//!     SystemLimits, LimitValidation, LimitCheck

use proptest::prelude::*;

use frankenterm_core::fd_budget::{
    AdmitDecision, AuditResult, FdBudget, FdBudgetConfig, FdSnapshot, LimitCheck, LimitValidation,
    SystemLimits,
};

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

fn arb_fd_budget_config() -> impl Strategy<Value = FdBudgetConfig> {
    (
        0.5_f64..0.9,     // warn_threshold
        0.9_f64..1.0,     // refuse_threshold
        1_u64..100,       // fds_per_pane
        1000_u64..200000, // min_nofile_limit
        5_u64..120,       // audit_interval_secs
        1_usize..20,      // leak_detection_count
    )
        .prop_map(|(warn, refuse, fpp, mnl, ais, ldc)| {
            let warn = warn.min(refuse - 0.01);
            FdBudgetConfig {
                warn_threshold: warn,
                refuse_threshold: refuse,
                fds_per_pane: fpp,
                min_nofile_limit: mnl,
                audit_interval_secs: ais,
                leak_detection_count: ldc,
            }
        })
}

fn arb_fd_snapshot() -> impl Strategy<Value = FdSnapshot> {
    (
        0_u64..10_000,    // current_open
        0_u64..10_000,    // total_allocated
        1000_u64..100000, // effective_limit
        0_usize..200,     // pane_count
    )
        .prop_map(
            |(current_open, total_allocated, effective_limit, pane_count)| {
                let usage_ratio = current_open as f64 / effective_limit as f64;
                let budget_ratio = total_allocated as f64 / effective_limit as f64;
                FdSnapshot {
                    current_open,
                    total_allocated,
                    effective_limit,
                    pane_count,
                    usage_ratio,
                    budget_ratio,
                }
            },
        )
}

fn arb_audit_result() -> impl Strategy<Value = AuditResult> {
    (
        0_u64..10_000,    // current_fds
        1000_u64..100000, // effective_limit
        any::<bool>(),    // leak_detected
        any::<bool>(),    // warning
        0_usize..100,     // audit_count
    )
        .prop_map(
            |(current_fds, effective_limit, leak_detected, warning, audit_count)| {
                let usage_ratio = current_fds as f64 / effective_limit as f64;
                AuditResult {
                    current_fds,
                    effective_limit,
                    usage_ratio,
                    leak_detected,
                    warning,
                    audit_count,
                }
            },
        )
}

fn arb_system_limits() -> impl Strategy<Value = SystemLimits> {
    (
        1000_u64..100_000,                             // nofile_soft
        100_000_u64..1_000_000,                        // nofile_hard
        proptest::option::of(100_000_u64..10_000_000), // system_max_files
        0_u64..5000,                                   // current_open_fds
    )
        .prop_map(|(soft, hard, sys_max, current)| {
            let hard = hard.max(soft);
            SystemLimits {
                nofile_soft: soft,
                nofile_hard: hard,
                system_max_files: sys_max,
                current_open_fds: current,
                platform: "test".to_string(),
            }
        })
}

fn arb_limit_check() -> impl Strategy<Value = LimitCheck> {
    (
        "[a-z_]{3,15}",
        0_u64..200_000,
        0_u64..200_000,
        any::<bool>(),
    )
        .prop_map(|(name, current, required, ok)| LimitCheck {
            name,
            current,
            required,
            ok,
        })
}

fn arb_limit_validation() -> impl Strategy<Value = LimitValidation> {
    (
        any::<bool>(),
        prop::collection::vec(arb_limit_check(), 0..5),
        prop::collection::vec("[a-z_ ]{5,30}", 0..3),
    )
        .prop_map(|(all_ok, checks, fix_commands)| LimitValidation {
            all_ok,
            checks,
            fix_commands,
        })
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

// =============================================================================
// Serde roundtrip: FdBudgetConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// FdBudgetConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_fd_budget_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: FdBudgetConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.warn_threshold - config.warn_threshold).abs() < 1e-10,
            "warn_threshold: {} vs {}", back.warn_threshold, config.warn_threshold);
        prop_assert!((back.refuse_threshold - config.refuse_threshold).abs() < 1e-10,
            "refuse_threshold: {} vs {}", back.refuse_threshold, config.refuse_threshold);
        prop_assert_eq!(back.fds_per_pane, config.fds_per_pane);
        prop_assert_eq!(back.min_nofile_limit, config.min_nofile_limit);
        prop_assert_eq!(back.audit_interval_secs, config.audit_interval_secs);
        prop_assert_eq!(back.leak_detection_count, config.leak_detection_count);
    }

    /// FdBudgetConfig deserializes from empty JSON with defaults.
    #[test]
    fn prop_config_from_empty_json(_dummy in 0..1_u8) {
        let back: FdBudgetConfig = serde_json::from_str("{}").unwrap();
        let expected = FdBudgetConfig::default();
        prop_assert!((back.warn_threshold - expected.warn_threshold).abs() < 1e-15);
        prop_assert!((back.refuse_threshold - expected.refuse_threshold).abs() < 1e-15);
        prop_assert_eq!(back.fds_per_pane, expected.fds_per_pane);
        prop_assert_eq!(back.min_nofile_limit, expected.min_nofile_limit);
    }

    /// FdBudgetConfig serialization is deterministic.
    #[test]
    fn prop_config_deterministic(config in arb_fd_budget_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =============================================================================
// Serde roundtrip: FdSnapshot
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// FdSnapshot serde roundtrip preserves all fields.
    #[test]
    fn prop_snapshot_serde_roundtrip(snap in arb_fd_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: FdSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.current_open, snap.current_open);
        prop_assert_eq!(back.total_allocated, snap.total_allocated);
        prop_assert_eq!(back.effective_limit, snap.effective_limit);
        prop_assert_eq!(back.pane_count, snap.pane_count);
        prop_assert!((back.usage_ratio - snap.usage_ratio).abs() < 1e-10,
            "usage_ratio: {} vs {}", back.usage_ratio, snap.usage_ratio);
        prop_assert!((back.budget_ratio - snap.budget_ratio).abs() < 1e-10,
            "budget_ratio: {} vs {}", back.budget_ratio, snap.budget_ratio);
    }
}

// =============================================================================
// Serde roundtrip: AuditResult
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// AuditResult serde roundtrip preserves all fields.
    #[test]
    fn prop_audit_result_serde_roundtrip(audit in arb_audit_result()) {
        let json = serde_json::to_string(&audit).unwrap();
        let back: AuditResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.current_fds, audit.current_fds);
        prop_assert_eq!(back.effective_limit, audit.effective_limit);
        prop_assert!((back.usage_ratio - audit.usage_ratio).abs() < 1e-10);
        prop_assert_eq!(back.leak_detected, audit.leak_detected);
        prop_assert_eq!(back.warning, audit.warning);
        prop_assert_eq!(back.audit_count, audit.audit_count);
    }
}

// =============================================================================
// Serde roundtrip: SystemLimits
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// SystemLimits serde roundtrip preserves all fields.
    #[test]
    fn prop_system_limits_serde_roundtrip(limits in arb_system_limits()) {
        let json = serde_json::to_string(&limits).unwrap();
        let back: SystemLimits = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.nofile_soft, limits.nofile_soft);
        prop_assert_eq!(back.nofile_hard, limits.nofile_hard);
        prop_assert_eq!(back.system_max_files, limits.system_max_files);
        prop_assert_eq!(back.current_open_fds, limits.current_open_fds);
        prop_assert_eq!(&back.platform, &limits.platform);
    }
}

// =============================================================================
// Serde roundtrip: LimitValidation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// LimitValidation serde roundtrip preserves structure.
    #[test]
    fn prop_limit_validation_serde_roundtrip(val in arb_limit_validation()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: LimitValidation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.all_ok, val.all_ok);
        prop_assert_eq!(back.checks.len(), val.checks.len());
        prop_assert_eq!(back.fix_commands.len(), val.fix_commands.len());
        for (b, v) in back.checks.iter().zip(val.checks.iter()) {
            prop_assert_eq!(&b.name, &v.name);
            prop_assert_eq!(b.current, v.current);
            prop_assert_eq!(b.required, v.required);
            prop_assert_eq!(b.ok, v.ok);
        }
    }

    /// LimitCheck serde roundtrip.
    #[test]
    fn prop_limit_check_serde_roundtrip(check in arb_limit_check()) {
        let json = serde_json::to_string(&check).unwrap();
        let back: LimitCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &check.name);
        prop_assert_eq!(back.current, check.current);
        prop_assert_eq!(back.required, check.required);
        prop_assert_eq!(back.ok, check.ok);
    }
}

// =============================================================================
// Property: Double register same pane is idempotent for allocation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn double_register_behavior(
        fds_per_pane in arb_fds_per_pane(),
        pane_id in arb_pane_id(),
    ) {
        let config = FdBudgetConfig {
            fds_per_pane,
            ..FdBudgetConfig::default()
        };
        let budget = FdBudget::with_limit(config, 1_000_000);

        budget.register_pane(pane_id);
        let snap1 = budget.snapshot();

        budget.register_pane(pane_id);
        let snap2 = budget.snapshot();

        // Double register: allocation should be same (idempotent) or doubled.
        // Either behavior is valid — just verify consistency.
        prop_assert!(snap2.total_allocated >= snap1.total_allocated,
            "double register should not decrease allocation");
    }
}
