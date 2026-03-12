//! Integration tests for crash/restart persistence invariants and recovery
//! replay gate (ft-e34d9.10.6.6).
//!
//! Validates end-to-end crash-consistency and restart recovery semantics
//! across `crash_persistence_gate`, `durable_state`, and `crash` modules.
//!
//! Test categories:
//! 1. Standard invariant coverage validation
//! 2. Gate evaluation across all crash scenarios
//! 3. Durable state checkpoint→crash→rollback recovery
//! 4. Persistence gate report rendering and serde
//! 5. Failure injection and data loss detection
//! 6. Recovery time tracking
//! 7. Cross-module contract validation

use std::collections::HashMap;

use frankenterm_core::crash_persistence_gate::{
    CrashScenarioType, InvariantResult, PersistenceGateReport, PersistenceGateVerdict,
    PersistenceInvariantId, RecoveryOutcome, ScenarioResult, standard_invariants,
    standard_recovery_scenarios,
};
use frankenterm_core::durable_state::{CheckpointTrigger, DurableStateManager};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState,
};

// =========================================================================
// Helper: create a scenario result
// =========================================================================

fn make_scenario_result(
    scenario_id: &str,
    crash_type: CrashScenarioType,
    met_expectation: bool,
    actual_outcome: RecoveryOutcome,
    expected_outcome: RecoveryOutcome,
    invariant_results: Vec<InvariantResult>,
    recovery_time_ms: u64,
) -> ScenarioResult {
    ScenarioResult {
        scenario_id: scenario_id.into(),
        crash_type,
        met_expectation,
        actual_outcome,
        expected_outcome,
        invariant_results,
        recovery_time_ms,
    }
}

fn invariant_held(id: PersistenceInvariantId, data_loss: bool) -> InvariantResult {
    InvariantResult {
        invariant_id: id,
        held: true,
        evidence: format!("{:?} verified: state consistent after recovery", id),
        data_loss,
    }
}

fn invariant_failed(id: PersistenceInvariantId, data_loss: bool) -> InvariantResult {
    InvariantResult {
        invariant_id: id,
        held: false,
        evidence: format!("{:?} FAILED: state inconsistent after recovery", id),
        data_loss,
    }
}

fn make_pane_identity(workspace: &str, pane_id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, workspace, "default", pane_id, 0)
}

// =========================================================================
// 1. Standard invariant coverage
// =========================================================================

#[test]
fn standard_invariants_cover_all_ids() {
    let invariants = standard_invariants();
    let all_ids = PersistenceInvariantId::all();
    assert_eq!(
        invariants.len(),
        all_ids.len(),
        "standard invariants should cover all invariant IDs"
    );
    for id in all_ids {
        assert!(
            invariants.iter().any(|i| i.id == *id),
            "missing invariant for {:?}",
            id
        );
    }
}

#[test]
fn all_invariant_ids_have_unique_as_str() {
    let all_ids = PersistenceInvariantId::all();
    let mut seen = std::collections::HashSet::new();
    for id in all_ids {
        let s = id.as_str();
        assert!(seen.insert(s), "duplicate as_str for {:?}: {}", id, s);
    }
}

#[test]
fn standard_invariants_have_data_loss_risk_flags() {
    let invariants = standard_invariants();
    let data_loss_count = invariants.iter().filter(|i| i.data_loss_risk).count();
    // At least some invariants should indicate data loss risk
    assert!(
        data_loss_count > 0,
        "should have at least one data-loss-risk invariant"
    );
    // Not all invariants are data-loss (some are degraded-service)
    assert!(
        data_loss_count < invariants.len(),
        "not all invariants should be data-loss-risk"
    );
}

// =========================================================================
// 2. Standard recovery scenarios coverage
// =========================================================================

#[test]
fn standard_recovery_scenarios_cover_all_crash_types() {
    let scenarios = standard_recovery_scenarios();
    let all_types = CrashScenarioType::all();
    assert_eq!(
        scenarios.len(),
        all_types.len(),
        "should have one scenario per crash type"
    );
    for ct in all_types {
        assert!(
            scenarios.iter().any(|s| s.crash_type == *ct),
            "missing scenario for {:?}",
            ct
        );
    }
}

#[test]
fn standard_scenarios_have_unique_ids() {
    let scenarios = standard_recovery_scenarios();
    let mut seen = std::collections::HashSet::new();
    for s in &scenarios {
        assert!(
            seen.insert(s.scenario_id.clone()),
            "duplicate scenario_id: {}",
            s.scenario_id
        );
    }
}

