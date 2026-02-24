//! Property-based tests for ARS Blast Radius Controller.
//!
//! Verifies hierarchical rate limiting, maturity tier promotion/demotion,
//! token replenishment, stats invariants, and serde roundtrips.

use proptest::prelude::*;

use std::collections::HashMap;

use frankenterm_core::ars_blast_radius::{
    BlastDecision, BlastRadiusConfig, BlastRadiusController, BlastStats, DenyReason, MaturityTier,
    ReflexState,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_tier() -> impl Strategy<Value = MaturityTier> {
    prop_oneof![
        Just(MaturityTier::Incubating),
        Just(MaturityTier::Graduated),
        Just(MaturityTier::Veteran),
    ]
}

fn arb_config() -> impl Strategy<Value = BlastRadiusConfig> {
    (
        1.0..100.0f64, // swarm_rate
        1.0..20.0f64,  // swarm_burst
        1.0..100.0f64, // cluster_rate
        1.0..20.0f64,  // cluster_burst
        1.0..50.0f64,  // incubating_rate
        1.0..10.0f64,  // incubating_burst
        1.0..100.0f64, // graduated_rate
        1.0..20.0f64,  // graduated_burst
    )
        .prop_map(|(sr, sb, cr, cb, ir, ib, gr, gb)| BlastRadiusConfig {
            swarm_rate_per_min: sr,
            swarm_burst: sb,
            cluster_rate_per_min: cr,
            cluster_burst: cb,
            incubating_rate_per_min: ir,
            incubating_burst: ib,
            graduated_rate_per_min: gr,
            graduated_burst: gb,
            veteran_rate_per_min: gr * 2.0,
            veteran_burst: gb * 2.0,
            graduation_threshold: 5,
            veteran_threshold: 20,
            demotion_failure_count: 3,
        })
}

fn arb_reflex_state() -> impl Strategy<Value = ReflexState> {
    (arb_tier(), 0..100u64, 0..50u64, 0..10u64, "[a-z]{2,6}").prop_map(
        |(tier, successes, failures, consec, cluster)| ReflexState {
            tier,
            successes,
            failures,
            consecutive_failures: consec,
            cluster_id: cluster,
        },
    )
}

// =============================================================================
// Tier invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tier_ordering_is_total(a in arb_tier(), b in arb_tier()) {
        // Tiers implement Ord — all pairs are comparable.
        let _cmp = a.cmp(&b);
    }

    #[test]
    fn tier_serde_roundtrip(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let decoded: MaturityTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, tier);
    }
}

