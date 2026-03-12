//! Property tests for dual_run_shadow_comparator module.
//!
//! Covers serde roundtrips for all enum types with PartialEq, DriftSeverity
//! ordering, comparison workflow invariants (match/divergence detection,
//! triage lifecycle, gate evaluation consistency), and telemetry counting.

use frankenterm_core::dual_run_shadow_comparator::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_dual_run_priority() -> impl Strategy<Value = DualRunPriority> {
    prop_oneof![
        Just(DualRunPriority::Blocking),
        Just(DualRunPriority::High),
        Just(DualRunPriority::Medium),
        Just(DualRunPriority::Low),
    ]
}

fn arb_match_status() -> impl Strategy<Value = MatchStatus> {
    prop_oneof![
        Just(MatchStatus::Match),
        Just(MatchStatus::IntentionalDelta),
        Just(MatchStatus::Divergence),
        Just(MatchStatus::ExecutionFailure),
        Just(MatchStatus::Inconclusive),
    ]
}

fn arb_drift_category() -> impl Strategy<Value = DriftCategory> {
    prop_oneof![
        Just(DriftCategory::Behavioral),
        Just(DriftCategory::Format),
        Just(DriftCategory::Performance),
        Just(DriftCategory::FeatureGap),
        Just(DriftCategory::IntentionalImprovement),
        Just(DriftCategory::Infrastructure),
        Just(DriftCategory::Timing),
    ]
}

fn arb_drift_severity() -> impl Strategy<Value = DriftSeverity> {
    prop_oneof![
        Just(DriftSeverity::Info),
        Just(DriftSeverity::Low),
        Just(DriftSeverity::Medium),
        Just(DriftSeverity::High),
        Just(DriftSeverity::Critical),
    ]
}

fn arb_drift_action() -> impl Strategy<Value = DriftAction> {
    prop_oneof![
        Just(DriftAction::Accept),
        Just(DriftAction::Document),
        Just(DriftAction::FixFt),
        Just(DriftAction::AcknowledgeImprovement),
        Just(DriftAction::Investigate),
        Just(DriftAction::AdjustTest),
        Just(DriftAction::DeferPostCutover),
    ]
}

fn arb_resolution_status() -> impl Strategy<Value = ResolutionStatus> {
    prop_oneof![
        Just(ResolutionStatus::Untriaged),
        Just(ResolutionStatus::Triaged),
        Just(ResolutionStatus::InProgress),
        Just(ResolutionStatus::Resolved),
        Just(ResolutionStatus::Accepted),
        Just(ResolutionStatus::Deferred),
    ]
}

fn arb_cutover_decision() -> impl Strategy<Value = CutoverDecision> {
    prop_oneof![
        Just(CutoverDecision::Go),
        Just(CutoverDecision::NoGo),
        Just(CutoverDecision::ReviewRequired),
    ]
}

fn arb_triage_telemetry() -> impl Strategy<Value = TriageTelemetry> {
    (0u64..1000, 0u64..500, 0u64..500, 0u64..500, 0u64..100).prop_map(
        |(comp, div, triaged, resolved, gate)| TriageTelemetry {
            comparisons_performed: comp,
            divergences_found: div,
            items_triaged: triaged,
            items_resolved: resolved,
            gate_evaluations: gate,
        },
    )
}

/// Build a RunCapture with configurable content.
fn make_capture(system: &str, stdout: &str, exit_code: i32, duration_ms: u64) -> RunCapture {
    RunCapture {
        system: system.to_string(),
        command: "ft test".to_string(),
        exit_code: Some(exit_code),
        stdout: stdout.to_string(),
        stderr: String::new(),
        duration_ms,
        completed: true,
        error: None,
    }
}

