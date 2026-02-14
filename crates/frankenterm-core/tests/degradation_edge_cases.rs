//! Edge-case integration tests for the degradation module.
//!
//! Covers multi-subsystem lifecycle, state transition cycles, boundary
//! conditions for the queued-write buffer, cross-subsystem isolation,
//! snapshot ordering guarantees, and serde roundtrips.
//!
//! Contributes to wa-1u90p.7.1 (unit test expansion).

use frankenterm_core::degradation::{
    DegradationLevel, DegradationManager, DegradationReport, DegradationSnapshot, OverallStatus,
    Subsystem,
};

// ---------------------------------------------------------------------------
// Helper: all six subsystems
// ---------------------------------------------------------------------------

const ALL: [Subsystem; 6] = [
    Subsystem::DbWrite,
    Subsystem::PatternEngine,
    Subsystem::WorkflowEngine,
    Subsystem::WeztermCli,
    Subsystem::MuxConnection,
    Subsystem::Capture,
];

// ===========================================================================
// Full lifecycle cycles
// ===========================================================================

#[test]
fn full_lifecycle_degraded_recover_degraded() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
    assert!(dm.is_degraded(Subsystem::DbWrite));
    dm.recover(Subsystem::DbWrite);
    assert!(!dm.is_degraded(Subsystem::DbWrite));
    dm.enter_degraded(Subsystem::DbWrite, "disk full again".into());
    assert!(dm.is_degraded(Subsystem::DbWrite));
    // Recovery attempts reset after recover() + re-enter.
    match dm.level(Subsystem::DbWrite) {
        DegradationLevel::Degraded {
            recovery_attempts, ..
        } => assert_eq!(*recovery_attempts, 0),
        other => panic!("expected Degraded, got {other:?}"),
    }
}

#[test]
fn lifecycle_degraded_to_unavailable_to_degraded_to_normal() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::WeztermCli, "hanging".into());
    dm.record_recovery_attempt(Subsystem::WeztermCli);
    dm.record_recovery_attempt(Subsystem::WeztermCli);
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);

    dm.enter_unavailable(Subsystem::WeztermCli, "crashed".into());
    assert_eq!(dm.overall_status(), OverallStatus::Critical);
    // Recovery attempts preserved across escalation.
    match dm.level(Subsystem::WeztermCli) {
        DegradationLevel::Unavailable {
            recovery_attempts, ..
        } => assert_eq!(*recovery_attempts, 2),
        other => panic!("expected Unavailable, got {other:?}"),
    }

    // De-escalate back to degraded.
    dm.enter_degraded(Subsystem::WeztermCli, "partially recovered".into());
    assert!(!dm.is_unavailable(Subsystem::WeztermCli));
    assert!(dm.is_degraded(Subsystem::WeztermCli));
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);
    // Recovery attempts preserved during de-escalation.
    match dm.level(Subsystem::WeztermCli) {
        DegradationLevel::Degraded {
            recovery_attempts, ..
        } => assert_eq!(*recovery_attempts, 2),
        other => panic!("expected Degraded, got {other:?}"),
    }

    dm.recover(Subsystem::WeztermCli);
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);
}

#[test]
fn lifecycle_unavailable_direct_to_recover() {
    let mut dm = DegradationManager::new();
    // Go straight to unavailable without passing through degraded.
    dm.enter_unavailable(Subsystem::Capture, "tailer died".into());
    assert!(dm.is_unavailable(Subsystem::Capture));
    assert!(dm.is_degraded(Subsystem::Capture)); // unavailable implies degraded
    dm.recover(Subsystem::Capture);
    assert!(!dm.is_degraded(Subsystem::Capture));
    assert!(!dm.is_unavailable(Subsystem::Capture));
}

// ===========================================================================
// All subsystems through full lifecycle
// ===========================================================================

#[test]
fn all_subsystems_through_degraded_and_recovery() {
    let mut dm = DegradationManager::new();
    for (i, &sub) in ALL.iter().enumerate() {
        dm.enter_degraded(sub, format!("reason_{i}"));
    }
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);
    assert_eq!(dm.snapshots().len(), 6);

    for &sub in &ALL {
        dm.recover(sub);
    }
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);
    assert!(dm.snapshots().is_empty());
}

