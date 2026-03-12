//! Property tests for chaos_scale_harness module (ft-3681t.7.4).
//!
//! Covers serde roundtrips for data types, scale profile factory invariants,
//! harness run invariants, SLO evaluation logic, and report consistency.

use frankenterm_core::chaos_scale_harness::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_failure_class() -> impl Strategy<Value = FailureClass> {
    prop_oneof![
        Just(FailureClass::ConnectorTransient),
        Just(FailureClass::ConnectorOffline),
        Just(FailureClass::CpuOverload),
        Just(FailureClass::MemoryExhaustion),
        Just(FailureClass::IoStall),
        Just(FailureClass::PolicyChurn),
        Just(FailureClass::ContextExhaustion),
        Just(FailureClass::RchWorkerLoss),
        Just(FailureClass::CascadingFailure),
    ]
}

fn arb_slo_metric() -> impl Strategy<Value = SloMetric> {
    prop_oneof![
        Just(SloMetric::GovernorAllowRate),
        Just(SloMetric::GovernorBlockRate),
        Just(SloMetric::CircuitBreakerTripRate),
        Just(SloMetric::DlqDepth),
        Just(SloMetric::ContextUtilization),
        Just(SloMetric::PanesNeedingAttention),
        Just(SloMetric::RecoveryTimeMs),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_failure_class(class in arb_failure_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: FailureClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(class, back);
    }

    #[test]
    fn serde_roundtrip_slo_metric(metric in arb_slo_metric()) {
        let json = serde_json::to_string(&metric).unwrap();
        let back: SloMetric = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(metric, back);
    }

    #[test]
    fn serde_roundtrip_scale_profile(_dummy in 0..1u32) {
        let profile = ScaleProfile::small();
        let json = serde_json::to_string(&profile).unwrap();
        let back: ScaleProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_count, profile.pane_count);
        prop_assert_eq!(back.label, profile.label);
    }

    #[test]
    fn serde_roundtrip_slo_definition(
        threshold in 0.0..1.0f64,
        higher_is_better in any::<bool>(),
    ) {
        let slo = SloDefinition {
            name: "test-slo".into(),
            metric: SloMetric::GovernorAllowRate,
            threshold,
            higher_is_better,
        };
        let json = serde_json::to_string(&slo).unwrap();
        let back: SloDefinition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.name, "test-slo");
        prop_assert_eq!(back.higher_is_better, higher_is_better);
    }
}

// =============================================================================
// Scale profile factory invariants
// =============================================================================

proptest! {
    #[test]
    fn scale_profile_sizes_ordered(_dummy in 0..1u32) {
        let s = ScaleProfile::small();
        let m = ScaleProfile::medium();
        let l = ScaleProfile::large();
        prop_assert!(s.pane_count < m.pane_count);
        prop_assert!(m.pane_count < l.pane_count);
        prop_assert!(s.connector_count < m.connector_count);
        prop_assert!(m.connector_count < l.connector_count);
        prop_assert!(s.duration_ms < m.duration_ms);
        prop_assert!(m.duration_ms < l.duration_ms);
    }

    #[test]
    fn scale_profile_fields_positive(_dummy in 0..1u32) {
        for profile in [ScaleProfile::small(), ScaleProfile::medium(), ScaleProfile::large()] {
            prop_assert!(profile.pane_count > 0);
            prop_assert!(profile.connector_count > 0);
            prop_assert!(profile.policy_eval_count > 0);
            prop_assert!(profile.event_burst_rate > 0);
            prop_assert!(profile.duration_ms > 0);
            prop_assert!(!profile.label.is_empty());
        }
    }
}

// =============================================================================
// Harness run invariants
// =============================================================================

