//! Property tests for runtime_slo_gates module.

use proptest::prelude::*;

use frankenterm_core::runtime_slo_gates::*;
use frankenterm_core::runtime_telemetry::FailureClass;

// =============================================================================
// Strategy helpers
// =============================================================================

fn arb_runtime_slo_id() -> impl Strategy<Value = RuntimeSloId> {
    prop_oneof![
        Just(RuntimeSloId::CancellationLatency),
        Just(RuntimeSloId::QueueBacklogDepth),
        Just(RuntimeSloId::TaskLeakRate),
        Just(RuntimeSloId::ServiceRecoveryTime),
        Just(RuntimeSloId::CaptureLoopLatency),
        Just(RuntimeSloId::EventDeliveryLoss),
        Just(RuntimeSloId::SchedulerDecisionLatency),
        Just(RuntimeSloId::StartupLatency),
    ]
}

fn arb_slo_comparison_op() -> impl Strategy<Value = SloComparisonOp> {
    prop_oneof![
        Just(SloComparisonOp::LessOrEqual),
        Just(SloComparisonOp::LessThan),
        Just(SloComparisonOp::GreaterOrEqual),
    ]
}

fn arb_alert_tier() -> impl Strategy<Value = RuntimeAlertTier> {
    prop_oneof![
        Just(RuntimeAlertTier::Info),
        Just(RuntimeAlertTier::Warning),
        Just(RuntimeAlertTier::Critical),
        Just(RuntimeAlertTier::Page),
    ]
}

fn arb_alert_action() -> impl Strategy<Value = AlertAction> {
    prop_oneof![
        Just(AlertAction::Log),
        Just(AlertAction::EmitEvent),
        Just(AlertAction::BlockGate),
        Just(AlertAction::AutoRemediate),
    ]
}

fn arb_gate_verdict() -> impl Strategy<Value = GateVerdict> {
    prop_oneof![
        Just(GateVerdict::Pass),
        Just(GateVerdict::ConditionalPass),
        Just(GateVerdict::Fail),
    ]
}

fn arb_failure_class() -> impl Strategy<Value = FailureClass> {
    prop_oneof![
        Just(FailureClass::Transient),
        Just(FailureClass::Permanent),
        Just(FailureClass::Degraded),
        Just(FailureClass::Overload),
        Just(FailureClass::Corruption),
        Just(FailureClass::Timeout),
        Just(FailureClass::Panic),
        Just(FailureClass::Deadlock),
        Just(FailureClass::Safety),
        Just(FailureClass::Configuration),
    ]
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_runtime_slo_id(id in arb_runtime_slo_id()) {
        let json = serde_json::to_string(&id).unwrap();
        let restored: RuntimeSloId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(id, restored);
    }

    #[test]
    fn serde_roundtrip_slo_comparison_op(op in arb_slo_comparison_op()) {
        let json = serde_json::to_string(&op).unwrap();
        let restored: SloComparisonOp = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(op, restored);
    }

    #[test]
    fn serde_roundtrip_alert_tier(tier in arb_alert_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let restored: RuntimeAlertTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, restored);
    }

    #[test]
    fn serde_roundtrip_alert_action(action in arb_alert_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let restored: AlertAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, restored);
    }

    #[test]
    fn serde_roundtrip_gate_verdict(v in arb_gate_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let restored: GateVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, restored);
    }

    #[test]
    fn serde_roundtrip_alert_policy(_dummy in Just(())) {
        let policy = standard_alert_policy();
        let json = serde_json::to_string(&policy).unwrap();
        let restored: AlertPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(policy.escalation_map.len(), restored.escalation_map.len());
        prop_assert_eq!(&policy.policy_id, &restored.policy_id);
    }
}

// =============================================================================
// RuntimeSloId invariants
// =============================================================================

proptest! {
    #[test]
    fn slo_id_as_str_not_empty(id in arb_runtime_slo_id()) {
        prop_assert!(!id.as_str().is_empty());
    }

    #[test]
    fn slo_id_as_str_starts_with_rt(id in arb_runtime_slo_id()) {
        prop_assert!(id.as_str().starts_with("rt."));
    }

    #[test]
    fn slo_id_as_str_stable(id in arb_runtime_slo_id()) {
        prop_assert_eq!(id.as_str(), id.as_str());
    }

    #[test]
    fn slo_id_self_equality(id in arb_runtime_slo_id()) {
        prop_assert_eq!(id, id);
    }
}