#[test]
fn all_subsystems_through_unavailable_and_recovery() {
    let mut dm = DegradationManager::new();
    for &sub in &ALL {
        dm.enter_unavailable(sub, "total failure".into());
    }
    assert_eq!(dm.overall_status(), OverallStatus::Critical);
    assert_eq!(dm.snapshots().len(), 6);
    for snap in dm.snapshots() {
        assert_eq!(snap.level, "unavailable");
    }

    for &sub in &ALL {
        dm.recover(sub);
    }
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);
}

// ===========================================================================
// Cross-subsystem isolation
// ===========================================================================

#[test]
fn degrading_one_subsystem_does_not_affect_others() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "disk full".into());

    for &sub in &ALL[1..] {
        assert!(
            !dm.is_degraded(sub),
            "{sub} should not be degraded when only DbWrite is"
        );
    }
}

#[test]
fn recovering_one_subsystem_does_not_affect_others() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
    dm.enter_degraded(Subsystem::Capture, "tailer failed".into());

    dm.recover(Subsystem::DbWrite);
    assert!(dm.is_degraded(Subsystem::Capture));
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);
}

#[test]
fn pattern_engine_recovery_clears_patterns_but_not_workflows() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::PatternEngine, "regex timeout".into());
    dm.enter_degraded(Subsystem::WorkflowEngine, "step failed".into());
    dm.disable_pattern("rule-1".into());
    dm.disable_pattern("rule-2".into());
    dm.pause_workflow("wf-1".into());

    dm.recover(Subsystem::PatternEngine);
    // Patterns cleared, workflows still paused.
    assert!(dm.disabled_patterns().is_empty());
    assert_eq!(dm.paused_workflows().len(), 1);
    assert!(dm.is_workflow_paused("wf-1"));
}

#[test]
fn workflow_engine_recovery_clears_workflows_but_not_patterns() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::PatternEngine, "regex timeout".into());
    dm.enter_degraded(Subsystem::WorkflowEngine, "step failed".into());
    dm.disable_pattern("rule-1".into());
    dm.pause_workflow("wf-1".into());

    dm.recover(Subsystem::WorkflowEngine);
    // Workflows cleared, patterns still disabled.
    assert!(dm.paused_workflows().is_empty());
    assert_eq!(dm.disabled_patterns().len(), 1);
    assert!(dm.is_pattern_disabled("rule-1"));
}

// ===========================================================================
// Overall status priority: Critical > Degraded > Healthy
// ===========================================================================

#[test]
fn overall_status_critical_dominates_degraded() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);
    dm.enter_unavailable(Subsystem::Capture, "tailer died".into());
    assert_eq!(dm.overall_status(), OverallStatus::Critical);
    // Recovering the unavailable subsystem returns to Degraded.
    dm.recover(Subsystem::Capture);
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);
}

// ===========================================================================
// Queued write edge cases
// ===========================================================================

#[test]
fn drain_queued_writes_when_empty() {
    let mut dm = DegradationManager::new();
    let writes = dm.drain_queued_writes();
    assert!(writes.is_empty());
    assert_eq!(dm.queued_write_count(), 0);
    assert_eq!(dm.queued_write_bytes(), 0);
}

#[test]
fn queued_write_zero_data_size() {
    let mut dm = DegradationManager::new();
    dm.queue_write("empty".into(), 0);
    assert_eq!(dm.queued_write_count(), 1);
    assert_eq!(dm.queued_write_bytes(), 0);
}

#[test]
fn queued_write_large_data_size() {
    let mut dm = DegradationManager::new();
    dm.queue_write("huge".into(), usize::MAX);
    assert_eq!(dm.queued_write_count(), 1);
    assert_eq!(dm.queued_write_bytes(), usize::MAX);
}

#[test]
fn queued_writes_accumulate_correctly() {
    let mut dm = DegradationManager::new();
    for i in 0..50 {
        dm.queue_write(format!("w-{i}"), i * 10);
    }
    assert_eq!(dm.queued_write_count(), 50);
    let total_bytes: usize = (0..50).map(|i| i * 10).sum();
    assert_eq!(dm.queued_write_bytes(), total_bytes);
    let writes = dm.drain_queued_writes();
    assert_eq!(writes.len(), 50);
    assert_eq!(writes[0].kind, "w-0");
    assert_eq!(writes[49].kind, "w-49");
}