#[test]
fn each_scenario_validates_at_least_one_invariant() {
    for scenario in &standard_recovery_scenarios() {
        assert!(
            !scenario.validates_invariants.is_empty(),
            "scenario {} should validate at least one invariant",
            scenario.scenario_id
        );
    }
}

#[test]
fn clean_shutdown_validates_all_invariants() {
    let scenarios = standard_recovery_scenarios();
    let clean = scenarios
        .iter()
        .find(|s| s.crash_type == CrashScenarioType::CleanShutdown)
        .expect("clean shutdown scenario must exist");
    assert_eq!(
        clean.validates_invariants.len(),
        PersistenceInvariantId::all().len(),
        "clean shutdown should validate ALL invariants"
    );
    assert_eq!(clean.expected_outcome, RecoveryOutcome::FullRecovery);
}

// =========================================================================
// 3. Gate evaluation — all pass
// =========================================================================

#[test]
fn gate_all_pass_returns_pass_verdict() {
    let results: Vec<ScenarioResult> = standard_recovery_scenarios()
        .iter()
        .map(|s| {
            let invariant_results = s
                .validates_invariants
                .iter()
                .map(|&id| {
                    let inv = standard_invariants()
                        .into_iter()
                        .find(|i| i.id == id)
                        .unwrap();
                    invariant_held(id, inv.data_loss_risk)
                })
                .collect();
            make_scenario_result(
                &s.scenario_id,
                s.crash_type,
                true,
                s.expected_outcome,
                s.expected_outcome,
                invariant_results,
                50,
            )
        })
        .collect();

    let report = PersistenceGateReport::evaluate(results);
    assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
    assert!(!report.any_data_loss);
    assert_eq!(report.failed_scenarios, 0);
    assert_eq!(report.passed_scenarios, report.total_scenarios);
}

// =========================================================================
// 4. Gate evaluation — data loss triggers Fail
// =========================================================================

#[test]
fn gate_data_loss_triggers_fail() {
    let results = vec![make_scenario_result(
        "CRASH-002-sigkill",
        CrashScenarioType::Sigkill,
        false,
        RecoveryOutcome::DegradedRecovery,
        RecoveryOutcome::PartialRecovery,
        vec![
            invariant_held(PersistenceInvariantId::SessionCheckpoint, true),
            invariant_failed(PersistenceInvariantId::CaptureFlush, true), // data_loss=true
        ],
        100,
    )];

    let report = PersistenceGateReport::evaluate(results);
    assert_eq!(report.verdict, PersistenceGateVerdict::Fail);
    assert!(report.any_data_loss);
}

// =========================================================================
// 5. Gate evaluation — non-data-loss failure → ConditionalPass
// =========================================================================

#[test]
fn gate_non_data_loss_failure_conditional_pass() {
    let results = vec![make_scenario_result(
        "CRASH-005-io-fault",
        CrashScenarioType::IoFaultDuringCheckpoint,
        false,
        RecoveryOutcome::DegradedRecovery,
        RecoveryOutcome::PartialRecovery,
        vec![
            invariant_held(PersistenceInvariantId::WalFsync, true),
            invariant_failed(PersistenceInvariantId::SearchIndexSync, false), // not data_loss
        ],
        200,
    )];

    let report = PersistenceGateReport::evaluate(results);
    assert_eq!(report.verdict, PersistenceGateVerdict::ConditionalPass);
    assert!(!report.any_data_loss);
    assert_eq!(report.failed_scenarios, 1);
}

// =========================================================================
// 6. Report rendering
// =========================================================================

#[test]
fn report_summary_contains_verdict_and_counts() {
    let results = vec![make_scenario_result(
        "test-scenario",
        CrashScenarioType::CleanShutdown,
        true,
        RecoveryOutcome::FullRecovery,
        RecoveryOutcome::FullRecovery,
        vec![invariant_held(
            PersistenceInvariantId::SessionCheckpoint,
            true,
        )],
        10,
    )];

    let report = PersistenceGateReport::evaluate(results);
    let summary = report.render_summary();
    assert!(summary.contains("Pass"), "summary should contain verdict");
    assert!(
        summary.contains("1/1"),
        "summary should contain passed/total"
    );
    assert!(summary.contains("Data loss detected: false"));
}

// =========================================================================
// 7. Report serde roundtrip
// =========================================================================