// =============================================================================
// SloComparisonOp evaluation properties
// =============================================================================

proptest! {
    #[test]
    fn less_or_equal_reflexive(v in -1e6f64..1e6f64) {
        prop_assert!(SloComparisonOp::LessOrEqual.evaluate(v, v));
    }

    #[test]
    fn less_than_irreflexive(v in -1e6f64..1e6f64) {
        prop_assert!(!SloComparisonOp::LessThan.evaluate(v, v));
    }

    #[test]
    fn greater_or_equal_reflexive(v in -1e6f64..1e6f64) {
        prop_assert!(SloComparisonOp::GreaterOrEqual.evaluate(v, v));
    }

    #[test]
    fn less_or_equal_transitivity(a in -1000.0f64..0.0, b in 0.0f64..1000.0) {
        // a < b always, so a <= b
        prop_assert!(SloComparisonOp::LessOrEqual.evaluate(a, b));
        // and b > a, so b is NOT <= a (unless equal, but ranges exclude that)
        if a < b {
            prop_assert!(!SloComparisonOp::LessOrEqual.evaluate(b, a));
        }
    }

    #[test]
    fn less_than_anti_symmetry(a in -1000.0f64..0.0, b in 0.0001f64..1000.0) {
        // a < 0 < b, so a < b is true, b < a is false
        prop_assert!(SloComparisonOp::LessThan.evaluate(a, b));
        prop_assert!(!SloComparisonOp::LessThan.evaluate(b, a));
    }

    #[test]
    fn greater_or_equal_and_less_or_equal_complement(measured in -100.0f64..100.0, target in -100.0f64..100.0) {
        let le = SloComparisonOp::LessOrEqual.evaluate(measured, target);
        let ge = SloComparisonOp::GreaterOrEqual.evaluate(measured, target);
        // If measured == target, both should be true
        // If measured < target, le=true, ge=false
        // If measured > target, le=false, ge=true
        // So at least one must be true
        prop_assert!(le || ge);
    }
}

// =============================================================================
// RuntimeAlertTier ordering properties
// =============================================================================

proptest! {
    #[test]
    fn alert_tier_info_is_minimum(_dummy in Just(())) {
        prop_assert!(RuntimeAlertTier::Info <= RuntimeAlertTier::Warning);
        prop_assert!(RuntimeAlertTier::Info <= RuntimeAlertTier::Critical);
        prop_assert!(RuntimeAlertTier::Info <= RuntimeAlertTier::Page);
    }

    #[test]
    fn alert_tier_page_is_maximum(_dummy in Just(())) {
        prop_assert!(RuntimeAlertTier::Page >= RuntimeAlertTier::Info);
        prop_assert!(RuntimeAlertTier::Page >= RuntimeAlertTier::Warning);
        prop_assert!(RuntimeAlertTier::Page >= RuntimeAlertTier::Critical);
    }

    #[test]
    fn alert_tier_total_order(a in arb_alert_tier(), b in arb_alert_tier()) {
        prop_assert!(a <= b || b <= a);
    }

    #[test]
    fn alert_tier_reflexive(tier in arb_alert_tier()) {
        prop_assert!(tier <= tier);
        prop_assert!(tier >= tier);
    }
}

// =============================================================================
// AlertPolicy properties
// =============================================================================

