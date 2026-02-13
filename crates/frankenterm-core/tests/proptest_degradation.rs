//! Property-based tests for degradation module.
//!
//! Verifies graceful degradation invariants:
//! - Subsystem: ordering, Display snake_case, serde roundtrip
//! - OverallStatus: ordering, Display UPPERCASE, serde roundtrip
//! - DegradationLevel: variant-only equality
//! - DegradationSnapshot / DegradationReport: serde roundtrips
//! - DegradationManager: state transition correctness, queue bounds,
//!   pattern/workflow idempotence, recovery cleanup, overall_status logic

use proptest::prelude::*;

use frankenterm_core::degradation::{
    DegradationManager, DegradationReport, DegradationSnapshot, OverallStatus, Subsystem,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_subsystem() -> impl Strategy<Value = Subsystem> {
    prop_oneof![
        Just(Subsystem::DbWrite),
        Just(Subsystem::PatternEngine),
        Just(Subsystem::WorkflowEngine),
        Just(Subsystem::WeztermCli),
        Just(Subsystem::MuxConnection),
        Just(Subsystem::Capture),
    ]
}

fn arb_overall_status() -> impl Strategy<Value = OverallStatus> {
    prop_oneof![
        Just(OverallStatus::Healthy),
        Just(OverallStatus::Degraded),
        Just(OverallStatus::Critical),
    ]
}

fn arb_snapshot() -> impl Strategy<Value = DegradationSnapshot> {
    (
        arb_subsystem(),
        prop_oneof![Just("degraded".to_string()), Just("unavailable".to_string())],
        prop::option::of("[a-z ]{1,30}"),
        prop::option::of(0u64..=10_000_000_000),
        prop::option::of(0u64..=1_000_000),
        0u32..=100,
        prop::collection::vec("[a-z ]{3,20}", 0..=5),
    )
        .prop_map(
            |(subsystem, level, reason, since_epoch_ms, duration_ms, recovery_attempts, caps)| {
                DegradationSnapshot {
                    subsystem,
                    level,
                    reason,
                    since_epoch_ms,
                    duration_ms,
                    recovery_attempts,
                    affected_capabilities: caps,
                }
            },
        )
}

fn arb_report() -> impl Strategy<Value = DegradationReport> {
    (
        arb_overall_status(),
        prop::collection::vec(arb_snapshot(), 0..=4),
        0usize..=100,
        0usize..=50,
        0usize..=50,
    )
        .prop_map(
            |(overall, active_degradations, queued, disabled, paused)| DegradationReport {
                overall,
                active_degradations,
                queued_write_count: queued,
                disabled_pattern_count: disabled,
                paused_workflow_count: paused,
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// Subsystem: ordering, Display, serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Subsystem Ord is consistent with PartialOrd.
    #[test]
    fn prop_subsystem_ord_consistent(a in arb_subsystem(), b in arb_subsystem()) {
        // PartialOrd and Ord agree
        prop_assert_eq!(a.partial_cmp(&b), Some(a.cmp(&b)));
    }

    /// Subsystem Display is non-empty and snake_case.
    #[test]
    fn prop_subsystem_display_snake_case(s in arb_subsystem()) {
        let d = s.to_string();
        prop_assert!(!d.is_empty());
        prop_assert!(
            d.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Display should be snake_case, got '{}'", d
        );
    }

    /// Subsystem serde JSON roundtrip.
    #[test]
    fn prop_subsystem_serde_roundtrip(s in arb_subsystem()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: Subsystem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    /// Subsystem serializes to snake_case.
    #[test]
    fn prop_subsystem_serde_snake_case(s in arb_subsystem()) {
        let json = serde_json::to_string(&s).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized subsystem should be snake_case, got '{}'", inner
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// OverallStatus: Display, serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// OverallStatus Display is UPPERCASE.
    #[test]
    fn prop_overall_display_uppercase(s in arb_overall_status()) {
        let d = s.to_string();
        prop_assert!(!d.is_empty());
        let upper = d.to_uppercase();
        prop_assert!(d == upper, "Display should be UPPERCASE, got '{}'", d);
    }

    /// OverallStatus serde roundtrip.
    #[test]
    fn prop_overall_serde_roundtrip(s in arb_overall_status()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: OverallStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    /// OverallStatus serializes to lowercase.
    #[test]
    fn prop_overall_serde_lowercase(s in arb_overall_status()) {
        let json = serde_json::to_string(&s).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase()),
            "serialized status should be lowercase, got '{}'", inner
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationSnapshot: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// DegradationSnapshot JSON roundtrip preserves all fields.
    #[test]
    fn prop_snapshot_serde_roundtrip(snap in arb_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: DegradationSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.subsystem, snap.subsystem);
        prop_assert_eq!(&back.level, &snap.level);
        prop_assert_eq!(back.reason, snap.reason);
        prop_assert_eq!(back.since_epoch_ms, snap.since_epoch_ms);
        prop_assert_eq!(back.duration_ms, snap.duration_ms);
        prop_assert_eq!(back.recovery_attempts, snap.recovery_attempts);
        prop_assert_eq!(back.affected_capabilities.len(), snap.affected_capabilities.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationReport: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// DegradationReport JSON roundtrip preserves counts and overall.
    #[test]
    fn prop_report_serde_roundtrip(report in arb_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: DegradationReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.overall, report.overall);
        prop_assert_eq!(back.queued_write_count, report.queued_write_count);
        prop_assert_eq!(back.disabled_pattern_count, report.disabled_pattern_count);
        prop_assert_eq!(back.paused_workflow_count, report.paused_workflow_count);
        prop_assert_eq!(back.active_degradations.len(), report.active_degradations.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationManager: initial state
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// New manager starts Healthy with no degradations.
    #[test]
    fn prop_initial_state_healthy(_dummy in 0..1u32) {
        let dm = DegradationManager::new();
        prop_assert!(!dm.has_degradations());
        prop_assert_eq!(dm.overall_status(), OverallStatus::Healthy);
        prop_assert_eq!(dm.queued_write_count(), 0);
        prop_assert_eq!(dm.queued_write_bytes(), 0);
        prop_assert!(dm.snapshots().is_empty());
    }

    /// Every subsystem starts as not degraded.
    #[test]
    fn prop_initial_subsystem_not_degraded(s in arb_subsystem()) {
        let dm = DegradationManager::new();
        prop_assert!(!dm.is_degraded(s));
        prop_assert!(!dm.is_unavailable(s));
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationManager: state transitions
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// enter_degraded makes subsystem degraded but not unavailable.
    #[test]
    fn prop_enter_degraded_marks_degraded(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(s, "test reason".into());
        prop_assert!(dm.is_degraded(s));
        prop_assert!(!dm.is_unavailable(s));
        prop_assert!(dm.has_degradations());
    }

    /// enter_unavailable makes subsystem both degraded and unavailable.
    #[test]
    fn prop_enter_unavailable_marks_both(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.enter_unavailable(s, "test reason".into());
        prop_assert!(dm.is_degraded(s));
        prop_assert!(dm.is_unavailable(s));
        prop_assert!(dm.has_degradations());
    }

    /// recover clears the degradation.
    #[test]
    fn prop_recover_clears_degradation(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(s, "test".into());
        dm.recover(s);
        prop_assert!(!dm.is_degraded(s));
        prop_assert!(!dm.is_unavailable(s));
    }

    /// recover from unavailable also clears.
    #[test]
    fn prop_recover_from_unavailable(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.enter_unavailable(s, "test".into());
        dm.recover(s);
        prop_assert!(!dm.is_degraded(s));
        prop_assert!(!dm.is_unavailable(s));
    }

    /// recover on normal subsystem is no-op.
    #[test]
    fn prop_recover_noop_on_normal(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.recover(s);
        prop_assert!(!dm.has_degradations());
        prop_assert_eq!(dm.overall_status(), OverallStatus::Healthy);
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationManager: overall_status logic
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty manager is Healthy.
    #[test]
    fn prop_empty_is_healthy(_dummy in 0..1u32) {
        let dm = DegradationManager::new();
        prop_assert_eq!(dm.overall_status(), OverallStatus::Healthy);
    }

    /// Any degraded subsystem makes overall at least Degraded.
    #[test]
    fn prop_degraded_subsystem_not_healthy(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(s, "test".into());
        prop_assert!(dm.overall_status() != OverallStatus::Healthy);
    }

    /// Any unavailable subsystem makes overall Critical.
    #[test]
    fn prop_unavailable_makes_critical(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.enter_unavailable(s, "test".into());
        prop_assert_eq!(dm.overall_status(), OverallStatus::Critical);
    }

    /// Only degraded (no unavailable) → Degraded status.
    #[test]
    fn prop_only_degraded_is_degraded(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(s, "test".into());
        prop_assert_eq!(dm.overall_status(), OverallStatus::Degraded);
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationManager: queued writes
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// queued_write_count tracks writes.
    #[test]
    fn prop_queue_write_count(count in 1usize..=20) {
        let mut dm = DegradationManager::new();
        for i in 0..count {
            dm.queue_write(format!("w{}", i), 100);
        }
        prop_assert_eq!(dm.queued_write_count(), count);
    }

    /// queued_write_bytes sums data_size.
    #[test]
    fn prop_queue_write_bytes(sizes in prop::collection::vec(1usize..=10_000, 1..=10)) {
        let mut dm = DegradationManager::new();
        let expected_total: usize = sizes.iter().sum();
        for (i, &size) in sizes.iter().enumerate() {
            dm.queue_write(format!("w{}", i), size);
        }
        prop_assert_eq!(dm.queued_write_bytes(), expected_total);
    }

    /// drain_queued_writes empties the queue.
    #[test]
    fn prop_drain_empties_queue(count in 1usize..=10) {
        let mut dm = DegradationManager::new();
        for i in 0..count {
            dm.queue_write(format!("w{}", i), 100);
        }
        let drained = dm.drain_queued_writes();
        prop_assert_eq!(drained.len(), count);
        prop_assert_eq!(dm.queued_write_count(), 0);
        prop_assert_eq!(dm.queued_write_bytes(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationManager: patterns and workflows
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// disable_pattern is idempotent.
    #[test]
    fn prop_disable_pattern_idempotent(id in "[a-z.]{3,20}") {
        let mut dm = DegradationManager::new();
        dm.disable_pattern(id.clone());
        dm.disable_pattern(id.clone());
        prop_assert_eq!(dm.disabled_patterns().len(), 1);
        prop_assert!(dm.is_pattern_disabled(&id));
    }

    /// pause_workflow is idempotent.
    #[test]
    fn prop_pause_workflow_idempotent(id in "[a-z0-9-]{3,20}") {
        let mut dm = DegradationManager::new();
        dm.pause_workflow(id.clone());
        dm.pause_workflow(id.clone());
        prop_assert_eq!(dm.paused_workflows().len(), 1);
        prop_assert!(dm.is_workflow_paused(&id));
    }

    /// resume_workflow clears the pause.
    #[test]
    fn prop_resume_workflow_clears(id in "[a-z0-9-]{3,20}") {
        let mut dm = DegradationManager::new();
        dm.pause_workflow(id.clone());
        prop_assert!(dm.is_workflow_paused(&id));
        dm.resume_workflow(&id);
        prop_assert!(!dm.is_workflow_paused(&id));
        prop_assert!(dm.paused_workflows().is_empty());
    }

    /// recover(PatternEngine) clears disabled patterns.
    #[test]
    fn prop_recover_clears_patterns(id in "[a-z.]{3,20}") {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::PatternEngine, "test".into());
        dm.disable_pattern(id);
        dm.recover(Subsystem::PatternEngine);
        prop_assert!(dm.disabled_patterns().is_empty());
    }

    /// recover(WorkflowEngine) clears paused workflows.
    #[test]
    fn prop_recover_clears_workflows(id in "[a-z0-9-]{3,20}") {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::WorkflowEngine, "test".into());
        dm.pause_workflow(id);
        dm.recover(Subsystem::WorkflowEngine);
        prop_assert!(dm.paused_workflows().is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationManager: snapshots
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// snapshots() only includes degraded/unavailable subsystems.
    #[test]
    fn prop_snapshots_excludes_normal(s in arb_subsystem()) {
        let dm = DegradationManager::new();
        prop_assert!(dm.snapshots().is_empty());

        let mut dm2 = DegradationManager::new();
        dm2.enter_degraded(s, "test".into());
        let snaps = dm2.snapshots();
        prop_assert_eq!(snaps.len(), 1);
        prop_assert_eq!(snaps[0].subsystem, s);
    }

    /// snapshot level string matches the state.
    #[test]
    fn prop_snapshot_level_matches_state(s in arb_subsystem(), unavail in prop::bool::ANY) {
        let mut dm = DegradationManager::new();
        if unavail {
            dm.enter_unavailable(s, "test".into());
        } else {
            dm.enter_degraded(s, "test".into());
        }
        let snaps = dm.snapshots();
        prop_assert_eq!(snaps.len(), 1);
        if unavail {
            prop_assert_eq!(&snaps[0].level, "unavailable");
        } else {
            prop_assert_eq!(&snaps[0].level, "degraded");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DegradationManager: recovery attempts
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Recovery attempts accumulate.
    #[test]
    fn prop_recovery_attempts_accumulate(
        s in arb_subsystem(),
        count in 1u32..=20,
    ) {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(s, "test".into());
        for _ in 0..count {
            dm.record_recovery_attempt(s);
        }
        let snaps = dm.snapshots();
        prop_assert_eq!(snaps.len(), 1);
        prop_assert_eq!(snaps[0].recovery_attempts, count);
    }

    /// Recovery attempts preserved when transitioning degraded → unavailable.
    #[test]
    fn prop_recovery_attempts_preserved_transition(
        s in arb_subsystem(),
        count in 1u32..=10,
    ) {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(s, "initial".into());
        for _ in 0..count {
            dm.record_recovery_attempt(s);
        }
        dm.enter_unavailable(s, "escalated".into());
        let snaps = dm.snapshots();
        prop_assert_eq!(snaps.len(), 1);
        prop_assert_eq!(snaps[0].recovery_attempts, count);
    }

    /// record_recovery_attempt on normal subsystem is no-op.
    #[test]
    fn prop_recovery_attempt_noop_normal(s in arb_subsystem()) {
        let mut dm = DegradationManager::new();
        dm.record_recovery_attempt(s);
        prop_assert!(!dm.has_degradations());
    }
}