#[test]
fn persistence_gate_report_serde_roundtrip() {
    let results = vec![
        make_scenario_result(
            "CRASH-001-clean",
            CrashScenarioType::CleanShutdown,
            true,
            RecoveryOutcome::FullRecovery,
            RecoveryOutcome::FullRecovery,
            vec![
                invariant_held(PersistenceInvariantId::CaptureFlush, true),
                invariant_held(PersistenceInvariantId::SessionCheckpoint, true),
            ],
            25,
        ),
        make_scenario_result(
            "CRASH-003-partial",
            CrashScenarioType::PartialWrite,
            true,
            RecoveryOutcome::FullRecovery,
            RecoveryOutcome::FullRecovery,
            vec![invariant_held(
                PersistenceInvariantId::TransactionAtomicity,
                true,
            )],
            75,
        ),
    ];

    let report = PersistenceGateReport::evaluate(results);
    let json = serde_json::to_string_pretty(&report).unwrap();
    let restored: PersistenceGateReport = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.verdict, report.verdict);
    assert_eq!(restored.total_scenarios, report.total_scenarios);
    assert_eq!(restored.passed_scenarios, report.passed_scenarios);
    assert_eq!(restored.failed_scenarios, report.failed_scenarios);
    assert_eq!(restored.any_data_loss, report.any_data_loss);
    assert_eq!(restored.max_recovery_time_ms, report.max_recovery_time_ms);
    assert_eq!(restored.results.len(), report.results.len());
}

// =========================================================================
// 8. Recovery time tracking
// =========================================================================

#[test]
fn max_recovery_time_tracked_correctly() {
    let results = vec![
        make_scenario_result(
            "fast",
            CrashScenarioType::CleanShutdown,
            true,
            RecoveryOutcome::FullRecovery,
            RecoveryOutcome::FullRecovery,
            vec![],
            10,
        ),
        make_scenario_result(
            "slow",
            CrashScenarioType::Sigkill,
            true,
            RecoveryOutcome::PartialRecovery,
            RecoveryOutcome::PartialRecovery,
            vec![],
            500,
        ),
        make_scenario_result(
            "medium",
            CrashScenarioType::RestartLoop,
            true,
            RecoveryOutcome::FullRecovery,
            RecoveryOutcome::FullRecovery,
            vec![],
            200,
        ),
    ];

    let report = PersistenceGateReport::evaluate(results);
    assert_eq!(report.max_recovery_time_ms, 500);
}

// =========================================================================
// 9. ScenarioResult data loss detection
// =========================================================================

#[test]
fn scenario_data_loss_detection_only_on_failed_data_loss_invariants() {
    // Held invariant with data_loss=true → no data loss
    let held = make_scenario_result(
        "ok",
        CrashScenarioType::CleanShutdown,
        true,
        RecoveryOutcome::FullRecovery,
        RecoveryOutcome::FullRecovery,
        vec![invariant_held(PersistenceInvariantId::CaptureFlush, true)],
        10,
    );
    assert!(!held.has_data_loss());

    // Failed invariant with data_loss=false → no data loss
    let non_critical = make_scenario_result(
        "non-critical",
        CrashScenarioType::Sigkill,
        false,
        RecoveryOutcome::DegradedRecovery,
        RecoveryOutcome::PartialRecovery,
        vec![invariant_failed(
            PersistenceInvariantId::SearchIndexSync,
            false,
        )],
        50,
    );
    assert!(!non_critical.has_data_loss());

    // Failed invariant with data_loss=true → DATA LOSS
    let critical = make_scenario_result(
        "critical",
        CrashScenarioType::Sigkill,
        false,
        RecoveryOutcome::DegradedRecovery,
        RecoveryOutcome::PartialRecovery,
        vec![invariant_failed(PersistenceInvariantId::WalFsync, true)],
        100,
    );
    assert!(critical.has_data_loss());
}

// =========================================================================
// 10. Durable state: checkpoint → rollback recovery pipeline
// =========================================================================