proptest! {
    #[test]
    fn standard_policy_known_classes_have_escalation(
        fc in prop_oneof![
            Just(FailureClass::Timeout),
            Just(FailureClass::Overload),
            Just(FailureClass::Deadlock),
            Just(FailureClass::Degraded),
            Just(FailureClass::Corruption),
        ]
    ) {
        let policy = standard_alert_policy();
        prop_assert!(policy.escalation_for(&fc).is_some());
    }

    #[test]
    fn standard_policy_unknown_classes_no_escalation(
        fc in prop_oneof![
            Just(FailureClass::Transient),
            Just(FailureClass::Permanent),
            Just(FailureClass::Panic),
            Just(FailureClass::Safety),
            Just(FailureClass::Configuration),
        ]
    ) {
        let policy = standard_alert_policy();
        prop_assert!(policy.escalation_for(&fc).is_none());
    }

    #[test]
    fn unknown_failure_class_defaults_to_info(fc in arb_failure_class(), breaches in 0..100u32) {
        let policy = standard_alert_policy();
        if policy.escalation_for(&fc).is_none() {
            let tier = policy.effective_tier(&fc, breaches);
            prop_assert_eq!(tier, RuntimeAlertTier::Info);
        }
    }

    #[test]
    fn unknown_failure_class_defaults_to_log_action(fc in arb_failure_class(), breaches in 0..100u32) {
        let policy = standard_alert_policy();
        if policy.escalation_for(&fc).is_none() {
            let actions = policy.effective_actions(&fc, breaches);
            prop_assert_eq!(actions.len(), 1);
            prop_assert_eq!(actions[0], AlertAction::Log);
        }
    }

    #[test]
    fn sustained_tier_gte_initial_tier(
        fc in prop_oneof![
            Just(FailureClass::Timeout),
            Just(FailureClass::Overload),
            Just(FailureClass::Deadlock),
            Just(FailureClass::Degraded),
            Just(FailureClass::Corruption),
        ]
    ) {
        let policy = standard_alert_policy();
        let esc = policy.escalation_for(&fc).unwrap();
        prop_assert!(esc.sustained_tier >= esc.initial_tier);
    }

    #[test]
    fn effective_tier_monotonic_with_breaches(
        fc in prop_oneof![
            Just(FailureClass::Timeout),
            Just(FailureClass::Overload),
            Just(FailureClass::Deadlock),
            Just(FailureClass::Degraded),
            Just(FailureClass::Corruption),
        ],
        low in 0..5u32,
        high in 5..100u32,
    ) {
        let policy = standard_alert_policy();
        let tier_low = policy.effective_tier(&fc, low);
        let tier_high = policy.effective_tier(&fc, high);
        prop_assert!(tier_high >= tier_low);
    }
}

// =============================================================================
// GateReport evaluation properties
// =============================================================================

proptest! {
    #[test]
    fn all_satisfied_is_pass(
        measured_fractions in prop::collection::vec(0.01f64..0.9, 8..=8)
    ) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().zip(measured_fractions.iter()).map(|(s, frac)| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * frac,
                good_count: 999,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        prop_assert_eq!(report.verdict, GateVerdict::Pass);
        prop_assert_eq!(report.breached_count, 0);
    }

    #[test]
    fn critical_breach_is_fail(
        idx in 0..5usize,
    ) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();

        // Find a critical SLO to breach
        let critical_slos: Vec<&RuntimeSlo> = slos.iter().filter(|s| s.critical).collect();
        let target_slo = critical_slos[idx % critical_slos.len()];

        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            if s.id == target_slo.id {
                RuntimeSloSample {
                    slo_id: s.id,
                    measured: s.target * 10.0, // Way over target
                    good_count: 500,
                    total_count: 1000,
                }
            } else {
                RuntimeSloSample {
                    slo_id: s.id,
                    measured: s.target * 0.1,
                    good_count: 999,
                    total_count: 1000,
                }
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        prop_assert_eq!(report.verdict, GateVerdict::Fail);
        prop_assert!(report.critical_breached > 0);
    }

    #[test]
    fn report_counts_consistent(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 0.5,
                good_count: 999,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        prop_assert_eq!(report.satisfied_count + report.breached_count, report.total_slos);
        prop_assert_eq!(report.critical_satisfied + report.critical_breached,
            slos.iter().filter(|s| s.critical).count());
    }

    #[test]
    fn missing_samples_excluded(
        n in 1..4usize,
    ) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().take(n).map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 0.5,
                good_count: 999,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        prop_assert_eq!(report.total_slos, n);
    }

    #[test]
    fn max_alert_tier_none_when_all_pass(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 0.1,
                good_count: 999,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        prop_assert!(report.max_alert_tier.is_none());
    }

    #[test]
    fn budget_remaining_positive_for_good_samples(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 0.1,
                good_count: 999,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        for result in &report.results {
            if result.satisfied {
                // Budget should be non-negative for satisfied SLOs
                prop_assert!(result.budget_remaining >= 0.0);
            }
        }
    }

    #[test]
    fn budget_remaining_zero_for_bad_samples(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 10.0,
                good_count: 100,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        for result in &report.results {
            if !result.satisfied {
                prop_assert!(result.budget_remaining.abs() < f64::EPSILON);
            }
        }
    }

    #[test]
    fn zero_total_count_gives_full_budget(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 0.1,
                good_count: 0,
                total_count: 0,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        for result in &report.results {
            prop_assert!((result.budget_remaining - 1.0).abs() < f64::EPSILON);
        }
    }
}

// =============================================================================
// to_slo_definitions properties
// =============================================================================

