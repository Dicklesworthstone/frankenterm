//! Property tests for capacity_governor module (ft-3681t.7.3).
//!
//! Covers serde roundtrips, workload category weight ordering, pressure signal
//! health tier derivation, governor decision logic, operator override semantics,
//! telemetry counter consistency, and decision log invariants.

use frankenterm_core::capacity_governor::*;
use frankenterm_core::runtime_telemetry::HealthTier;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_workload_category() -> impl Strategy<Value = WorkloadCategory> {
    prop_oneof![
        Just(WorkloadCategory::Heavy),
        Just(WorkloadCategory::Medium),
        Just(WorkloadCategory::Light),
    ]
}

fn arb_pressure_signals() -> impl Strategy<Value = PressureSignals> {
    (
        0.0..1.0f64,       // cpu_utilization
        0.0..1.0f64,       // memory_utilization
        0..5u32,           // active_heavy_workloads
        0..10u32,          // active_medium_workloads
        0.0..20.0f64,      // load_average_1m
        any::<bool>(),     // rch_available
        0..8u32,           // rch_workers_available
        0.0..1.0f64,       // io_pressure
        0..10_000_000u64,  // timestamp_ms
    )
        .prop_map(
            |(cpu, mem, heavy, medium, load, rch, workers, io, ts)| PressureSignals {
                cpu_utilization: cpu,
                memory_utilization: mem,
                active_heavy_workloads: heavy,
                active_medium_workloads: medium,
                load_average_1m: load,
                rch_available: rch,
                rch_workers_available: workers,
                io_pressure: io,
                timestamp_ms: ts,
            },
        )
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_workload_category(cat in arb_workload_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: WorkloadCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_config(_dummy in 0..1u32) {
        let config = CapacityGovernorConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: CapacityGovernorConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    #[test]
    fn serde_roundtrip_pressure_signals(signals in arb_pressure_signals()) {
        let json = serde_json::to_string(&signals).unwrap();
        let back: PressureSignals = serde_json::from_str(&json).unwrap();
        // f64 roundtrip should be exact for finite values
        prop_assert_eq!(back.active_heavy_workloads, signals.active_heavy_workloads);
        prop_assert_eq!(back.rch_available, signals.rch_available);
        prop_assert_eq!(back.timestamp_ms, signals.timestamp_ms);
    }

    #[test]
    fn serde_roundtrip_telemetry(_dummy in 0..1u32) {
        let telem = GovernorTelemetry::default();
        let json = serde_json::to_string(&telem).unwrap();
        let back: GovernorTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.evaluations, 0);
    }
}

// =============================================================================
// Workload category weight ordering
// =============================================================================

proptest! {
    #[test]
    fn weight_monotonic(cat in arb_workload_category()) {
        let w = cat.weight();
        match cat {
            WorkloadCategory::Heavy => prop_assert!(w > WorkloadCategory::Medium.weight()),
            WorkloadCategory::Medium => prop_assert!(w > WorkloadCategory::Light.weight()),
            WorkloadCategory::Light => prop_assert!(w >= 1),
        }
    }
}

// =============================================================================
// Pressure signals → health tier
// =============================================================================

proptest! {
    #[test]
    fn health_tier_from_max_pressure(signals in arb_pressure_signals()) {
        let tier = signals.health_tier();
        let max_pressure = signals.cpu_utilization
            .max(signals.memory_utilization)
            .max(signals.io_pressure);

        let expected = HealthTier::from_ratio(max_pressure);
        prop_assert_eq!(tier, expected);
    }

    #[test]
    fn default_signals_are_green(_dummy in 0..1u32) {
        let signals = PressureSignals::default();
        prop_assert_eq!(signals.health_tier(), HealthTier::Green);
    }
}

// =============================================================================
// Governor decision logic
// =============================================================================

proptest! {
    #[test]
    fn extreme_cpu_always_blocks(
        cat in arb_workload_category(),
        cpu in 0.95..1.0f64,
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            cpu_utilization: cpu,
            ..PressureSignals::default()
        };
        let decision = gov.evaluate(cat, &signals);
        let is_block = matches!(decision, GovernorDecision::Block { .. });
        prop_assert!(is_block);
        prop_assert!(!decision.is_permitted());
    }

    #[test]
    fn extreme_memory_always_blocks(
        cat in arb_workload_category(),
        mem in 0.95..1.0f64,
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            memory_utilization: mem,
            ..PressureSignals::default()
        };
        let decision = gov.evaluate(cat, &signals);
        let is_block = matches!(decision, GovernorDecision::Block { .. });
        prop_assert!(is_block);
    }

    #[test]
    fn low_pressure_allows_any_category(cat in arb_workload_category()) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            cpu_utilization: 0.2,
            memory_utilization: 0.3,
            load_average_1m: 1.0,
            ..PressureSignals::default()
        };
        let decision = gov.evaluate(cat, &signals);
        let is_allow = matches!(decision, GovernorDecision::Allow { .. });
        prop_assert!(is_allow);
        prop_assert!(decision.is_permitted());
    }

    #[test]
    fn high_load_average_blocks_heavy_only(load in 12.0..100.0f64) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            cpu_utilization: 0.3,
            memory_utilization: 0.3,
            load_average_1m: load,
            ..PressureSignals::default()
        };
        // Heavy → blocked
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        let is_block = matches!(decision, GovernorDecision::Block { .. });
        prop_assert!(is_block);

        // Light → allowed (load average doesn't block light)
        let mut gov2 = CapacityGovernor::with_defaults();
        let decision = gov2.evaluate(WorkloadCategory::Light, &signals);
        let is_allow = matches!(decision, GovernorDecision::Allow { .. });
        prop_assert!(is_allow);
    }

    #[test]
    fn heavy_at_concurrency_limit_offloads_when_rch(
        workers in 1..8u32,
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            cpu_utilization: 0.3,
            memory_utilization: 0.3,
            active_heavy_workloads: 2, // = max_concurrent_heavy default
            rch_available: true,
            rch_workers_available: workers,
            ..PressureSignals::default()
        };
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        let is_offload = matches!(decision, GovernorDecision::Offload { .. });
        prop_assert!(is_offload);
    }

    #[test]
    fn heavy_at_concurrency_limit_throttles_without_rch(_dummy in 0..1u32) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            cpu_utilization: 0.3,
            memory_utilization: 0.3,
            active_heavy_workloads: 2,
            rch_available: false,
            ..PressureSignals::default()
        };
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        let is_throttle = matches!(decision, GovernorDecision::Throttle { .. });
        prop_assert!(is_throttle);
    }

    #[test]
    fn decision_always_has_reason(
        cat in arb_workload_category(),
        signals in arb_pressure_signals(),
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let decision = gov.evaluate(cat, &signals);
        prop_assert!(!decision.reason().is_empty());
    }

    #[test]
    fn block_is_never_permitted(
        cat in arb_workload_category(),
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            cpu_utilization: 0.99,
            ..PressureSignals::default()
        };
        let decision = gov.evaluate(cat, &signals);
        prop_assert!(!decision.is_permitted());
    }
}