/// Build a DualRunResult for property testing.
fn make_dual(
    scenario_id: &str,
    ntm_stdout: &str,
    ft_stdout: &str,
    ntm_exit: i32,
    ft_exit: i32,
    ntm_duration: u64,
    ft_duration: u64,
    priority: DualRunPriority,
) -> DualRunResult {
    DualRunResult {
        scenario_id: scenario_id.to_string(),
        domain: "test".to_string(),
        priority,
        ntm: make_capture("ntm", ntm_stdout, ntm_exit, ntm_duration),
        ft: make_capture("ft", ft_stdout, ft_exit, ft_duration),
        compared_at_ms: 1000,
        correlation_id: None,
    }
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_dual_run_priority(p in arb_dual_run_priority()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: DualRunPriority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, back);
    }

    #[test]
    fn serde_roundtrip_match_status(s in arb_match_status()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: MatchStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn serde_roundtrip_drift_category(c in arb_drift_category()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: DriftCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c, back);
    }

    #[test]
    fn serde_roundtrip_drift_severity(s in arb_drift_severity()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: DriftSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn serde_roundtrip_drift_action(a in arb_drift_action()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: DriftAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }

    #[test]
    fn serde_roundtrip_resolution_status(s in arb_resolution_status()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: ResolutionStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn serde_roundtrip_cutover_decision(d in arb_cutover_decision()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: CutoverDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    #[test]
    fn serde_roundtrip_triage_telemetry(t in arb_triage_telemetry()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: TriageTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t.comparisons_performed, back.comparisons_performed);
        prop_assert_eq!(t.divergences_found, back.divergences_found);
        prop_assert_eq!(t.items_triaged, back.items_triaged);
        prop_assert_eq!(t.items_resolved, back.items_resolved);
        prop_assert_eq!(t.gate_evaluations, back.gate_evaluations);
    }
}

// =============================================================================
// DriftSeverity ordering
// =============================================================================

proptest! {
    #[test]
    fn drift_severity_total_order(a in arb_drift_severity(), b in arb_drift_severity()) {
        if a <= b && b <= a {
            prop_assert_eq!(a, b);
        }
    }

    #[test]
    fn drift_severity_info_is_minimum(s in arb_drift_severity()) {
        prop_assert!(DriftSeverity::Info <= s);
    }

    #[test]
    fn drift_severity_critical_is_maximum(s in arb_drift_severity()) {
        prop_assert!(s <= DriftSeverity::Critical);
    }
}

// =============================================================================
// DualRunPriority ordering
// =============================================================================

proptest! {
    #[test]
    fn priority_blocking_is_minimum(p in arb_dual_run_priority()) {
        prop_assert!(DualRunPriority::Blocking <= p);
    }

    #[test]
    fn priority_low_is_maximum(p in arb_dual_run_priority()) {
        prop_assert!(p <= DualRunPriority::Low);
    }
}

// =============================================================================
// Comparison workflow invariants
// =============================================================================

proptest! {
    #[test]
    fn matching_outputs_produce_match_status(
        exit_code in 0..5i32,
        duration in 10..500u64,
        output in "[a-z ]{0,50}",
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual("s1", &output, &output, exit_code, exit_code, duration, duration, DualRunPriority::Blocking);
        let verdict = wf.compare(&dual);
        prop_assert_eq!(verdict.match_status, MatchStatus::Match,
            "identical outputs must produce Match status");
        prop_assert!(verdict.semantic.exit_code_match);
        prop_assert!(verdict.semantic.stdout_match);
        prop_assert!(verdict.semantic.stderr_match);
        prop_assert!(verdict.divergences.is_empty());
    }

    #[test]
    fn different_exit_codes_produce_divergence(
        ntm_exit in 0..3i32,
        ft_exit in 3..6i32,
        output in "[a-z]{3,20}",
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual("s-exit", &output, &output, ntm_exit, ft_exit, 100, 100, DualRunPriority::Blocking);
        let verdict = wf.compare(&dual);
        prop_assert!(!verdict.semantic.exit_code_match);
        prop_assert!(verdict.divergences.iter().any(|d| d.field == "exit_code"));
    }

    #[test]
    fn different_stdout_blocking_is_divergence(
        ntm_out in "[a-z]{5,20}",
        ft_out in "[A-Z]{5,20}",
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual("s-stdout", &ntm_out, &ft_out, 0, 0, 100, 100, DualRunPriority::Blocking);
        let verdict = wf.compare(&dual);
        // If stdout differs at all, there should be divergences
        if ntm_out.trim_end() != ft_out.trim_end() {
            prop_assert!(!verdict.divergences.is_empty(),
                "different stdout must produce divergences");
            prop_assert_eq!(verdict.match_status, MatchStatus::Divergence,
                "blocking stdout mismatch must be Divergence");
        }
    }

    #[test]
    fn assertion_match_rate_bounded(
        ntm_out in "[a-z]{0,20}",
        ft_out in "[a-z]{0,20}",
        ntm_exit in 0..3i32,
        ft_exit in 0..3i32,
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual("s-rate", &ntm_out, &ft_out, ntm_exit, ft_exit, 100, 100, DualRunPriority::Low);
        let verdict = wf.compare(&dual);
        prop_assert!(verdict.semantic.assertion_match_rate >= 0.0);
        prop_assert!(verdict.semantic.assertion_match_rate <= 1.0);
    }

    #[test]
    fn perfect_match_has_assertion_rate_1(
        output in "[a-z]{3,20}",
        exit_code in 0..5i32,
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual("s-perfect", &output, &output, exit_code, exit_code, 100, 100, DualRunPriority::Blocking);
        let verdict = wf.compare(&dual);
        prop_assert!((verdict.semantic.assertion_match_rate - 1.0).abs() < 0.001,
            "perfect match must have assertion_match_rate == 1.0");
    }
}

// =============================================================================
// Performance comparison properties
// =============================================================================

proptest! {
    #[test]
    fn performance_delta_correct(
        ntm_ms in 10..5000u64,
        ft_ms in 10..5000u64,
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual("perf", "out", "out", 0, 0, ntm_ms, ft_ms, DualRunPriority::Low);
        let verdict = wf.compare(&dual);
        let expected_delta = ft_ms as i64 - ntm_ms as i64;
        prop_assert_eq!(verdict.performance.delta_ms, expected_delta);
        prop_assert_eq!(verdict.performance.ntm_duration_ms, ntm_ms);
        prop_assert_eq!(verdict.performance.ft_duration_ms, ft_ms);
    }

    #[test]
    fn performance_within_threshold_correct(
        ntm_ms in 100..200u64,
        ft_ms in 100..200u64,
    ) {
        let mut wf = DriftTriageWorkflow::new(500, 0, 3); // 500ms threshold
        let dual = make_dual("thresh", "out", "out", 0, 0, ntm_ms, ft_ms, DualRunPriority::Low);
        let verdict = wf.compare(&dual);
        // Delta is at most 100ms which is < 500ms threshold
        prop_assert!(verdict.performance.within_threshold,
            "delta of {}ms should be within 500ms threshold", verdict.performance.delta_ms);
    }
}

// =============================================================================
// Triage workflow lifecycle
// =============================================================================

proptest! {
    #[test]
    fn triage_item_lookup_after_add(
        n_scenarios in 1..5usize,
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let mut all_ids = Vec::new();

        for i in 0..n_scenarios {
            let dual = make_dual(
                &format!("s-{i}"),
                "ntm-output",
                "ft-different-output",
                0, 0, 100, 100,
                DualRunPriority::High,
            );
            let verdict = wf.compare(&dual);
            let ids = wf.classify_and_add(&format!("s-{i}"), &verdict, 1000);
            all_ids.extend(ids);
        }

        // All IDs must be retrievable
        for &id in &all_ids {
            prop_assert!(wf.get_item(id).is_some(),
                "item {} must be retrievable after add", id);
        }
        prop_assert_eq!(wf.len(), all_ids.len());
    }

    #[test]
    fn triage_then_resolve_lifecycle(
        exit_code_diff in 1..5i32,
    ) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual("lifecycle", "out", "out", 0, exit_code_diff, 100, 100, DualRunPriority::High);
        let verdict = wf.compare(&dual);
        let ids = wf.classify_and_add("lifecycle", &verdict, 1000);

        prop_assert!(!ids.is_empty(), "divergence should produce triage items");

        for &id in &ids {
            // Initially untriaged
            let item = wf.get_item(id).unwrap();
            prop_assert_eq!(item.resolution_status, ResolutionStatus::Untriaged);

            // Triage
            let ok = wf.triage_item(id, Some("test-agent".into()), "investigating");
            prop_assert!(ok);
            let item = wf.get_item(id).unwrap();
            prop_assert_eq!(item.resolution_status, ResolutionStatus::Triaged);

            // Resolve
            let ok = wf.resolve_item(id, ResolutionStatus::Resolved, "fixed");
            prop_assert!(ok);
            let item = wf.get_item(id).unwrap();
            prop_assert_eq!(item.resolution_status, ResolutionStatus::Resolved);
        }
    }

    #[test]
    fn empty_workflow_gate_is_go(_dummy in 0..1u32) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let gate = wf.evaluate_cutover_gate();
        prop_assert_eq!(gate.decision, CutoverDecision::Go,
            "empty workflow must produce Go decision");
        prop_assert!(gate.gate_checks.iter().all(|g| g.passed));
    }
}