#[test]
fn drain_clears_and_allows_refill() {
    let mut dm = DegradationManager::new();
    dm.queue_write("first".into(), 100);
    dm.queue_write("second".into(), 200);
    let batch1 = dm.drain_queued_writes();
    assert_eq!(batch1.len(), 2);
    assert_eq!(dm.queued_write_count(), 0);

    dm.queue_write("third".into(), 300);
    assert_eq!(dm.queued_write_count(), 1);
    let batch2 = dm.drain_queued_writes();
    assert_eq!(batch2.len(), 1);
    assert_eq!(batch2[0].kind, "third");
}

// ===========================================================================
// Pattern and workflow edge cases
// ===========================================================================

#[test]
fn disable_many_patterns_and_check_each() {
    let mut dm = DegradationManager::new();
    for i in 0..20 {
        dm.disable_pattern(format!("rule-{i}"));
    }
    assert_eq!(dm.disabled_patterns().len(), 20);
    for i in 0..20 {
        assert!(dm.is_pattern_disabled(&format!("rule-{i}")));
    }
    assert!(!dm.is_pattern_disabled("rule-999"));
}

#[test]
fn resume_nonexistent_workflow_is_noop() {
    let mut dm = DegradationManager::new();
    dm.pause_workflow("wf-1".into());
    dm.resume_workflow("wf-nonexistent");
    assert_eq!(dm.paused_workflows().len(), 1);
    assert!(dm.is_workflow_paused("wf-1"));
}

#[test]
fn resume_all_workflows_one_by_one() {
    let mut dm = DegradationManager::new();
    for i in 0..5 {
        dm.pause_workflow(format!("wf-{i}"));
    }
    assert_eq!(dm.paused_workflows().len(), 5);

    for i in 0..5 {
        dm.resume_workflow(&format!("wf-{i}"));
    }
    assert!(dm.paused_workflows().is_empty());
}

// ===========================================================================
// Snapshot ordering and content
// ===========================================================================

#[test]
fn snapshots_ordered_by_subsystem_btree_order() {
    let mut dm = DegradationManager::new();
    // Insert in reverse order.
    dm.enter_degraded(Subsystem::Capture, "fail".into());
    dm.enter_unavailable(Subsystem::DbWrite, "fail".into());
    dm.enter_degraded(Subsystem::MuxConnection, "fail".into());

    let snaps = dm.snapshots();
    assert_eq!(snaps.len(), 3);
    // BTreeMap ordering: DbWrite < MuxConnection < Capture
    assert_eq!(snaps[0].subsystem, Subsystem::DbWrite);
    assert_eq!(snaps[0].level, "unavailable");
    assert_eq!(snaps[1].subsystem, Subsystem::MuxConnection);
    assert_eq!(snaps[1].level, "degraded");
    assert_eq!(snaps[2].subsystem, Subsystem::Capture);
    assert_eq!(snaps[2].level, "degraded");
}

#[test]
fn snapshots_include_affected_capabilities_per_subsystem() {
    let mut dm = DegradationManager::new();
    for &sub in &ALL {
        dm.enter_degraded(sub, "test".into());
    }
    for snap in dm.snapshots() {
        assert!(
            !snap.affected_capabilities.is_empty(),
            "{:?} should have affected capabilities",
            snap.subsystem
        );
    }
}

#[test]
fn snapshot_epoch_and_duration_are_populated() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
    let snaps = dm.snapshots();
    assert_eq!(snaps.len(), 1);
    assert!(snaps[0].since_epoch_ms.unwrap() > 0);
    // Duration should be very small since we just entered.
    assert!(snaps[0].duration_ms.is_some());
}

// ===========================================================================
// Report coherence
// ===========================================================================

#[test]
fn report_healthy_when_no_degradations() {
    let dm = DegradationManager::new();
    let report = dm.report();
    assert_eq!(report.overall, OverallStatus::Healthy);
    assert!(report.active_degradations.is_empty());
    assert_eq!(report.queued_write_count, 0);
    assert_eq!(report.disabled_pattern_count, 0);
    assert_eq!(report.paused_workflow_count, 0);
}