// =============================================================================
// Operator override semantics
// =============================================================================

proptest! {
    #[test]
    fn override_no_expiry_always_active(now_ms in 0..u64::MAX / 2) {
        let ovr = OperatorOverride {
            operator: "admin".into(),
            category: None,
            expires_ms: 0,
            reason: "test".into(),
        };
        prop_assert!(ovr.is_active(now_ms));
    }

    #[test]
    fn override_expires_at_deadline(
        created in 0..1_000_000u64,
        ttl in 1..1_000_000u64,
    ) {
        let ovr = OperatorOverride {
            operator: "admin".into(),
            category: None,
            expires_ms: created + ttl,
            reason: "test".into(),
        };
        // Before expiry
        prop_assert!(ovr.is_active(created));
        // At or past expiry
        prop_assert!(!ovr.is_active(created + ttl));
    }

    #[test]
    fn override_with_no_category_matches_all(cat in arb_workload_category()) {
        let ovr = OperatorOverride {
            operator: "admin".into(),
            category: None,
            expires_ms: 0,
            reason: "test".into(),
        };
        prop_assert!(ovr.applies_to(cat));
    }

    #[test]
    fn override_with_specific_category_only_matches_that(
        target in arb_workload_category(),
        other in arb_workload_category(),
    ) {
        let ovr = OperatorOverride {
            operator: "admin".into(),
            category: Some(target),
            expires_ms: 0,
            reason: "test".into(),
        };
        prop_assert_eq!(ovr.applies_to(other), target == other);
    }

    #[test]
    fn active_override_produces_override_decision(
        cat in arb_workload_category(),
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        gov.add_override(OperatorOverride {
            operator: "admin".into(),
            category: None,
            expires_ms: 0,
            reason: "test".into(),
        });
        let signals = PressureSignals {
            cpu_utilization: 0.99,
            ..PressureSignals::default()
        };
        let decision = gov.evaluate(cat, &signals);
        let is_override = matches!(decision, GovernorDecision::Override { .. });
        prop_assert!(is_override);
    }
}

// =============================================================================
// Telemetry counter consistency
// =============================================================================

proptest! {
    #[test]
    fn telemetry_evaluations_count_matches(
        n_evals in 1..10usize,
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals::default();
        for _ in 0..n_evals {
            gov.evaluate(WorkloadCategory::Light, &signals);
        }
        prop_assert_eq!(gov.telemetry().evaluations, n_evals as u64);
    }

    #[test]
    fn telemetry_counter_sum_equals_evaluations(
        n_evals in 1..20usize,
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals {
            cpu_utilization: 0.2,
            memory_utilization: 0.2,
            ..PressureSignals::default()
        };
        for _ in 0..n_evals {
            gov.evaluate(WorkloadCategory::Light, &signals);
        }
        let t = gov.telemetry();
        let sum = t.allowed + t.throttled + t.offloaded + t.blocked + t.overrides;
        prop_assert_eq!(sum, t.evaluations);
    }
}

// =============================================================================
// Decision log invariants
// =============================================================================

proptest! {
    #[test]
    fn decision_log_grows_with_evaluations(
        n_evals in 0..8usize,
    ) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals::default();
        for _ in 0..n_evals {
            gov.evaluate(WorkloadCategory::Light, &signals);
        }
        prop_assert_eq!(gov.decision_log().len(), n_evals);
    }

    #[test]
    fn decision_log_records_correct_category(cat in arb_workload_category()) {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = PressureSignals::default();
        gov.evaluate(cat, &signals);
        prop_assert_eq!(gov.decision_log().len(), 1);
        prop_assert_eq!(gov.decision_log()[0].category, cat);
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn default_config_has_reasonable_thresholds() {
    let config = CapacityGovernorConfig::default();
    assert!(config.cpu_throttle_threshold < config.cpu_block_threshold);
    assert!(config.memory_throttle_threshold < config.memory_block_threshold);
    assert!(config.max_concurrent_heavy > 0);
    assert!(config.max_concurrent_medium > 0);
}

#[test]
fn new_governor_has_zero_telemetry() {
    let gov = CapacityGovernor::with_defaults();
    assert_eq!(gov.telemetry().evaluations, 0);
    assert!(gov.decision_log().is_empty());
}
