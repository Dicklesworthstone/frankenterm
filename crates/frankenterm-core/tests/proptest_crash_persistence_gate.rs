//! Property tests for crash_persistence_gate module.

use proptest::prelude::*;

use frankenterm_core::crash_persistence_gate::*;

// =============================================================================
// Strategy helpers
// =============================================================================

fn arb_persistence_invariant_id() -> impl Strategy<Value = PersistenceInvariantId> {
    prop_oneof![
        Just(PersistenceInvariantId::CaptureFlush),
        Just(PersistenceInvariantId::SessionCheckpoint),
        Just(PersistenceInvariantId::SearchIndexSync),
        Just(PersistenceInvariantId::EventQueueDrain),
        Just(PersistenceInvariantId::ControlPlaneAck),
        Just(PersistenceInvariantId::WalFsync),
        Just(PersistenceInvariantId::TransactionAtomicity),
    ]
}

fn arb_crash_scenario_type() -> impl Strategy<Value = CrashScenarioType> {
    prop_oneof![
        Just(CrashScenarioType::CleanShutdown),
        Just(CrashScenarioType::Sigkill),
        Just(CrashScenarioType::PartialWrite),
        Just(CrashScenarioType::RestartLoop),
        Just(CrashScenarioType::IoFaultDuringCheckpoint),
        Just(CrashScenarioType::DiskFull),
        Just(CrashScenarioType::CorruptedCheckpoint),
    ]
}

fn arb_recovery_outcome() -> impl Strategy<Value = RecoveryOutcome> {
    prop_oneof![
        Just(RecoveryOutcome::FullRecovery),
        Just(RecoveryOutcome::PartialRecovery),
        Just(RecoveryOutcome::DegradedRecovery),
        Just(RecoveryOutcome::GracefulFailure),
    ]
}

fn arb_gate_verdict() -> impl Strategy<Value = PersistenceGateVerdict> {
    prop_oneof![
        Just(PersistenceGateVerdict::Pass),
        Just(PersistenceGateVerdict::ConditionalPass),
        Just(PersistenceGateVerdict::Fail),
    ]
}

fn arb_invariant_result() -> impl Strategy<Value = InvariantResult> {
    (arb_persistence_invariant_id(), any::<bool>(), ".*", any::<bool>()).prop_map(
        |(id, held, evidence, data_loss)| InvariantResult {
            invariant_id: id,
            held,
            evidence,
            data_loss,
        },
    )
}

fn arb_scenario_result() -> impl Strategy<Value = ScenarioResult> {
    (
        "[A-Z]{4}-[0-9]{3}",
        arb_crash_scenario_type(),
        any::<bool>(),
        arb_recovery_outcome(),
        arb_recovery_outcome(),
        prop::collection::vec(arb_invariant_result(), 0..8),
        0..100_000u64,
    )
        .prop_map(
            |(scenario_id, crash_type, met, actual, expected, inv_results, recovery_ms)| {
                ScenarioResult {
                    scenario_id,
                    crash_type,
                    met_expectation: met,
                    actual_outcome: actual,
                    expected_outcome: expected,
                    invariant_results: inv_results,
                    recovery_time_ms: recovery_ms,
                }
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_persistence_invariant_id(id in arb_persistence_invariant_id()) {
        let json = serde_json::to_string(&id).unwrap();
        let restored: PersistenceInvariantId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(id, restored);
    }

    #[test]
    fn serde_roundtrip_crash_scenario_type(ct in arb_crash_scenario_type()) {
        let json = serde_json::to_string(&ct).unwrap();
        let restored: CrashScenarioType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ct, restored);
    }

    #[test]
    fn serde_roundtrip_recovery_outcome(outcome in arb_recovery_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let restored: RecoveryOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, restored);
    }

    #[test]
    fn serde_roundtrip_gate_verdict(v in arb_gate_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let restored: PersistenceGateVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, restored);
    }

    #[test]
    fn serde_roundtrip_invariant_result(ir in arb_invariant_result()) {
        let json = serde_json::to_string(&ir).unwrap();
        let restored: InvariantResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ir.invariant_id, restored.invariant_id);
        prop_assert_eq!(ir.held, restored.held);
        prop_assert_eq!(ir.data_loss, restored.data_loss);
    }

    #[test]
    fn serde_roundtrip_scenario_result(sr in arb_scenario_result()) {
        let json = serde_json::to_string(&sr).unwrap();
        let restored: ScenarioResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&sr.scenario_id, &restored.scenario_id);
        prop_assert_eq!(sr.crash_type, restored.crash_type);
        prop_assert_eq!(sr.met_expectation, restored.met_expectation);
        prop_assert_eq!(sr.recovery_time_ms, restored.recovery_time_ms);
        prop_assert_eq!(sr.invariant_results.len(), restored.invariant_results.len());
    }

    #[test]
    fn serde_roundtrip_persistence_gate_report(results in prop::collection::vec(arb_scenario_result(), 0..6)) {
        let report = PersistenceGateReport::evaluate(results);
        let json = serde_json::to_string(&report).unwrap();
        let restored: PersistenceGateReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.verdict, restored.verdict);
        prop_assert_eq!(report.total_scenarios, restored.total_scenarios);
        prop_assert_eq!(report.any_data_loss, restored.any_data_loss);
    }
}