proptest! {
    #[test]
    fn to_slo_definitions_preserves_count(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let defs = GateReport::to_slo_definitions(&slos);
        prop_assert_eq!(defs.len(), slos.len());
    }

    #[test]
    fn to_slo_definitions_all_runtime_subsystem(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let defs = GateReport::to_slo_definitions(&slos);
        for d in &defs {
            prop_assert_eq!(&d.subsystem, "runtime");
        }
    }

    #[test]
    fn to_slo_definitions_ids_match_slo_ids(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let defs = GateReport::to_slo_definitions(&slos);
        for (slo, def) in slos.iter().zip(defs.iter()) {
            prop_assert_eq!(slo.id.as_str(), def.id.as_str());
        }
    }

    #[test]
    fn to_slo_definitions_targets_match(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let defs = GateReport::to_slo_definitions(&slos);
        for (slo, def) in slos.iter().zip(defs.iter()) {
            prop_assert!((slo.target - def.target).abs() < 1e-10);
        }
    }
}

// =============================================================================
// Standard data properties
// =============================================================================

proptest! {
    #[test]
    fn standard_slos_all_unique_ids(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        for (i, a) in slos.iter().enumerate() {
            for (j, b) in slos.iter().enumerate() {
                if i != j {
                    prop_assert_ne!(a.id.as_str(), b.id.as_str());
                }
            }
        }
    }

    #[test]
    fn standard_slos_cover_all_variants(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let all = RuntimeSloId::all();
        prop_assert_eq!(slos.len(), all.len());
        for id in all {
            let found = slos.iter().any(|s| s.id == *id);
            prop_assert!(found);
        }
    }

    #[test]
    fn standard_slos_positive_targets(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        for s in &slos {
            prop_assert!(s.target > 0.0);
        }
    }

    #[test]
    fn standard_slos_valid_error_budgets(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        for s in &slos {
            prop_assert!(s.error_budget >= 0.0);
            prop_assert!(s.error_budget <= 1.0);
        }
    }

    #[test]
    fn standard_policy_has_entries(_dummy in Just(())) {
        let policy = standard_alert_policy();
        prop_assert!(policy.escalation_map.len() >= 5);
    }
}

// =============================================================================
// Render summary properties
// =============================================================================

proptest! {
    #[test]
    fn render_summary_not_empty(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 0.5,
                good_count: 999,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        let summary = report.render_summary();
        prop_assert!(!summary.is_empty());
        prop_assert!(summary.contains("Runtime SLO Gate"));
    }

    #[test]
    fn render_summary_reflects_verdict(_dummy in Just(())) {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos.iter().map(|s| {
            RuntimeSloSample {
                slo_id: s.id,
                measured: s.target * 0.5,
                good_count: 999,
                total_count: 1000,
            }
        }).collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        let summary = report.render_summary();
        prop_assert!(summary.contains("Pass"));
        prop_assert!(summary.contains("OK"));
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn runtime_slo_id_all_length() {
    assert_eq!(RuntimeSloId::all().len(), 8);
}

#[test]
fn non_critical_only_breach_is_conditional_pass() {
    let slos = standard_runtime_slos();
    let policy = standard_alert_policy();

    let mut samples: Vec<RuntimeSloSample> = slos
        .iter()
        .map(|s| RuntimeSloSample {
            slo_id: s.id,
            measured: s.target * 0.5,
            good_count: 999,
            total_count: 1000,
        })
        .collect();

    // Breach only QueueBacklogDepth (non-critical)
    if let Some(s) = samples
        .iter_mut()
        .find(|s| s.slo_id == RuntimeSloId::QueueBacklogDepth)
    {
        s.measured = 2000.0;
    }

    let report = GateReport::evaluate(&slos, &samples, &policy);
    assert_eq!(report.verdict, GateVerdict::ConditionalPass);
}

#[test]
fn alert_tier_to_slo_severity_mapping() {
    use frankenterm_core::slo_conformance::SloSeverity;

    assert_eq!(RuntimeAlertTier::Info.to_slo_severity(), SloSeverity::Info);
    assert_eq!(
        RuntimeAlertTier::Warning.to_slo_severity(),
        SloSeverity::Warning
    );
    assert_eq!(
        RuntimeAlertTier::Critical.to_slo_severity(),
        SloSeverity::Critical
    );
    assert_eq!(RuntimeAlertTier::Page.to_slo_severity(), SloSeverity::Page);
}
