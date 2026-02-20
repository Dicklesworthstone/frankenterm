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
    DegradationManager, DegradationReport, DegradationSnapshot, OverallStatus,
    ResizeDegradationAssessment, ResizeDegradationSignals, ResizeDegradationTier, Subsystem,
    evaluate_resize_degradation_ladder,
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
        prop_oneof![
            Just("degraded".to_string()),
            Just("unavailable".to_string())
        ],
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
        .prop_map(|(overall, active_degradations, queued, disabled, paused)| {
            DegradationReport {
                overall,
                active_degradations,
                queued_write_count: queued,
                disabled_pattern_count: disabled,
                paused_workflow_count: paused,
            }
        })
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

// ────────────────────────────────────────────────────────────────────
// Strategies: Resize Degradation types
// ────────────────────────────────────────────────────────────────────

fn arb_resize_degradation_tier() -> impl Strategy<Value = ResizeDegradationTier> {
    prop_oneof![
        Just(ResizeDegradationTier::FullQuality),
        Just(ResizeDegradationTier::QualityReduced),
        Just(ResizeDegradationTier::CorrectnessGuarded),
        Just(ResizeDegradationTier::EmergencyCompatibility),
    ]
}

fn arb_resize_degradation_signals() -> impl Strategy<Value = ResizeDegradationSignals> {
    (
        0usize..=50,
        0usize..=20,
        100u64..=30_000,
        100u64..=60_000,
        1usize..=10,
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::bool::ANY,
    )
        .prop_map(
            |(
                stalled_total,
                stalled_critical,
                warning_threshold_ms,
                critical_threshold_ms,
                critical_stalled_limit,
                safe_mode_recommended,
                safe_mode_active,
                legacy_fallback_enabled,
            )| {
                ResizeDegradationSignals {
                    stalled_total,
                    stalled_critical,
                    warning_threshold_ms,
                    critical_threshold_ms,
                    critical_stalled_limit,
                    safe_mode_recommended,
                    safe_mode_active,
                    legacy_fallback_enabled,
                }
            },
        )
}