// =============================================================================
// PersistenceInvariantId invariants
// =============================================================================

proptest! {
    #[test]
    fn invariant_id_as_str_not_empty(id in arb_persistence_invariant_id()) {
        prop_assert!(!id.as_str().is_empty());
    }

    #[test]
    fn invariant_id_as_str_starts_with_persist(id in arb_persistence_invariant_id()) {
        prop_assert!(id.as_str().starts_with("PERSIST-"));
    }

    #[test]
    fn invariant_id_self_equality(id in arb_persistence_invariant_id()) {
        prop_assert_eq!(id, id);
    }
}

// =============================================================================
// CrashScenarioType invariants
// =============================================================================

proptest! {
    #[test]
    fn crash_scenario_type_label_not_empty(ct in arb_crash_scenario_type()) {
        prop_assert!(!ct.label().is_empty());
    }

    #[test]
    fn crash_scenario_type_label_stable(ct in arb_crash_scenario_type()) {
        prop_assert_eq!(ct.label(), ct.label());
    }
}

// =============================================================================
// ScenarioResult properties
// =============================================================================

proptest! {
    #[test]
    fn has_data_loss_requires_unheld_and_data_loss(
        inv_results in prop::collection::vec(arb_invariant_result(), 0..10)
    ) {
        let result = ScenarioResult {
            scenario_id: "test".into(),
            crash_type: CrashScenarioType::Sigkill,
            met_expectation: false,
            actual_outcome: RecoveryOutcome::PartialRecovery,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: inv_results.clone(),
            recovery_time_ms: 100,
        };

        let has_dl = result.has_data_loss();
        let expected_dl = inv_results.iter().any(|r| !r.held && r.data_loss);
        prop_assert_eq!(has_dl, expected_dl);
    }

    #[test]
    fn invariants_held_count_bounded(
        inv_results in prop::collection::vec(arb_invariant_result(), 0..10)
    ) {
        let result = ScenarioResult {
            scenario_id: "test".into(),
            crash_type: CrashScenarioType::CleanShutdown,
            met_expectation: true,
            actual_outcome: RecoveryOutcome::FullRecovery,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: inv_results.clone(),
            recovery_time_ms: 50,
        };

        prop_assert!(result.invariants_held() <= inv_results.len());
    }

    #[test]
    fn all_held_means_no_data_loss(
        ids in prop::collection::vec(arb_persistence_invariant_id(), 1..6)
    ) {
        let inv_results: Vec<InvariantResult> = ids.into_iter().map(|id| InvariantResult {
            invariant_id: id,
            held: true,
            evidence: "ok".into(),
            data_loss: true,
        }).collect();

        let result = ScenarioResult {
            scenario_id: "test".into(),
            crash_type: CrashScenarioType::Sigkill,
            met_expectation: true,
            actual_outcome: RecoveryOutcome::FullRecovery,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: inv_results,
            recovery_time_ms: 0,
        };

        prop_assert!(!result.has_data_loss());
    }

    #[test]
    fn no_data_loss_flag_means_no_data_loss_even_when_unheld(
        ids in prop::collection::vec(arb_persistence_invariant_id(), 1..6)
    ) {
        let inv_results: Vec<InvariantResult> = ids.into_iter().map(|id| InvariantResult {
            invariant_id: id,
            held: false,
            evidence: "failed".into(),
            data_loss: false,
        }).collect();

        let result = ScenarioResult {
            scenario_id: "test".into(),
            crash_type: CrashScenarioType::PartialWrite,
            met_expectation: false,
            actual_outcome: RecoveryOutcome::DegradedRecovery,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: inv_results,
            recovery_time_ms: 0,
        };

        prop_assert!(!result.has_data_loss());
    }
}