#[test]
fn report_reflects_all_accumulated_state() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
    dm.enter_unavailable(Subsystem::WeztermCli, "crashed".into());
    dm.queue_write("seg-1".into(), 100);
    dm.queue_write("seg-2".into(), 200);
    dm.disable_pattern("rule-a".into());
    dm.disable_pattern("rule-b".into());
    dm.disable_pattern("rule-c".into());
    dm.pause_workflow("wf-1".into());
    dm.pause_workflow("wf-2".into());

    let report = dm.report();
    assert_eq!(report.overall, OverallStatus::Critical);
    assert_eq!(report.active_degradations.len(), 2);
    assert_eq!(report.queued_write_count, 2);
    assert_eq!(report.disabled_pattern_count, 3);
    assert_eq!(report.paused_workflow_count, 2);
}

// ===========================================================================
// Recovery attempt counter edge cases
// ===========================================================================

#[test]
fn recovery_attempts_accumulate_across_multiple_record_calls() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::MuxConnection, "timeout".into());
    for _ in 0..100 {
        dm.record_recovery_attempt(Subsystem::MuxConnection);
    }
    match dm.level(Subsystem::MuxConnection) {
        DegradationLevel::Degraded {
            recovery_attempts, ..
        } => assert_eq!(*recovery_attempts, 100),
        other => panic!("expected Degraded, got {other:?}"),
    }
}

#[test]
fn recovery_attempts_reset_on_full_recover_and_reenter() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "fail".into());
    dm.record_recovery_attempt(Subsystem::DbWrite);
    dm.record_recovery_attempt(Subsystem::DbWrite);
    dm.recover(Subsystem::DbWrite);
    dm.enter_degraded(Subsystem::DbWrite, "fail again".into());
    match dm.level(Subsystem::DbWrite) {
        DegradationLevel::Degraded {
            recovery_attempts, ..
        } => assert_eq!(*recovery_attempts, 0),
        other => panic!("expected Degraded, got {other:?}"),
    }
}

#[test]
fn recovery_attempts_preserved_on_reason_update() {
    let mut dm = DegradationManager::new();
    dm.enter_degraded(Subsystem::DbWrite, "reason-1".into());
    dm.record_recovery_attempt(Subsystem::DbWrite);
    dm.record_recovery_attempt(Subsystem::DbWrite);
    dm.record_recovery_attempt(Subsystem::DbWrite);

    // Re-entering degraded with a new reason preserves the count.
    dm.enter_degraded(Subsystem::DbWrite, "reason-2".into());
    match dm.level(Subsystem::DbWrite) {
        DegradationLevel::Degraded {
            recovery_attempts,
            reason,
            ..
        } => {
            assert_eq!(*recovery_attempts, 3);
            assert_eq!(reason, "reason-2");
        }
        other => panic!("expected Degraded, got {other:?}"),
    }
}

// ===========================================================================
// DegradationLevel PartialEq edge cases
// ===========================================================================

#[test]
fn degradation_level_eq_ignores_reason_differences() {
    let a = DegradationLevel::Degraded {
        reason: "reason-a".into(),
        since: std::time::Instant::now(),
        since_epoch_ms: 1000,
        recovery_attempts: 5,
    };
    let b = DegradationLevel::Degraded {
        reason: "reason-b".into(),
        since: std::time::Instant::now(),
        since_epoch_ms: 2000,
        recovery_attempts: 0,
    };
    // PartialEq only checks variant, not fields.
    assert_eq!(a, b);
}

#[test]
fn unavailable_levels_are_equal_regardless_of_fields() {
    let a = DegradationLevel::Unavailable {
        reason: "a".into(),
        since: std::time::Instant::now(),
        since_epoch_ms: 0,
        recovery_attempts: 0,
    };
    let b = DegradationLevel::Unavailable {
        reason: "b".into(),
        since: std::time::Instant::now(),
        since_epoch_ms: 999,
        recovery_attempts: 50,
    };
    assert_eq!(a, b);
}

#[test]
fn degraded_not_equal_unavailable() {
    let d = DegradationLevel::Degraded {
        reason: "x".into(),
        since: std::time::Instant::now(),
        since_epoch_ms: 0,
        recovery_attempts: 0,
    };
    let u = DegradationLevel::Unavailable {
        reason: "x".into(),
        since: std::time::Instant::now(),
        since_epoch_ms: 0,
        recovery_attempts: 0,
    };
    assert_ne!(d, u);
}

// ===========================================================================
// Serde roundtrips
// ===========================================================================

#[test]
fn subsystem_serde_roundtrip_all_variants() {
    for &sub in &ALL {
        let json = serde_json::to_string(&sub).unwrap();
        let parsed: Subsystem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, sub);
    }
}