fn arb_resize_degradation_assessment() -> impl Strategy<Value = ResizeDegradationAssessment> {
    (
        arb_resize_degradation_tier(),
        0u8..=3,
        "[a-z_]{5,40}",
        "[a-z_]{5,40}",
        "[a-z_]{5,40}",
        prop::collection::vec("[a-z_]{3,20}", 0..=3),
        prop::collection::vec("[a-z_]{3,20}", 0..=3),
        prop::collection::vec("[a-z_]{3,20}", 0..=3),
        arb_resize_degradation_signals(),
    )
        .prop_map(
            |(
                tier,
                tier_rank,
                trigger_condition,
                recovery_rule,
                recommended_action,
                quality_reductions,
                correctness_guards,
                availability_changes,
                signals,
            )| {
                ResizeDegradationAssessment {
                    tier,
                    tier_rank,
                    trigger_condition,
                    recovery_rule,
                    recommended_action,
                    quality_reductions,
                    correctness_guards,
                    availability_changes,
                    signals,
                }
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// ResizeDegradationTier: serde, ordering, Display
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ResizeDegradationTier serde JSON roundtrip.
    #[test]
    fn prop_resize_tier_serde_roundtrip(tier in arb_resize_degradation_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: ResizeDegradationTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }

    /// ResizeDegradationTier serializes to snake_case.
    #[test]
    fn prop_resize_tier_serde_snake_case(tier in arb_resize_degradation_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized tier should be snake_case, got '{}'", inner
        );
    }

    /// ResizeDegradationTier Ord is consistent with PartialOrd.
    #[test]
    fn prop_resize_tier_ord_consistent(a in arb_resize_degradation_tier(), b in arb_resize_degradation_tier()) {
        prop_assert_eq!(a.partial_cmp(&b), Some(a.cmp(&b)));
    }

    /// ResizeDegradationTier ordering follows severity: FullQuality < QualityReduced < CorrectnessGuarded < EmergencyCompatibility.
    #[test]
    fn prop_resize_tier_severity_ordering(_dummy in 0..1u32) {
        prop_assert!(ResizeDegradationTier::FullQuality < ResizeDegradationTier::QualityReduced);
        prop_assert!(ResizeDegradationTier::QualityReduced < ResizeDegradationTier::CorrectnessGuarded);
        prop_assert!(ResizeDegradationTier::CorrectnessGuarded < ResizeDegradationTier::EmergencyCompatibility);
    }

    /// ResizeDegradationTier rank() increases with severity.
    #[test]
    fn prop_resize_tier_rank_monotonic(a in arb_resize_degradation_tier(), b in arb_resize_degradation_tier()) {
        if a < b {
            prop_assert!(a.rank() < b.rank(), "rank should increase with severity");
        } else if a == b {
            prop_assert_eq!(a.rank(), b.rank());
        }
    }

    /// ResizeDegradationTier Display is non-empty snake_case.
    #[test]
    fn prop_resize_tier_display_snake_case(tier in arb_resize_degradation_tier()) {
        let d = tier.to_string();
        prop_assert!(!d.is_empty());
        prop_assert!(
            d.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Display should be snake_case, got '{}'", d
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// ResizeDegradationSignals: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// ResizeDegradationSignals JSON roundtrip preserves all fields.
    #[test]
    fn prop_resize_signals_serde_roundtrip(signals in arb_resize_degradation_signals()) {
        let json = serde_json::to_string(&signals).unwrap();
        let back: ResizeDegradationSignals = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, signals);
    }

    /// ResizeDegradationSignals serialization is deterministic.
    #[test]
    fn prop_resize_signals_serde_deterministic(signals in arb_resize_degradation_signals()) {
        let json1 = serde_json::to_string(&signals).unwrap();
        let json2 = serde_json::to_string(&signals).unwrap();
        prop_assert_eq!(json1, json2);
    }

    /// ResizeDegradationSignals JSON structure has expected keys.
    #[test]
    fn prop_resize_signals_json_keys(signals in arb_resize_degradation_signals()) {
        let v: serde_json::Value = serde_json::to_value(&signals).unwrap();
        let obj = v.as_object().unwrap();
        prop_assert!(obj.contains_key("stalled_total"));
        prop_assert!(obj.contains_key("stalled_critical"));
        prop_assert!(obj.contains_key("warning_threshold_ms"));
        prop_assert!(obj.contains_key("critical_threshold_ms"));
        prop_assert!(obj.contains_key("safe_mode_recommended"));
        prop_assert!(obj.contains_key("safe_mode_active"));
        prop_assert!(obj.contains_key("legacy_fallback_enabled"));
    }
}

// ────────────────────────────────────────────────────────────────────
// ResizeDegradationAssessment: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// ResizeDegradationAssessment JSON roundtrip preserves all fields.
    #[test]
    fn prop_resize_assessment_serde_roundtrip(assessment in arb_resize_degradation_assessment()) {
        let json = serde_json::to_string(&assessment).unwrap();
        let back: ResizeDegradationAssessment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.tier, assessment.tier);
        prop_assert_eq!(back.tier_rank, assessment.tier_rank);
        prop_assert_eq!(&back.trigger_condition, &assessment.trigger_condition);
        prop_assert_eq!(&back.recovery_rule, &assessment.recovery_rule);
        prop_assert_eq!(&back.recommended_action, &assessment.recommended_action);
        prop_assert_eq!(back.quality_reductions.len(), assessment.quality_reductions.len());
        prop_assert_eq!(back.correctness_guards.len(), assessment.correctness_guards.len());
        prop_assert_eq!(back.availability_changes.len(), assessment.availability_changes.len());
        prop_assert_eq!(back.signals, assessment.signals);
    }

    /// ResizeDegradationAssessment serialization is deterministic.
    #[test]
    fn prop_resize_assessment_serde_deterministic(assessment in arb_resize_degradation_assessment()) {
        let json1 = serde_json::to_string(&assessment).unwrap();
        let json2 = serde_json::to_string(&assessment).unwrap();
        prop_assert_eq!(json1, json2);
    }

    /// ResizeDegradationAssessment warning_line is None for FullQuality.
    #[test]
    fn prop_resize_assessment_full_quality_no_warning(signals in arb_resize_degradation_signals()) {
        let mut assessment = evaluate_resize_degradation_ladder(signals);
        assessment.tier = ResizeDegradationTier::FullQuality;
        prop_assert!(assessment.warning_line().is_none(),
                    "FullQuality tier should have no warning");
    }

    /// ResizeDegradationAssessment warning_line is Some for non-FullQuality tiers.
    #[test]
    fn prop_resize_assessment_degraded_has_warning(
        tier in prop_oneof![
            Just(ResizeDegradationTier::QualityReduced),
            Just(ResizeDegradationTier::CorrectnessGuarded),
            Just(ResizeDegradationTier::EmergencyCompatibility),
        ],
        signals in arb_resize_degradation_signals(),
    ) {
        let mut assessment = evaluate_resize_degradation_ladder(signals);
        assessment.tier = tier;
        prop_assert!(assessment.warning_line().is_some(),
                    "Non-FullQuality tier should have a warning line");
    }
}

