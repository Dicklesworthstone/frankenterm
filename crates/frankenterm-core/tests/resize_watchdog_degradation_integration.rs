//! Cross-module integration tests for the resize watchdog → degradation ladder pipeline.
//!
//! Tests the full interaction chain:
//! - `ResizeScheduler` creates stalled transactions (via submit+schedule with ancient timestamps)
//! - `evaluate_resize_watchdog()` reads global debug snapshot and classifies severity
//! - `evaluate_resize_degradation_ladder()` maps watchdog signals to degradation tiers
//!
//! These tests verify that the severity escalation ordering is correct across the full
//! pipeline: Healthy → Warning → Critical → SafeModeActive, and that the degradation
//! ladder tiers map correctly: FullQuality → QualityReduced → CorrectnessGuarded →
//! EmergencyCompatibility.
//!
//! **NOTE:** Tests that call `evaluate_resize_watchdog()` read a process-global
//! `ResizeSchedulerDebugSnapshot` and are unreliable under parallel execution.
//! Run with `--test-threads=1` or `--ignored` for full coverage.
//!
//! Bead: wa-1u90p.7.1

use frankenterm_core::degradation::{
    ResizeDegradationSignals, ResizeDegradationTier, evaluate_resize_degradation_ladder,
};
use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeExecutionPhase, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig,
    ResizeWorkClass,
};
use frankenterm_core::runtime::{ResizeWatchdogSeverity, evaluate_resize_watchdog};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn intent(pane_id: u64, intent_seq: u64, submitted_at_ms: u64) -> ResizeIntent {
    ResizeIntent {
        pane_id,
        intent_seq,
        scheduler_class: ResizeWorkClass::Interactive,
        work_units: 1,
        submitted_at_ms,
        domain: ResizeDomain::Local,
        tab_id: Some(1),
    }
}

/// Create a scheduler, submit and schedule panes so they become active with
/// `phase_started_at_ms` equal to the intent's `submitted_at_ms`.
fn scheduler_with_active_panes(pane_count: u64, submitted_at_ms: u64) -> ResizeScheduler {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: (pane_count as u32) * 2,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    for i in 1..=pane_count {
        scheduler.submit_intent(intent(i, 1, submitted_at_ms));
    }
    let frame = scheduler.schedule_frame();
    assert_eq!(
        frame.scheduled.len(),
        pane_count as usize,
        "all panes should become active"
    );
    scheduler
}