#[test]
fn checkpoint_rollback_recovery_pipeline() {
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    // Phase 1: Create initial state with 3 panes
    for i in 0..3 {
        let identity = make_pane_identity("ws-1", i);
        registry
            .register_entity(
                identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                1000 + i,
            )
            .unwrap();
    }

    // Phase 2: Checkpoint before risky operation
    let cp1 = manager.checkpoint(
        &registry,
        "pre-operation",
        CheckpointTrigger::PreOperation {
            operation: "resize-all".into(),
        },
        HashMap::new(),
    );
    let cp1_id = cp1.id;
    assert_eq!(cp1.entities.len(), 3);

    // Phase 3: Simulate state changes (risky operation)
    let new_pane = make_pane_identity("ws-1", 10);
    registry
        .register_entity(
            new_pane,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            2000,
        )
        .unwrap();

    // Phase 4: Simulate crash — state is now 4 panes but we want 3
    // Rollback to checkpoint
    let rollback = manager
        .rollback(cp1_id, &mut registry, "simulated crash recovery")
        .unwrap();
    assert_eq!(rollback.target_checkpoint_id, cp1_id);
    assert_eq!(
        rollback.restored_entity_count + rollback.removed_entity_count,
        1
    );

    // Phase 5: Verify recovery — should have exactly 3 entities again
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 3);

    // Phase 6: Post-recovery checkpoint
    let cp2 = manager.checkpoint(
        &registry,
        "post-recovery",
        CheckpointTrigger::PostRecovery,
        HashMap::new(),
    );
    assert_eq!(cp2.entities.len(), 3);
    assert!(
        cp2.id > cp1_id,
        "post-recovery checkpoint should have higher ID"
    );
}

// =========================================================================
// 11. Durable state: double rollback prevention
// =========================================================================