// =============================================================================
// Telemetry counting
// =============================================================================

proptest! {
    #[test]
    fn telemetry_comparisons_counted(n in 1..5usize) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        for i in 0..n {
            let dual = make_dual(&format!("t-{i}"), "out", "out", 0, 0, 100, 100, DualRunPriority::Low);
            wf.compare(&dual);
        }
        prop_assert_eq!(wf.telemetry().comparisons_performed, n as u64);
    }

    #[test]
    fn telemetry_gate_evaluations_counted(n in 1..5usize) {
        let mut wf = DriftTriageWorkflow::with_defaults();
        for _ in 0..n {
            wf.evaluate_cutover_gate();
        }
        prop_assert_eq!(wf.telemetry().gate_evaluations, n as u64);
    }
}

// =============================================================================
// Default construction
// =============================================================================

#[test]
fn triage_telemetry_default_is_zeroed() {
    let t = TriageTelemetry::default();
    assert_eq!(t.comparisons_performed, 0);
    assert_eq!(t.divergences_found, 0);
    assert_eq!(t.items_triaged, 0);
    assert_eq!(t.items_resolved, 0);
    assert_eq!(t.gate_evaluations, 0);
}

#[test]
fn workflow_with_defaults_starts_empty() {
    let wf = DriftTriageWorkflow::with_defaults();
    assert!(wf.is_empty());
    assert_eq!(wf.len(), 0);
}