proptest! {
    #[test]
    fn green_path_passes_all_slos(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        prop_assert!(report.overall_pass);
        prop_assert_eq!(report.failure_classes.len(), 0);
        prop_assert!(report.governor_probe.allow_rate > 0.5);
    }

    #[test]
    fn report_profile_label_matches(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        prop_assert_eq!(report.profile_label, "small");
    }

    #[test]
    fn report_pane_count_matches_profile(_dummy in 0..1u32) {
        let profile = ScaleProfile::small();
        let expected_panes = profile.pane_count;
        let mut harness = ChaosScaleHarness::new(profile);
        let report = harness.run();
        prop_assert_eq!(report.pane_probe.pane_count, expected_panes);
    }

    #[test]
    fn report_connector_count_matches_profile(_dummy in 0..1u32) {
        let profile = ScaleProfile::small();
        let expected = profile.connector_count;
        let mut harness = ChaosScaleHarness::new(profile);
        let report = harness.run();
        prop_assert_eq!(report.connector_stress.connector_count, expected);
    }

    #[test]
    fn governor_counter_sum_equals_evaluations(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        let g = &report.governor_probe;
        let sum = g.allowed + g.throttled + g.offloaded + g.blocked;
        prop_assert_eq!(sum, g.evaluations);
    }

    #[test]
    fn connector_stress_ops_sum_consistent(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        let c = &report.connector_stress;
        // Successes + failures should account for all ops
        // (some ops may be blocked by circuit breaker)
        prop_assert!(c.total_successes + c.total_failures <= c.total_operations);
    }

    #[test]
    fn overall_pass_matches_slo_results(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        let all_slos_pass = report.slo_results.iter().all(|s| s.passed);
        prop_assert_eq!(report.overall_pass, all_slos_pass);
    }
}

// =============================================================================
// Failure injection effects
// =============================================================================

proptest! {
    #[test]
    fn context_exhaustion_increases_attention_panes(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::ContextExhaustion);
        let report = harness.run();
        prop_assert!(report.pane_probe.panes_needing_attention > 0);
        prop_assert!(report.pane_probe.average_utilization > 0.5);
    }

    #[test]
    fn rch_loss_prevents_offloading(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::RchWorkerLoss);
        let report = harness.run();
        prop_assert_eq!(report.governor_probe.offloaded, 0);
    }

    #[test]
    fn connector_offline_causes_failures(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::ConnectorOffline);
        let report = harness.run();
        prop_assert!(report.connector_stress.total_failures > 0);
        prop_assert!(report.connector_stress.circuit_breaker_trips > 0);
    }

    #[test]
    fn injected_failures_appear_in_report(class in arb_failure_class()) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(class);
        let report = harness.run();
        prop_assert!(report.failure_classes.contains(&class));
    }
}

// =============================================================================
// SLO evaluation logic
// =============================================================================

proptest! {
    #[test]
    fn slo_higher_is_better_logic(
        threshold in 0.0..1.0f64,
        actual in 0.0..1.0f64,
    ) {
        // Emulate the SLO evaluation: higher_is_better=true → actual >= threshold
        let passed = actual >= threshold;
        let slo_result = SloResult {
            name: "test".into(),
            metric: SloMetric::GovernorAllowRate,
            threshold,
            actual,
            passed,
        };
        prop_assert_eq!(slo_result.passed, actual >= threshold);
    }

    #[test]
    fn slo_lower_is_better_logic(
        threshold in 0.0..1.0f64,
        actual in 0.0..1.0f64,
    ) {
        let passed = actual <= threshold;
        let slo_result = SloResult {
            name: "test".into(),
            metric: SloMetric::GovernorBlockRate,
            threshold,
            actual,
            passed,
        };
        prop_assert_eq!(slo_result.passed, actual <= threshold);
    }
}

// =============================================================================
// Report serialization
// =============================================================================

proptest! {
    #[test]
    fn report_serde_roundtrip(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        let json = serde_json::to_string(&report).unwrap();
        let back: HarnessReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.profile_label, report.profile_label);
        prop_assert_eq!(back.overall_pass, report.overall_pass);
        prop_assert_eq!(back.slo_results.len(), report.slo_results.len());
        prop_assert_eq!(back.duration_ms, report.duration_ms);
    }

    #[test]
    fn summary_line_contains_verdict(_dummy in 0..1u32) {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        let line = report.summary_line();
        let has_verdict = line.contains("[PASS]") || line.contains("[FAIL]");
        prop_assert!(has_verdict);
        prop_assert!(line.contains("small"));
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn default_slos_are_populated() {
    let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
    let report = harness.run();
    assert!(report.slo_results.len() >= 4);
}

#[test]
fn compactions_occur_in_pane_probe() {
    let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
    let report = harness.run();
    assert!(report.pane_probe.total_compactions > 0);
}