#[test]
fn rollback_to_rolled_back_checkpoint_rejected() {
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    let identity = make_pane_identity("ws-1", 0);
    registry
        .register_entity(
            identity,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            1000,
        )
        .unwrap();

    let cp1 = manager.checkpoint(
        &registry,
        "first",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let cp1_id = cp1.id;

    // Second checkpoint
    let cp2 = manager.checkpoint(
        &registry,
        "second",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let cp2_id = cp2.id;

    // Rollback to cp1 marks cp2 as rolled_back
    manager
        .rollback(cp1_id, &mut registry, "rollback to first")
        .unwrap();

    // Attempting to rollback to cp2 (which was marked rolled_back) should fail
    let result = manager.rollback(cp2_id, &mut registry, "rollback to rolled-back cp");
    assert!(
        result.is_err(),
        "rollback to already rolled-back checkpoint should be rejected"
    );
}

// =========================================================================
// 12. Durable state: nonexistent checkpoint rollback
// =========================================================================

#[test]
fn rollback_to_nonexistent_checkpoint_fails() {
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    let result = manager.rollback(999, &mut registry, "should fail");
    assert!(result.is_err());
}

// =========================================================================
// 13. Durable state: checkpoint serialization roundtrip
// =========================================================================

#[test]
fn durable_state_json_roundtrip() {
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    for i in 0..5 {
        let identity = make_pane_identity("ws-roundtrip", i);
        registry
            .register_entity(
                identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                1000 + i,
            )
            .unwrap();
    }

    manager.checkpoint(
        &registry,
        "roundtrip-test",
        CheckpointTrigger::Manual,
        HashMap::from([("key".into(), "value".into())]),
    );

    let json = manager.to_json().unwrap();
    let restored = DurableStateManager::from_json(&json).unwrap();

    assert_eq!(restored.checkpoint_count(), 1);
    let cp = restored.latest_checkpoint().unwrap();
    assert_eq!(cp.label, "roundtrip-test");
    assert_eq!(cp.entities.len(), 5);
    assert_eq!(cp.metadata.get("key").map(|s| s.as_str()), Some("value"));
}

// =========================================================================
// 14. Durable state: retention limit enforced
// =========================================================================

#[test]
fn checkpoint_retention_limit_enforced() {
    let registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::with_max_checkpoints(3);

    for i in 0..10 {
        manager.checkpoint(
            &registry,
            &format!("cp-{i}"),
            CheckpointTrigger::Periodic,
            HashMap::new(),
        );
    }

    assert!(
        manager.checkpoint_count() <= 3,
        "should retain at most 3 checkpoints, got {}",
        manager.checkpoint_count()
    );
}

// =========================================================================
// 15. Durable state: diff between checkpoints
// =========================================================================

#[test]
fn checkpoint_diff_detects_changes() {
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    // Checkpoint 1: 2 panes
    for i in 0..2 {
        let identity = make_pane_identity("ws-diff", i);
        registry
            .register_entity(
                identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                1000,
            )
            .unwrap();
    }
    let cp1 = manager.checkpoint(
        &registry,
        "state-1",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let cp1_id = cp1.id;

    // Add a third pane
    let new_pane = make_pane_identity("ws-diff", 2);
    registry
        .register_entity(
            new_pane,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            2000,
        )
        .unwrap();

    // Checkpoint 2: 3 panes
    let cp2 = manager.checkpoint(
        &registry,
        "state-2",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let cp2_id = cp2.id;

    let diff = manager.diff(cp1_id, cp2_id).unwrap();
    assert!(
        !diff.added.is_empty() || !diff.changed.is_empty(),
        "diff should detect the new pane"
    );
}

// =========================================================================
// 16. Cross-module: gate + durable state recovery contract
// =========================================================================

#[test]
fn full_pipeline_checkpoint_crash_gate_verdict() {
    // Simulate: create state → checkpoint → modify state → crash →
    // rollback → verify → run gate evaluation
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    // Set up 5 panes
    for i in 0..5 {
        let identity = make_pane_identity("production", i);
        registry
            .register_entity(
                identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                1000,
            )
            .unwrap();
    }
    let cp = manager.checkpoint(
        &registry,
        "pre-crash",
        CheckpointTrigger::PreShutdown,
        HashMap::new(),
    );
    let cp_id = cp.id;

    // Simulate crash aftermath: add 2 zombie panes
    for i in 100..102 {
        let identity = make_pane_identity("production", i);
        registry
            .register_entity(
                identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Closed),
                3000,
            )
            .unwrap();
    }
    assert_eq!(registry.snapshot().len(), 7);

    // Rollback to pre-crash state
    let rollback = manager
        .rollback(cp_id, &mut registry, "crash recovery")
        .unwrap();

    // Verify recovery
    let recovered = registry.snapshot();
    assert_eq!(recovered.len(), 5, "should be back to 5 panes");

    // Build gate result based on successful recovery
    let scenario_result = make_scenario_result(
        "CRASH-002-sigkill",
        CrashScenarioType::Sigkill,
        true,
        RecoveryOutcome::FullRecovery,
        RecoveryOutcome::PartialRecovery,
        vec![
            invariant_held(PersistenceInvariantId::SessionCheckpoint, true),
            invariant_held(PersistenceInvariantId::CaptureFlush, true),
            invariant_held(PersistenceInvariantId::WalFsync, true),
            invariant_held(PersistenceInvariantId::TransactionAtomicity, true),
        ],
        rollback.rolled_back_at.saturating_sub(1000), // simulated recovery time
    );

    let report = PersistenceGateReport::evaluate(vec![scenario_result]);
    assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
    assert!(!report.any_data_loss);
}

// =========================================================================
// 17. Edge case: empty scenario list
// =========================================================================

#[test]
fn gate_empty_scenarios_pass() {
    let report = PersistenceGateReport::evaluate(vec![]);
    assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
    assert_eq!(report.total_scenarios, 0);
    assert!(!report.any_data_loss);
    assert_eq!(report.max_recovery_time_ms, 0);
}

// =========================================================================
// 18. Edge case: all scenarios fail but no data loss
// =========================================================================

#[test]
fn gate_all_fail_no_data_loss_conditional_pass() {
    let results: Vec<ScenarioResult> = CrashScenarioType::all()
        .iter()
        .map(|ct| {
            make_scenario_result(
                &format!("fail-{}", ct.label()),
                *ct,
                false,
                RecoveryOutcome::DegradedRecovery,
                RecoveryOutcome::FullRecovery,
                vec![invariant_failed(
                    PersistenceInvariantId::SearchIndexSync,
                    false,
                )],
                100,
            )
        })
        .collect();

    let report = PersistenceGateReport::evaluate(results);
    assert_eq!(report.verdict, PersistenceGateVerdict::ConditionalPass);
    assert!(!report.any_data_loss);
    assert_eq!(report.passed_scenarios, 0);
}

// =========================================================================
// 19. Crash scenario type coverage
// =========================================================================

#[test]
fn crash_scenario_types_have_unique_labels() {
    let types = CrashScenarioType::all();
    let mut labels = std::collections::HashSet::new();
    for ct in types {
        assert!(labels.insert(ct.label()), "duplicate label: {}", ct.label());
    }
    assert_eq!(labels.len(), 7);
}

// =========================================================================
// 20. Recovery outcome variants exhaustive
// =========================================================================

#[test]
fn recovery_outcome_serde_roundtrip() {
    let outcomes = [
        RecoveryOutcome::FullRecovery,
        RecoveryOutcome::PartialRecovery,
        RecoveryOutcome::DegradedRecovery,
        RecoveryOutcome::GracefulFailure,
    ];
    for outcome in &outcomes {
        let json = serde_json::to_string(outcome).unwrap();
        let back: RecoveryOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(*outcome, back);
    }
}

// =========================================================================
// 21. Invariant result count contract
// =========================================================================

#[test]
fn scenario_invariants_held_count_correct() {
    let result = make_scenario_result(
        "count-test",
        CrashScenarioType::PartialWrite,
        true,
        RecoveryOutcome::FullRecovery,
        RecoveryOutcome::FullRecovery,
        vec![
            invariant_held(PersistenceInvariantId::TransactionAtomicity, true),
            invariant_held(PersistenceInvariantId::WalFsync, true),
            invariant_failed(PersistenceInvariantId::SearchIndexSync, false),
        ],
        50,
    );
    assert_eq!(result.invariants_held(), 2);
    assert!(!result.has_data_loss()); // failed invariant is not data_loss
}

// =========================================================================
// 22. Checkpoint monotonic IDs across rollbacks
// =========================================================================

#[test]
fn checkpoint_ids_monotonic_across_rollbacks() {
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    let cp1 = manager.checkpoint(
        &registry,
        "first",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let id1 = cp1.id;

    let cp2 = manager.checkpoint(
        &registry,
        "second",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let id2 = cp2.id;
    assert!(id2 > id1);

    // Rollback to first
    manager.rollback(id1, &mut registry, "rollback").unwrap();

    // New checkpoint after rollback should still have monotonically increasing ID
    let cp3 = manager.checkpoint(
        &registry,
        "post-rollback",
        CheckpointTrigger::PostRecovery,
        HashMap::new(),
    );
    let id3 = cp3.id;
    assert!(
        id3 > id2,
        "checkpoint ID should be monotonic: {} > {}",
        id3,
        id2
    );
}

// =========================================================================
// 23. Durable state: diff from current vs checkpoint
// =========================================================================

#[test]
fn diff_from_current_detects_live_changes() {
    let mut registry = LifecycleRegistry::new();
    let mut manager = DurableStateManager::new();

    let identity = make_pane_identity("ws-live", 0);
    registry
        .register_entity(
            identity,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            1000,
        )
        .unwrap();

    let cp = manager.checkpoint(
        &registry,
        "baseline",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let cp_id = cp.id;

    // Add new entity to live registry (not checkpointed)
    let new_identity = make_pane_identity("ws-live", 1);
    registry
        .register_entity(
            new_identity,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            2000,
        )
        .unwrap();

    let diff = manager.diff_from_current(cp_id, &registry).unwrap();
    assert!(
        !diff.added.is_empty(),
        "diff should detect entity added since checkpoint"
    );
}

// =========================================================================
// 24. Multiple scenario composite gate evaluation
// =========================================================================

#[test]
fn composite_gate_evaluation_mixed_results() {
    let results = vec![
        // Clean shutdown: full pass
        make_scenario_result(
            "clean",
            CrashScenarioType::CleanShutdown,
            true,
            RecoveryOutcome::FullRecovery,
            RecoveryOutcome::FullRecovery,
            PersistenceInvariantId::all()
                .iter()
                .map(|&id| invariant_held(id, false))
                .collect(),
            10,
        ),
        // Sigkill: partial recovery (expected)
        make_scenario_result(
            "sigkill",
            CrashScenarioType::Sigkill,
            true,
            RecoveryOutcome::PartialRecovery,
            RecoveryOutcome::PartialRecovery,
            vec![
                invariant_held(PersistenceInvariantId::SessionCheckpoint, true),
                invariant_held(PersistenceInvariantId::CaptureFlush, true),
            ],
            150,
        ),
        // Partial write: all good
        make_scenario_result(
            "partial",
            CrashScenarioType::PartialWrite,
            true,
            RecoveryOutcome::FullRecovery,
            RecoveryOutcome::FullRecovery,
            vec![invariant_held(
                PersistenceInvariantId::TransactionAtomicity,
                true,
            )],
            30,
        ),
    ];

    let report = PersistenceGateReport::evaluate(results);
    assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
    assert_eq!(report.total_scenarios, 3);
    assert_eq!(report.passed_scenarios, 3);
    assert_eq!(report.max_recovery_time_ms, 150);
}