// =============================================================================
// PersistenceGateReport evaluation properties
// =============================================================================

proptest! {
    #[test]
    fn report_counts_consistent(results in prop::collection::vec(arb_scenario_result(), 0..8)) {
        let n = results.len();
        let report = PersistenceGateReport::evaluate(results);
        prop_assert_eq!(report.total_scenarios, n);
        prop_assert_eq!(report.passed_scenarios + report.failed_scenarios, n);
    }

    #[test]
    fn empty_results_pass(results in Just(vec![])) {
        let report = PersistenceGateReport::evaluate(results);
        prop_assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
        prop_assert!(!report.any_data_loss);
        prop_assert_eq!(report.max_recovery_time_ms, 0);
    }

    #[test]
    fn all_met_no_data_loss_is_pass(
        crash_types in prop::collection::vec(arb_crash_scenario_type(), 1..5)
    ) {
        let results: Vec<ScenarioResult> = crash_types.into_iter().enumerate().map(|(i, ct)| {
            ScenarioResult {
                scenario_id: format!("PASS-{}", i),
                crash_type: ct,
                met_expectation: true,
                actual_outcome: RecoveryOutcome::FullRecovery,
                expected_outcome: RecoveryOutcome::FullRecovery,
                invariant_results: vec![InvariantResult {
                    invariant_id: PersistenceInvariantId::CaptureFlush,
                    held: true,
                    evidence: "ok".into(),
                    data_loss: true,
                }],
                recovery_time_ms: 100,
            }
        }).collect();

        let report = PersistenceGateReport::evaluate(results);
        prop_assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
    }

    #[test]
    fn data_loss_always_fail(
        recovery_ms in 0..100_000u64,
        ct in arb_crash_scenario_type(),
    ) {
        let results = vec![ScenarioResult {
            scenario_id: "DL".into(),
            crash_type: ct,
            met_expectation: false,
            actual_outcome: RecoveryOutcome::GracefulFailure,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: vec![InvariantResult {
                invariant_id: PersistenceInvariantId::CaptureFlush,
                held: false,
                evidence: "lost".into(),
                data_loss: true,
            }],
            recovery_time_ms: recovery_ms,
        }];

        let report = PersistenceGateReport::evaluate(results);
        prop_assert_eq!(report.verdict, PersistenceGateVerdict::Fail);
        prop_assert!(report.any_data_loss);
    }

    #[test]
    fn non_critical_failure_is_conditional(
        ct in arb_crash_scenario_type(),
    ) {
        // One passing, one failing on non-data-loss invariant
        let results = vec![
            ScenarioResult {
                scenario_id: "OK".into(),
                crash_type: CrashScenarioType::CleanShutdown,
                met_expectation: true,
                actual_outcome: RecoveryOutcome::FullRecovery,
                expected_outcome: RecoveryOutcome::FullRecovery,
                invariant_results: vec![],
                recovery_time_ms: 10,
            },
            ScenarioResult {
                scenario_id: "NONCRIT".into(),
                crash_type: ct,
                met_expectation: false,
                actual_outcome: RecoveryOutcome::DegradedRecovery,
                expected_outcome: RecoveryOutcome::FullRecovery,
                invariant_results: vec![InvariantResult {
                    invariant_id: PersistenceInvariantId::SearchIndexSync,
                    held: false,
                    evidence: "index stale".into(),
                    data_loss: false,
                }],
                recovery_time_ms: 200,
            },
        ];

        let report = PersistenceGateReport::evaluate(results);
        prop_assert_eq!(report.verdict, PersistenceGateVerdict::ConditionalPass);
    }

    #[test]
    fn max_recovery_time_tracked(
        times in prop::collection::vec(0..50_000u64, 1..6)
    ) {
        let expected_max = *times.iter().max().unwrap();
        let results: Vec<ScenarioResult> = times.into_iter().enumerate().map(|(i, t)| {
            ScenarioResult {
                scenario_id: format!("T-{}", i),
                crash_type: CrashScenarioType::CleanShutdown,
                met_expectation: true,
                actual_outcome: RecoveryOutcome::FullRecovery,
                expected_outcome: RecoveryOutcome::FullRecovery,
                invariant_results: vec![],
                recovery_time_ms: t,
            }
        }).collect();

        let report = PersistenceGateReport::evaluate(results);
        prop_assert_eq!(report.max_recovery_time_ms, expected_max);
    }

    #[test]
    fn any_data_loss_iff_some_scenario_has_data_loss(
        results in prop::collection::vec(arb_scenario_result(), 1..6)
    ) {
        let expected = results.iter().any(|r| r.has_data_loss());
        let report = PersistenceGateReport::evaluate(results);
        prop_assert_eq!(report.any_data_loss, expected);
    }
}