// =========================================================================
// Section 1: Watchdog severity classification via scheduler global state
// =========================================================================

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn watchdog_healthy_when_no_stalls() {
    // Active transactions with recent timestamps should be Healthy
    let _scheduler = scheduler_with_active_panes(2, 9_000);
    // Evaluate at now=10_000 → phase age = 1_000ms < 2_000ms warning threshold
    let assessment = evaluate_resize_watchdog(10_000).expect("watchdog should produce assessment");
    assert_eq!(assessment.severity, ResizeWatchdogSeverity::Healthy);
    assert_eq!(assessment.stalled_total, 0);
    assert_eq!(assessment.stalled_critical, 0);
    assert!(!assessment.safe_mode_recommended);
    assert!(assessment.warning_line().is_none());
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn watchdog_warning_when_transactions_exceed_warning_threshold() {
    // Active transactions with age > 2_000ms but < 8_000ms
    let _scheduler = scheduler_with_active_panes(2, 1_000);
    // Evaluate at now=4_000 → phase age = 3_000ms > 2_000ms warning, < 8_000ms critical
    let assessment = evaluate_resize_watchdog(4_000).expect("watchdog should produce assessment");
    assert_eq!(assessment.severity, ResizeWatchdogSeverity::Warning);
    assert_eq!(assessment.stalled_total, 2);
    assert_eq!(assessment.stalled_critical, 0);
    assert!(!assessment.safe_mode_recommended);
    let warning = assessment
        .warning_line()
        .expect("warning severity should produce warning line");
    assert!(
        warning.contains("stalled"),
        "warning line should mention stalled transactions"
    );
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn watchdog_critical_when_transactions_exceed_critical_threshold() {
    // Active transactions with age > 8_000ms
    let _scheduler = scheduler_with_active_panes(2, 0);
    // Evaluate at now=10_000 → phase age = 10_000ms > 8_000ms critical threshold
    let assessment = evaluate_resize_watchdog(10_000).expect("watchdog should produce assessment");
    assert_eq!(assessment.severity, ResizeWatchdogSeverity::Critical);
    assert_eq!(assessment.stalled_critical, 2);
    assert!(
        assessment.safe_mode_recommended,
        "critical stalls >= 2 should recommend safe mode"
    );
    let warning = assessment
        .warning_line()
        .expect("critical should produce warning line");
    assert!(warning.contains("CRITICAL"));
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn watchdog_safe_mode_active_when_emergency_disabled() {
    let mut scheduler = scheduler_with_active_panes(2, 0);
    scheduler.set_emergency_disable(true);

    // Evaluate at now=20_000 → even though stalls exist, emergency_disable is active
    let assessment = evaluate_resize_watchdog(20_000).expect("watchdog should produce assessment");
    assert_eq!(assessment.severity, ResizeWatchdogSeverity::SafeModeActive);
    assert!(assessment.safe_mode_active);
    // Safe mode is already active, so don't recommend it again
    assert!(
        !assessment.safe_mode_recommended,
        "should not recommend safe mode when already active"
    );
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn watchdog_single_critical_stall_not_enough_for_safe_mode() {
    // Only 1 critical stall; limit is 2
    let _scheduler = scheduler_with_active_panes(1, 0);
    let assessment = evaluate_resize_watchdog(10_000).expect("watchdog should produce assessment");
    // 1 stall > 8_000ms → Warning (not Critical), since critical needs 2+ stalls
    assert_eq!(assessment.stalled_critical, 1);
    assert!(
        !assessment.safe_mode_recommended,
        "1 critical stall should not trigger safe mode recommendation"
    );
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn watchdog_sample_stalled_includes_pane_details() {
    let _scheduler = scheduler_with_active_panes(3, 0);
    let assessment = evaluate_resize_watchdog(10_000).expect("watchdog should produce assessment");
    assert!(
        !assessment.sample_stalled.is_empty(),
        "assessment should include stalled transaction samples"
    );
    // Verify each sample has valid pane data
    for sample in &assessment.sample_stalled {
        assert!(sample.pane_id > 0, "sample should have valid pane_id");
        assert!(
            sample.age_ms >= 8_000,
            "sample should exceed critical threshold"
        );
    }
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn watchdog_threshold_values_are_propagated() {
    let _scheduler = scheduler_with_active_panes(1, 9_000);
    let assessment = evaluate_resize_watchdog(10_000).expect("watchdog should produce assessment");
    assert_eq!(assessment.warning_threshold_ms, 2_000);
    assert_eq!(assessment.critical_threshold_ms, 8_000);
    assert_eq!(assessment.critical_stalled_limit, 2);
}

// =========================================================================
// Section 2: Degradation ladder tier mapping
// =========================================================================

#[test]
fn degradation_full_quality_when_no_stalls() {
    let signals = ResizeDegradationSignals {
        stalled_total: 0,
        stalled_critical: 0,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(assessment.tier, ResizeDegradationTier::FullQuality);
    assert_eq!(assessment.tier_rank, 0);
    assert!(assessment.quality_reductions.is_empty());
    assert!(assessment.correctness_guards.is_empty());
    assert!(assessment.availability_changes.is_empty());
    assert!(assessment.warning_line().is_none());
}

#[test]
fn degradation_quality_reduced_when_warning_stalls() {
    let signals = ResizeDegradationSignals {
        stalled_total: 3,
        stalled_critical: 0,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(assessment.tier, ResizeDegradationTier::QualityReduced);
    assert_eq!(assessment.tier_rank, 1);
    assert!(
        !assessment.quality_reductions.is_empty(),
        "quality_reduced tier should list reductions"
    );
    assert!(
        assessment.correctness_guards.is_empty(),
        "quality_reduced tier should not have correctness guards"
    );
    let warning = assessment
        .warning_line()
        .expect("quality_reduced should produce warning");
    assert!(warning.contains("quality-reduced"));
}

#[test]
fn degradation_correctness_guarded_when_critical_stalls() {
    let signals = ResizeDegradationSignals {
        stalled_total: 5,
        stalled_critical: 2,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: true,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(assessment.tier, ResizeDegradationTier::CorrectnessGuarded);
    assert_eq!(assessment.tier_rank, 2);
    assert!(
        !assessment.quality_reductions.is_empty(),
        "correctness_guarded should inherit quality reductions"
    );
    assert!(
        !assessment.correctness_guards.is_empty(),
        "correctness_guarded should list guards"
    );
    assert!(
        assessment.availability_changes.is_empty(),
        "correctness_guarded should not have availability changes"
    );
}

#[test]
fn degradation_emergency_compatibility_when_safe_mode_active() {
    let signals = ResizeDegradationSignals {
        stalled_total: 5,
        stalled_critical: 3,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false, // already active, doesn't need recommendation
        safe_mode_active: true,
        legacy_fallback_enabled: true,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(
        assessment.tier,
        ResizeDegradationTier::EmergencyCompatibility
    );
    assert_eq!(assessment.tier_rank, 3);
    assert!(
        !assessment.availability_changes.is_empty(),
        "emergency_compatibility should list availability changes"
    );
    let warning = assessment
        .warning_line()
        .expect("emergency tier should produce warning");
    assert!(warning.contains("emergency"));
    assert!(warning.contains("legacy fallback"));
}

#[test]
fn degradation_tier_ordering_is_monotonic() {
    assert!(ResizeDegradationTier::FullQuality < ResizeDegradationTier::QualityReduced);
    assert!(ResizeDegradationTier::QualityReduced < ResizeDegradationTier::CorrectnessGuarded);
    assert!(
        ResizeDegradationTier::CorrectnessGuarded < ResizeDegradationTier::EmergencyCompatibility
    );
}

#[test]
fn degradation_rank_matches_ordering() {
    let tiers = [
        ResizeDegradationTier::FullQuality,
        ResizeDegradationTier::QualityReduced,
        ResizeDegradationTier::CorrectnessGuarded,
        ResizeDegradationTier::EmergencyCompatibility,
    ];
    for pair in tiers.windows(2) {
        assert!(
            pair[0].rank() < pair[1].rank(),
            "rank should increase: {:?} rank {} vs {:?} rank {}",
            pair[0],
            pair[0].rank(),
            pair[1],
            pair[1].rank()
        );
    }
}

// =========================================================================
// Section 3: End-to-end scheduler → watchdog → degradation integration
// =========================================================================

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn e2e_healthy_scheduler_produces_full_quality_degradation() {
    let _scheduler = scheduler_with_active_panes(2, 9_500);
    let watchdog = evaluate_resize_watchdog(10_000).expect("watchdog assessment");
    assert_eq!(watchdog.severity, ResizeWatchdogSeverity::Healthy);

    let signals = ResizeDegradationSignals {
        stalled_total: watchdog.stalled_total,
        stalled_critical: watchdog.stalled_critical,
        warning_threshold_ms: watchdog.warning_threshold_ms,
        critical_threshold_ms: watchdog.critical_threshold_ms,
        critical_stalled_limit: watchdog.critical_stalled_limit,
        safe_mode_recommended: watchdog.safe_mode_recommended,
        safe_mode_active: watchdog.safe_mode_active,
        legacy_fallback_enabled: watchdog.legacy_fallback_enabled,
    };
    let degradation = evaluate_resize_degradation_ladder(signals);
    assert_eq!(degradation.tier, ResizeDegradationTier::FullQuality);
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn e2e_warning_stalls_produce_quality_reduced_degradation() {
    let _scheduler = scheduler_with_active_panes(2, 1_000);
    // now=4_000 → age 3_000ms > 2_000ms warning, < 8_000ms critical
    let watchdog = evaluate_resize_watchdog(4_000).expect("watchdog assessment");
    assert_eq!(watchdog.severity, ResizeWatchdogSeverity::Warning);

    let signals = ResizeDegradationSignals {
        stalled_total: watchdog.stalled_total,
        stalled_critical: watchdog.stalled_critical,
        warning_threshold_ms: watchdog.warning_threshold_ms,
        critical_threshold_ms: watchdog.critical_threshold_ms,
        critical_stalled_limit: watchdog.critical_stalled_limit,
        safe_mode_recommended: watchdog.safe_mode_recommended,
        safe_mode_active: watchdog.safe_mode_active,
        legacy_fallback_enabled: watchdog.legacy_fallback_enabled,
    };
    let degradation = evaluate_resize_degradation_ladder(signals);
    assert_eq!(degradation.tier, ResizeDegradationTier::QualityReduced);
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn e2e_critical_stalls_produce_correctness_guarded_degradation() {
    let _scheduler = scheduler_with_active_panes(2, 0);
    // now=10_000 → age 10_000ms > 8_000ms critical
    let watchdog = evaluate_resize_watchdog(10_000).expect("watchdog assessment");
    assert_eq!(watchdog.severity, ResizeWatchdogSeverity::Critical);

    let signals = ResizeDegradationSignals {
        stalled_total: watchdog.stalled_total,
        stalled_critical: watchdog.stalled_critical,
        warning_threshold_ms: watchdog.warning_threshold_ms,
        critical_threshold_ms: watchdog.critical_threshold_ms,
        critical_stalled_limit: watchdog.critical_stalled_limit,
        safe_mode_recommended: watchdog.safe_mode_recommended,
        safe_mode_active: watchdog.safe_mode_active,
        legacy_fallback_enabled: watchdog.legacy_fallback_enabled,
    };
    let degradation = evaluate_resize_degradation_ladder(signals);
    assert_eq!(degradation.tier, ResizeDegradationTier::CorrectnessGuarded);
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn e2e_emergency_disable_produces_emergency_compatibility() {
    let mut scheduler = scheduler_with_active_panes(2, 0);
    scheduler.set_emergency_disable(true);

    let watchdog = evaluate_resize_watchdog(20_000).expect("watchdog assessment");
    assert_eq!(watchdog.severity, ResizeWatchdogSeverity::SafeModeActive);

    let signals = ResizeDegradationSignals {
        stalled_total: watchdog.stalled_total,
        stalled_critical: watchdog.stalled_critical,
        warning_threshold_ms: watchdog.warning_threshold_ms,
        critical_threshold_ms: watchdog.critical_threshold_ms,
        critical_stalled_limit: watchdog.critical_stalled_limit,
        safe_mode_recommended: watchdog.safe_mode_recommended,
        safe_mode_active: watchdog.safe_mode_active,
        legacy_fallback_enabled: watchdog.legacy_fallback_enabled,
    };
    let degradation = evaluate_resize_degradation_ladder(signals);
    assert_eq!(
        degradation.tier,
        ResizeDegradationTier::EmergencyCompatibility
    );
}

// =========================================================================
// Section 4: Phase transitions affecting watchdog detection
// =========================================================================

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn completed_transactions_no_longer_stall() {
    let mut scheduler = scheduler_with_active_panes(2, 0);

    // Complete both transactions
    assert!(scheduler.complete_active(1, 1));
    assert!(scheduler.complete_active(2, 1));

    // Watchdog should report healthy since no active transactions remain
    let assessment = evaluate_resize_watchdog(20_000).expect("watchdog assessment");
    assert_eq!(
        assessment.severity,
        ResizeWatchdogSeverity::Healthy,
        "completed transactions should not count as stalled"
    );
    assert_eq!(assessment.stalled_total, 0);
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn phase_transition_resets_phase_started_at() {
    let mut scheduler = scheduler_with_active_panes(1, 0);

    // At now=5_000, age is 5_000ms → stalled (> 2_000ms warning)
    let before = evaluate_resize_watchdog(5_000).expect("watchdog before phase transition");
    assert_eq!(before.severity, ResizeWatchdogSeverity::Warning);

    // Advance phase with recent timestamp → resets phase_started_at_ms
    scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Reflowing, 4_500);

    // Now at now=5_000, age is only 500ms → not stalled
    let after = evaluate_resize_watchdog(5_000).expect("watchdog after phase transition");
    assert_eq!(
        after.severity,
        ResizeWatchdogSeverity::Healthy,
        "phase transition with recent timestamp should reset stall timer"
    );
}

#[test]
#[ignore = "requires serial execution: evaluate_resize_watchdog reads process-global state"]
fn supersession_removes_stalled_active() {
    let mut scheduler = scheduler_with_active_panes(1, 0);

    // At now=5_000 → stalled
    let before = evaluate_resize_watchdog(5_000).expect("watchdog before supersession");
    assert_eq!(before.stalled_total, 1);

    // Submit newer intent and cancel active
    scheduler.submit_intent(intent(1, 2, 4_900));
    scheduler.cancel_active_if_superseded(1);

    // Active is now gone; schedule the new intent
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);
    assert_eq!(frame.scheduled[0].intent_seq, 2);

    // At now=5_000, new active's phase_started_at is 4_900 → age 100ms → healthy
    let after = evaluate_resize_watchdog(5_000).expect("watchdog after supersession");
    assert_eq!(
        after.severity,
        ResizeWatchdogSeverity::Healthy,
        "superseded transaction replaced by fresh one should be healthy"
    );
}

// =========================================================================
// Section 5: Degradation assessment metadata
// =========================================================================

#[test]
fn degradation_trigger_condition_contains_stall_counts() {
    let signals = ResizeDegradationSignals {
        stalled_total: 5,
        stalled_critical: 3,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: true,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert!(
        assessment
            .trigger_condition
            .contains("safe_mode_recommended"),
        "trigger should mention safe mode: {}",
        assessment.trigger_condition
    );
}

#[test]
fn degradation_recovery_rule_is_not_empty() {
    let tiers = [
        (0, 0, false, false),
        (3, 0, false, false),
        (5, 2, true, false),
        (5, 3, false, true),
    ];
    for (stalled_total, stalled_critical, safe_mode_recommended, safe_mode_active) in tiers {
        let signals = ResizeDegradationSignals {
            stalled_total,
            stalled_critical,
            warning_threshold_ms: 2_000,
            critical_threshold_ms: 8_000,
            critical_stalled_limit: 2,
            safe_mode_recommended,
            safe_mode_active,
            legacy_fallback_enabled: true,
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        assert!(
            !assessment.recovery_rule.is_empty(),
            "tier {:?} should have recovery rule",
            assessment.tier
        );
        assert!(
            !assessment.recommended_action.is_empty(),
            "tier {:?} should have recommended action",
            assessment.tier
        );
    }
}

#[test]
fn degradation_correctness_guarded_without_safe_mode_recommendation() {
    // Critical stalls > 0 but < limit → CorrectnessGuarded without safe_mode_recommended
    let signals = ResizeDegradationSignals {
        stalled_total: 3,
        stalled_critical: 1,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(assessment.tier, ResizeDegradationTier::CorrectnessGuarded);
    assert!(
        assessment
            .trigger_condition
            .contains("critical_stalls_detected"),
        "trigger should mention critical stalls: {}",
        assessment.trigger_condition
    );
}

#[test]
fn degradation_emergency_without_legacy_fallback() {
    let signals = ResizeDegradationSignals {
        stalled_total: 5,
        stalled_critical: 3,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false,
        safe_mode_active: true,
        legacy_fallback_enabled: false, // no legacy fallback
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(
        assessment.tier,
        ResizeDegradationTier::EmergencyCompatibility
    );
    let warning = assessment.warning_line().expect("should have warning line");
    // Without legacy fallback, warning should NOT mention "legacy fallback"
    assert!(
        !warning.contains("legacy fallback"),
        "no legacy fallback should not mention it"
    );
}

#[test]
fn degradation_serde_roundtrip() {
    let signals = ResizeDegradationSignals {
        stalled_total: 2,
        stalled_critical: 1,
        warning_threshold_ms: 2_000,
        critical_threshold_ms: 8_000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    let json = serde_json::to_string(&assessment).expect("serialize");
    let roundtripped: frankenterm_core::degradation::ResizeDegradationAssessment =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(assessment.tier, roundtripped.tier);
    assert_eq!(assessment.tier_rank, roundtripped.tier_rank);
    assert_eq!(
        assessment.quality_reductions.len(),
        roundtripped.quality_reductions.len()
    );
}