#[test]
fn overall_status_serde_roundtrip_all_variants() {
    for status in [
        OverallStatus::Healthy,
        OverallStatus::Degraded,
        OverallStatus::Critical,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let parsed: OverallStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);
    }
}

#[test]
fn degradation_snapshot_serde_roundtrip() {
    let snap = DegradationSnapshot {
        subsystem: Subsystem::MuxConnection,
        level: "unavailable".to_string(),
        reason: Some("socket reset".to_string()),
        since_epoch_ms: Some(1_700_000_000_000),
        duration_ms: Some(5432),
        recovery_attempts: 7,
        affected_capabilities: vec!["mux ops".into(), "streaming".into()],
    };
    let json = serde_json::to_string(&snap).unwrap();
    let parsed: DegradationSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.subsystem, snap.subsystem);
    assert_eq!(parsed.level, snap.level);
    assert_eq!(parsed.reason, snap.reason);
    assert_eq!(parsed.since_epoch_ms, snap.since_epoch_ms);
    assert_eq!(parsed.duration_ms, snap.duration_ms);
    assert_eq!(parsed.recovery_attempts, snap.recovery_attempts);
    assert_eq!(parsed.affected_capabilities, snap.affected_capabilities);
}

#[test]
fn degradation_snapshot_optional_fields_omitted_in_json() {
    let snap = DegradationSnapshot {
        subsystem: Subsystem::DbWrite,
        level: "degraded".to_string(),
        reason: None,
        since_epoch_ms: None,
        duration_ms: None,
        recovery_attempts: 0,
        affected_capabilities: vec![],
    };
    let json = serde_json::to_string(&snap).unwrap();
    // Optional fields with skip_serializing_if should be absent.
    assert!(!json.contains("reason"));
    assert!(!json.contains("since_epoch_ms"));
    assert!(!json.contains("duration_ms"));
}

#[test]
fn degradation_report_serde_roundtrip() {
    let report = DegradationReport {
        overall: OverallStatus::Critical,
        active_degradations: vec![
            DegradationSnapshot {
                subsystem: Subsystem::DbWrite,
                level: "degraded".to_string(),
                reason: Some("disk full".to_string()),
                since_epoch_ms: Some(1000),
                duration_ms: Some(500),
                recovery_attempts: 3,
                affected_capabilities: vec!["writes".into()],
            },
            DegradationSnapshot {
                subsystem: Subsystem::WeztermCli,
                level: "unavailable".to_string(),
                reason: Some("crashed".to_string()),
                since_epoch_ms: Some(2000),
                duration_ms: Some(1500),
                recovery_attempts: 0,
                affected_capabilities: vec!["capture".into()],
            },
        ],
        queued_write_count: 42,
        disabled_pattern_count: 3,
        paused_workflow_count: 1,
    };
    let json = serde_json::to_string_pretty(&report).unwrap();
    let parsed: DegradationReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.overall, OverallStatus::Critical);
    assert_eq!(parsed.active_degradations.len(), 2);
    assert_eq!(parsed.queued_write_count, 42);
    assert_eq!(parsed.disabled_pattern_count, 3);
    assert_eq!(parsed.paused_workflow_count, 1);
}

// ===========================================================================
// Subsystem Display matches serde rename
// ===========================================================================

#[test]
fn subsystem_display_matches_serde_value() {
    for &sub in &ALL {
        let display = sub.to_string();
        let json = serde_json::to_string(&sub).unwrap();
        // JSON wraps in quotes: "db_write"
        let serde_str = json.trim_matches('"');
        assert_eq!(
            display, serde_str,
            "Display and serde should agree for {sub:?}"
        );
    }
}

// ===========================================================================
// has_degradations precision
// ===========================================================================

#[test]
fn has_degradations_false_after_all_recovered() {
    let mut dm = DegradationManager::new();
    for &sub in &ALL {
        dm.enter_degraded(sub, "test".into());
    }
    assert!(dm.has_degradations());
    for &sub in &ALL {
        dm.recover(sub);
    }
    assert!(!dm.has_degradations());
}

#[test]
fn has_degradations_true_with_single_unavailable() {
    let mut dm = DegradationManager::new();
    dm.enter_unavailable(Subsystem::Capture, "dead".into());
    assert!(dm.has_degradations());
}