// =============================================================================
// Standard data properties
// =============================================================================

proptest! {
    #[test]
    fn standard_invariants_all_unique_ids(_dummy in Just(())) {
        let invariants = standard_invariants();
        let ids: Vec<&str> = invariants.iter().map(|i| i.id.as_str()).collect();
        for (i, a) in ids.iter().enumerate() {
            for (j, b) in ids.iter().enumerate() {
                if i != j {
                    prop_assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn standard_invariants_cover_all_enum_variants(_dummy in Just(())) {
        let invariants = standard_invariants();
        let all = PersistenceInvariantId::all();
        prop_assert_eq!(invariants.len(), all.len());
        for id in all {
            let found = invariants.iter().any(|i| i.id == *id);
            prop_assert!(found);
        }
    }

    #[test]
    fn standard_scenarios_cover_all_crash_types(_dummy in Just(())) {
        let scenarios = standard_recovery_scenarios();
        let all = CrashScenarioType::all();
        for ct in all {
            let found = scenarios.iter().any(|s| s.crash_type == *ct);
            prop_assert!(found);
        }
    }

    #[test]
    fn standard_scenarios_all_have_invariants(_dummy in Just(())) {
        let scenarios = standard_recovery_scenarios();
        for s in &scenarios {
            prop_assert!(!s.validates_invariants.is_empty());
        }
    }

    #[test]
    fn standard_invariants_have_descriptions(_dummy in Just(())) {
        let invariants = standard_invariants();
        for inv in &invariants {
            prop_assert!(!inv.description.is_empty());
            prop_assert!(!inv.protected_resource.is_empty());
            prop_assert!(!inv.verification_method.is_empty());
        }
    }
}

// =============================================================================
// Render summary properties
// =============================================================================

proptest! {
    #[test]
    fn render_summary_not_empty(results in prop::collection::vec(arb_scenario_result(), 0..5)) {
        let report = PersistenceGateReport::evaluate(results);
        let summary = report.render_summary();
        prop_assert!(!summary.is_empty());
        prop_assert!(summary.contains("Crash Persistence Gate"));
    }

    #[test]
    fn render_summary_contains_verdict(results in prop::collection::vec(arb_scenario_result(), 0..5)) {
        let report = PersistenceGateReport::evaluate(results);
        let summary = report.render_summary();
        let has_verdict = summary.contains("Pass")
            || summary.contains("ConditionalPass")
            || summary.contains("Fail");
        prop_assert!(has_verdict);
    }

    #[test]
    fn render_summary_data_loss_marker(
        ct in arb_crash_scenario_type(),
    ) {
        let results = vec![ScenarioResult {
            scenario_id: "DLTEST".into(),
            crash_type: ct,
            met_expectation: false,
            actual_outcome: RecoveryOutcome::GracefulFailure,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: vec![InvariantResult {
                invariant_id: PersistenceInvariantId::WalFsync,
                held: false,
                evidence: "lost".into(),
                data_loss: true,
            }],
            recovery_time_ms: 999,
        }];

        let report = PersistenceGateReport::evaluate(results);
        let summary = report.render_summary();
        prop_assert!(summary.contains("DATA LOSS"));
    }
}

// =============================================================================
// Unit tests for edge cases
// =============================================================================

#[test]
fn persistence_invariant_id_all_length() {
    assert_eq!(PersistenceInvariantId::all().len(), 7);
}

#[test]
fn crash_scenario_type_all_length() {
    assert_eq!(CrashScenarioType::all().len(), 7);
}

#[test]
fn report_with_zero_scenarios_passes() {
    let report = PersistenceGateReport::evaluate(vec![]);
    assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
    assert_eq!(report.total_scenarios, 0);
    assert_eq!(report.max_recovery_time_ms, 0);
}