// =============================================================================
// Reflex state promotion/demotion invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn promotion_monotone_until_veteran(
        grad_thresh in 2..20u64,
        vet_thresh in 20..100u64,
    ) {
        let config = BlastRadiusConfig {
            graduation_threshold: grad_thresh,
            veteran_threshold: vet_thresh,
            demotion_failure_count: 100, // disable demotion
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");

        let mut prev_tier = state.tier;
        for _ in 0..vet_thresh {
            state.record_success(&config);
            // Tier should only increase or stay the same (no demotion).
            prop_assert!(state.tier >= prev_tier);
            prev_tier = state.tier;
        }
        prop_assert_eq!(state.tier, MaturityTier::Veteran);
    }

    #[test]
    fn demotion_on_consecutive_failures(
        demotion_count in 1..10u64,
    ) {
        let config = BlastRadiusConfig {
            graduation_threshold: 1,
            demotion_failure_count: demotion_count,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");

        // Promote first.
        state.record_success(&config);
        prop_assert_eq!(state.tier, MaturityTier::Graduated);

        // Apply consecutive failures.
        for _ in 0..demotion_count {
            state.record_failure(&config);
        }
        prop_assert_eq!(state.tier, MaturityTier::Incubating);
    }

    #[test]
    fn success_resets_consecutive_failures(
        n_failures in 1..5u64,
    ) {
        let config = BlastRadiusConfig {
            demotion_failure_count: 100, // won't demote
            graduation_threshold: 100,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");
        state.tier = MaturityTier::Graduated;

        for _ in 0..n_failures {
            state.record_failure(&config);
        }
        prop_assert_eq!(state.consecutive_failures, n_failures);

        state.record_success(&config);
        prop_assert_eq!(state.consecutive_failures, 0);
    }

    #[test]
    fn total_executions_is_sum(
        n_success in 0..20u64,
        n_failure in 0..20u64,
    ) {
        let config = BlastRadiusConfig {
            graduation_threshold: 1000,
            demotion_failure_count: 1000,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");
        for _ in 0..n_success {
            state.record_success(&config);
        }
        for _ in 0..n_failure {
            state.record_failure(&config);
        }
        prop_assert_eq!(state.total_executions(), n_success + n_failure);
        prop_assert_eq!(state.successes, n_success);
        prop_assert_eq!(state.failures, n_failure);
    }

    #[test]
    fn incubating_cannot_demote(
        n_failures in 1..20u64,
    ) {
        let config = BlastRadiusConfig {
            demotion_failure_count: 1,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");
        // Already incubating.
        for _ in 0..n_failures {
            state.record_failure(&config);
        }
        // Should stay incubating (can't go lower).
        prop_assert_eq!(state.tier, MaturityTier::Incubating);
    }
}

// =============================================================================
// Controller rate limiting invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn first_check_always_allowed(
        reflex_id in 1..100u64,
        cluster in "[a-z]{2,6}",
    ) {
        let mut ctrl = BlastRadiusController::with_defaults();
        ctrl.register_reflex(reflex_id, &cluster);
        let decision = ctrl.check(reflex_id, 1000);
        prop_assert!(decision.is_allowed());
    }

    #[test]
    fn burst_exhaustion_denies(
        burst in 1.0..5.0f64,
    ) {
        let config = BlastRadiusConfig {
            incubating_rate_per_min: 0.001, // negligible refill
            incubating_burst: burst,
            swarm_rate_per_min: 60000.0,
            swarm_burst: 1000.0,
            cluster_rate_per_min: 60000.0,
            cluster_burst: 1000.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        let burst_count = burst.floor() as usize;
        for i in 0..burst_count {
            let d = ctrl.check(1, 1000 + i as u64);
            prop_assert!(d.is_allowed(), "should allow within burst at i={}", i);
        }

        // One more should be denied.
        let d = ctrl.check(1, 1000 + burst_count as u64);
        prop_assert!(!d.is_allowed());
    }

    #[test]
    fn stats_total_equals_allowed_plus_denied(
        n_checks in 1..20usize,
    ) {
        let config = BlastRadiusConfig {
            incubating_rate_per_min: 6.0,
            incubating_burst: 2.0,
            swarm_rate_per_min: 60000.0,
            swarm_burst: 1000.0,
            cluster_rate_per_min: 60000.0,
            cluster_burst: 1000.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        for i in 0..n_checks {
            ctrl.check(1, 1000 + i as u64);
        }

        let stats = ctrl.stats();
        prop_assert_eq!(
            stats.total_allowed + stats.total_denied,
            n_checks as u64
        );
    }

    #[test]
    fn multiple_reflexes_independent(
        n_reflexes in 2..6usize,
    ) {
        let config = BlastRadiusConfig {
            incubating_rate_per_min: 6.0,
            incubating_burst: 1.0,
            swarm_rate_per_min: 60000.0,
            swarm_burst: 1000.0,
            cluster_rate_per_min: 60000.0,
            cluster_burst: 1000.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        for i in 0..n_reflexes {
            ctrl.register_reflex(i as u64 + 1, &format!("c{i}"));
        }

        // Each reflex should be independently allowed once.
        for i in 0..n_reflexes {
            let d = ctrl.check(i as u64 + 1, 1000);
            prop_assert!(d.is_allowed(), "reflex {} should be allowed", i + 1);
        }

        let stats = ctrl.stats();
        prop_assert_eq!(stats.total_allowed, n_reflexes as u64);
    }

    #[test]
    fn swarm_limit_caps_all(
        n_reflexes in 2..5usize,
    ) {
        let config = BlastRadiusConfig {
            swarm_rate_per_min: 0.001,
            swarm_burst: 1.0,
            cluster_rate_per_min: 60000.0,
            cluster_burst: 1000.0,
            incubating_rate_per_min: 60000.0,
            incubating_burst: 1000.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        for i in 0..n_reflexes {
            ctrl.register_reflex(i as u64 + 1, "c1");
        }

        // First should pass.
        let d1 = ctrl.check(1, 1000);
        prop_assert!(d1.is_allowed());

        // Second (different reflex) should fail at swarm level.
        let d2 = ctrl.check(2, 1001);
        let is_swarm = matches!(d2, BlastDecision::Deny { reason: DenyReason::SwarmLimit, .. });
        prop_assert!(is_swarm);
    }

    #[test]
    fn promotion_increases_effective_rate(
        grad_thresh in 2..10u64,
    ) {
        let config = BlastRadiusConfig {
            graduation_threshold: grad_thresh,
            veteran_threshold: 1000,
            demotion_failure_count: 1000,
            incubating_rate_per_min: 6.0,
            incubating_burst: 1.0,
            graduated_rate_per_min: 600.0,
            graduated_burst: 10.0,
            swarm_rate_per_min: 60000.0,
            swarm_burst: 1000.0,
            cluster_rate_per_min: 60000.0,
            cluster_burst: 1000.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        // Use up incubating burst.
        ctrl.check(1, 1000);
        let is_denied = !ctrl.check(1, 1001).is_allowed();
        prop_assert!(is_denied, "should be rate-limited as incubating");

        // Promote to graduated.
        for _ in 0..grad_thresh {
            ctrl.record_success(1);
        }
        let tier = ctrl.reflex_state(1).unwrap().tier;
        prop_assert_eq!(tier, MaturityTier::Graduated);

        // After promotion, burst should be higher — allow again.
        let d = ctrl.check(1, 2000);
        prop_assert!(d.is_allowed(), "graduated should have more burst");
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: BlastRadiusConfig = serde_json::from_str(&json).unwrap();
        let diff = (decoded.swarm_rate_per_min - config.swarm_rate_per_min).abs();
        prop_assert!(diff < 1e-10);
        let diff2 = (decoded.cluster_burst - config.cluster_burst).abs();
        prop_assert!(diff2 < 1e-10);
    }

    #[test]
    fn reflex_state_serde_roundtrip(state in arb_reflex_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let decoded: ReflexState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.tier, state.tier);
        prop_assert_eq!(decoded.successes, state.successes);
        prop_assert_eq!(decoded.failures, state.failures);
        prop_assert_eq!(decoded.consecutive_failures, state.consecutive_failures);
        prop_assert_eq!(decoded.cluster_id, state.cluster_id);
    }

    #[test]
    fn blast_decision_serde_roundtrip(tier in arb_tier()) {
        let allow = BlastDecision::Allow { tier };
        let json = serde_json::to_string(&allow).unwrap();
        let decoded: BlastDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, allow);

        let deny = BlastDecision::Deny {
            reason: DenyReason::SwarmLimit,
            tier,
        };
        let json2 = serde_json::to_string(&deny).unwrap();
        let decoded2: BlastDecision = serde_json::from_str(&json2).unwrap();
        prop_assert_eq!(decoded2, deny);
    }

    #[test]
    fn deny_reason_serde_roundtrip(
        reflex_id in 1..1000u64,
        cluster in "[a-z]{2,6}",
    ) {
        let reasons = vec![
            DenyReason::SwarmLimit,
            DenyReason::ClusterLimit { cluster_id: cluster },
            DenyReason::ReflexLimit { reflex_id },
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let decoded: DenyReason = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(decoded, reason);
        }
    }

    #[test]
    fn blast_stats_serde_roundtrip(
        allowed in 0..1000u64,
        denied in 0..500u64,
        reflexes in 0..50usize,
        clusters in 0..10usize,
    ) {
        let stats = BlastStats {
            total_allowed: allowed,
            total_denied: denied,
            registered_reflexes: reflexes,
            cluster_count: clusters,
            tier_counts: HashMap::from([
                ("Incubating".to_string(), reflexes),
            ]),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: BlastStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }
}