// ────────────────────────────────────────────────────────────────────
// evaluate_resize_degradation_ladder: tier selection logic
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// No stalls and no safe-mode produces FullQuality tier.
    #[test]
    fn prop_ladder_no_stalls_full_quality(
        warning_ms in 100u64..=30_000,
        critical_ms in 100u64..=60_000,
    ) {
        let signals = ResizeDegradationSignals {
            stalled_total: 0,
            stalled_critical: 0,
            warning_threshold_ms: warning_ms,
            critical_threshold_ms: critical_ms,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: false,
        };
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(result.tier, ResizeDegradationTier::FullQuality);
    }

    /// Warning stalls (but no critical/safe-mode) produce QualityReduced tier.
    #[test]
    fn prop_ladder_warning_stalls_quality_reduced(stalled in 1usize..=50) {
        let signals = ResizeDegradationSignals {
            stalled_total: stalled,
            stalled_critical: 0,
            warning_threshold_ms: 5000,
            critical_threshold_ms: 15000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: false,
        };
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(result.tier, ResizeDegradationTier::QualityReduced);
    }

    /// Critical stalls produce CorrectnessGuarded tier.
    #[test]
    fn prop_ladder_critical_stalls_correctness_guarded(stalled in 1usize..=20) {
        let signals = ResizeDegradationSignals {
            stalled_total: stalled + 1,
            stalled_critical: stalled,
            warning_threshold_ms: 5000,
            critical_threshold_ms: 15000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: false,
        };
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(result.tier, ResizeDegradationTier::CorrectnessGuarded);
    }

    /// safe_mode_recommended produces CorrectnessGuarded tier.
    #[test]
    fn prop_ladder_safe_mode_recommended_correctness(_dummy in 0..1u32) {
        let signals = ResizeDegradationSignals {
            stalled_total: 0,
            stalled_critical: 0,
            warning_threshold_ms: 5000,
            critical_threshold_ms: 15000,
            critical_stalled_limit: 3,
            safe_mode_recommended: true,
            safe_mode_active: false,
            legacy_fallback_enabled: false,
        };
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(result.tier, ResizeDegradationTier::CorrectnessGuarded);
    }

    /// safe_mode_active produces EmergencyCompatibility tier.
    #[test]
    fn prop_ladder_safe_mode_active_emergency(
        stalled in 0usize..=50,
        recommended in proptest::bool::ANY,
    ) {
        let signals = ResizeDegradationSignals {
            stalled_total: stalled,
            stalled_critical: 0,
            warning_threshold_ms: 5000,
            critical_threshold_ms: 15000,
            critical_stalled_limit: 3,
            safe_mode_recommended: recommended,
            safe_mode_active: true,
            legacy_fallback_enabled: true,
        };
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(result.tier, ResizeDegradationTier::EmergencyCompatibility);
    }

    /// Ladder result tier_rank matches tier.rank().
    #[test]
    fn prop_ladder_rank_matches_tier(signals in arb_resize_degradation_signals()) {
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(result.tier_rank, result.tier.rank());
    }

    /// Ladder result trigger_condition is non-empty.
    #[test]
    fn prop_ladder_trigger_condition_non_empty(signals in arb_resize_degradation_signals()) {
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!result.trigger_condition.is_empty());
    }

    /// Ladder result recovery_rule is non-empty.
    #[test]
    fn prop_ladder_recovery_rule_non_empty(signals in arb_resize_degradation_signals()) {
        let result = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!result.recovery_rule.is_empty());
    }

    /// Ladder result signals match input signals.
    #[test]
    fn prop_ladder_preserves_input_signals(signals in arb_resize_degradation_signals()) {
        let result = evaluate_resize_degradation_ladder(signals.clone());
        prop_assert_eq!(result.signals, signals);
    }
}
