//! Integration tests for the resize watchdog assessment pipeline.
//!
//! Validates that `evaluate_resize_watchdog` produces correct severity
//! classifications, safe-mode recommendations, and warning lines for
//! various stalled-transaction scenarios.
//!
//! Contributes to wa-1u90p.7.1 (unit test expansion).

use frankenterm_core::resize_invariants::{ResizeInvariantReport, ResizeInvariantTelemetry};
use frankenterm_core::resize_scheduler::{
    ResizeControlPlaneGateState, ResizeExecutionPhase, ResizeSchedulerConfig,
    ResizeSchedulerDebugSnapshot, ResizeSchedulerMetrics, ResizeSchedulerPaneSnapshot,
    ResizeSchedulerSnapshot, ResizeStalledTransaction,
};
use frankenterm_core::runtime::{
    ResizeWatchdogAssessment, ResizeWatchdogSeverity, evaluate_resize_watchdog,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal debug snapshot with the given pane snapshots.
fn snapshot_with_panes(panes: Vec<ResizeSchedulerPaneSnapshot>) -> ResizeSchedulerDebugSnapshot {
    snapshot_with_panes_and_gate(panes, gate_active())
}

/// Build a snapshot with custom gate state.
fn snapshot_with_panes_and_gate(
    panes: Vec<ResizeSchedulerPaneSnapshot>,
    gate: ResizeControlPlaneGateState,
) -> ResizeSchedulerDebugSnapshot {
    let active_total = panes.iter().filter(|p| p.active_seq.is_some()).count();
    let pending_total = panes.iter().filter(|p| p.pending_seq.is_some()).count();
    ResizeSchedulerDebugSnapshot {
        gate,
        scheduler: ResizeSchedulerSnapshot {
            config: ResizeSchedulerConfig::default(),
            metrics: ResizeSchedulerMetrics::default(),
            pending_total,
            active_total,
            panes,
        },
        lifecycle_events: vec![],
        invariants: ResizeInvariantReport::default(),
        invariant_telemetry: ResizeInvariantTelemetry::default(),
    }
}

fn gate_active() -> ResizeControlPlaneGateState {
    ResizeControlPlaneGateState {
        control_plane_enabled: true,
        emergency_disable: false,
        legacy_fallback_enabled: false,
        active: true,
    }
}

fn gate_safe_mode() -> ResizeControlPlaneGateState {
    ResizeControlPlaneGateState {
        control_plane_enabled: true,
        emergency_disable: true,
        legacy_fallback_enabled: false,
        active: false,
    }
}


/// A pane with an active transaction started at `started_at_ms`.
fn active_pane(pane_id: u64, intent_seq: u64, started_at_ms: u64) -> ResizeSchedulerPaneSnapshot {
    ResizeSchedulerPaneSnapshot {
        pane_id,
        latest_seq: Some(intent_seq),
        pending_seq: None,
        pending_class: None,
        active_seq: Some(intent_seq),
        active_phase: Some(ResizeExecutionPhase::Reflowing),
        active_phase_started_at_ms: Some(started_at_ms),
        deferrals: 0,
        aging_credit: 0,
    }
}

/// A pane with no active transaction (idle).
fn idle_pane(pane_id: u64) -> ResizeSchedulerPaneSnapshot {
    ResizeSchedulerPaneSnapshot {
        pane_id,
        latest_seq: None,
        pending_seq: None,
        pending_class: None,
        active_seq: None,
        active_phase: None,
        active_phase_started_at_ms: None,
        deferrals: 0,
        aging_credit: 0,
    }
}

/// Publish a snapshot globally and evaluate the watchdog at `now_ms`.
fn evaluate_with(snapshot: ResizeSchedulerDebugSnapshot, now_ms: u64) -> ResizeWatchdogAssessment {
    ResizeSchedulerDebugSnapshot::update_global(snapshot);
    evaluate_resize_watchdog(now_ms).expect("snapshot was just published")
}

// ===========================================================================
// Severity classification
// ===========================================================================

#[test]
fn healthy_when_no_active_transactions() {
    let snap = snapshot_with_panes(vec![idle_pane(1), idle_pane(2)]);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.severity, ResizeWatchdogSeverity::Healthy);
    assert_eq!(result.stalled_total, 0);
    assert_eq!(result.stalled_critical, 0);
    assert!(!result.safe_mode_recommended);
    assert!(result.warning_line().is_none());
    assert_eq!(result.recommended_action, "none");
}

#[test]
fn healthy_when_active_transaction_is_fresh() {
    // Active transaction started 500ms ago, well below 2000ms warning threshold.
    let snap = snapshot_with_panes(vec![active_pane(1, 1, 99_500)]);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.severity, ResizeWatchdogSeverity::Healthy);
    assert_eq!(result.stalled_total, 0);
}

#[test]
fn warning_when_one_transaction_stalls_above_threshold() {
    // Active transaction started 3000ms ago, above 2000ms warning threshold.
    let snap = snapshot_with_panes(vec![active_pane(1, 1, 97_000)]);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.severity, ResizeWatchdogSeverity::Warning);
    assert_eq!(result.stalled_total, 1);
    assert_eq!(result.stalled_critical, 0);
    assert!(!result.safe_mode_recommended);
    assert!(result.warning_line().is_some());
    assert!(result.warning_line().unwrap().contains("warning"));
    assert_eq!(result.recommended_action, "monitor_stalled_transactions");
}

#[test]
fn critical_when_two_transactions_stall_above_critical_threshold() {
    // Two transactions stalled above 8000ms critical threshold.
    let snap = snapshot_with_panes(vec![active_pane(1, 1, 90_000), active_pane(2, 1, 91_000)]);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.severity, ResizeWatchdogSeverity::Critical);
    assert_eq!(result.stalled_total, 2);
    assert_eq!(result.stalled_critical, 2);
    assert!(result.safe_mode_recommended);
    assert!(result.warning_line().is_some());
    assert!(result.warning_line().unwrap().contains("CRITICAL"));
    assert_eq!(result.recommended_action, "enable_safe_mode_fallback");
}

#[test]
fn warning_not_critical_when_only_one_critical_stall() {
    // One transaction above critical (8s), one above warning (2s) but not critical.
    let snap = snapshot_with_panes(vec![
        active_pane(1, 1, 90_000), // 10s stall = critical
        active_pane(2, 1, 97_500), // 2.5s stall = warning only
    ]);
    let result = evaluate_with(snap, 100_000);
    // Only 1 critical stall; limit is 2 for safe-mode recommendation.
    assert_eq!(result.stalled_critical, 1);
    assert!(!result.safe_mode_recommended);
    // But still Warning since we have stalled transactions.
    assert_eq!(result.severity, ResizeWatchdogSeverity::Warning);
}

#[test]
fn safe_mode_active_severity_when_emergency_disable_is_set() {
    let snap = snapshot_with_panes_and_gate(
        vec![active_pane(1, 1, 90_000), active_pane(2, 1, 91_000)],
        gate_safe_mode(),
    );
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.severity, ResizeWatchdogSeverity::SafeModeActive);
    assert!(result.safe_mode_active);
    // Even though there are critical stalls, safe_mode_recommended is false
    // because safe_mode is already active.
    assert!(!result.safe_mode_recommended);
    assert!(result.warning_line().unwrap().contains("safe-mode active"));
    assert_eq!(
        result.recommended_action,
        "safe_mode_active_monitor_and_recover"
    );
}

// ===========================================================================
// Legacy fallback flag propagation
// ===========================================================================

#[test]
fn legacy_fallback_flag_in_critical_warning_line() {
    // Test the warning_line output directly (avoids global state race).
    let a = ResizeWatchdogAssessment {
        severity: ResizeWatchdogSeverity::Critical,
        stalled_total: 5,
        stalled_critical: 3,
        warning_threshold_ms: 2000,
        critical_threshold_ms: 8000,
        critical_stalled_limit: 2,
        safe_mode_recommended: true,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
        recommended_action: "enable_safe_mode_fallback".into(),
        sample_stalled: vec![],
    };
    assert!(a.warning_line().unwrap().contains("legacy path"));
}

#[test]
fn no_legacy_text_when_flag_is_false() {
    let a = ResizeWatchdogAssessment {
        severity: ResizeWatchdogSeverity::Critical,
        stalled_total: 5,
        stalled_critical: 3,
        warning_threshold_ms: 2000,
        critical_threshold_ms: 8000,
        critical_stalled_limit: 2,
        safe_mode_recommended: true,
        safe_mode_active: false,
        legacy_fallback_enabled: false,
        recommended_action: "enable_safe_mode_fallback".into(),
        sample_stalled: vec![],
    };
    assert!(!a.warning_line().unwrap().contains("legacy"));
}

// ===========================================================================
// Sample stalled transactions
// ===========================================================================

#[test]
fn sample_stalled_limited_to_sample_cap() {
    // Create 20 critical stalls; sample cap is 8.
    let panes: Vec<_> = (0..20).map(|i| active_pane(i, 1, 80_000)).collect();
    let snap = snapshot_with_panes(panes);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.stalled_critical, 20);
    assert!(result.sample_stalled.len() <= 8);
}

#[test]
fn sample_stalled_uses_critical_when_available() {
    // 1 warning-only + 1 critical. Sample should prefer critical.
    let snap = snapshot_with_panes(vec![
        active_pane(1, 1, 97_500), // 2.5s = warning only
        active_pane(2, 1, 90_000), // 10s = critical
    ]);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.stalled_critical, 1);
    // When critical stalls exist, samples come from critical list.
    assert_eq!(result.sample_stalled.len(), 1);
    assert_eq!(result.sample_stalled[0].pane_id, 2);
}

#[test]
fn sample_stalled_uses_warning_when_no_critical() {
    // 2 warning stalls, 0 critical.
    let snap = snapshot_with_panes(vec![
        active_pane(1, 1, 97_500), // 2.5s = warning
        active_pane(2, 1, 97_000), // 3.0s = warning
    ]);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.stalled_critical, 0);
    assert_eq!(result.stalled_total, 2);
    assert_eq!(result.sample_stalled.len(), 2);
}

// ===========================================================================
// Threshold values propagated correctly
// ===========================================================================

#[test]
fn thresholds_propagated_to_assessment() {
    let snap = snapshot_with_panes(vec![idle_pane(1)]);
    let result = evaluate_with(snap, 100_000);
    assert_eq!(result.warning_threshold_ms, 2_000);
    assert_eq!(result.critical_threshold_ms, 8_000);
    assert_eq!(result.critical_stalled_limit, 2);
}

// ===========================================================================
// Warning line formatting
// ===========================================================================

#[test]
fn warning_line_none_for_healthy() {
    let a = ResizeWatchdogAssessment {
        severity: ResizeWatchdogSeverity::Healthy,
        stalled_total: 0,
        stalled_critical: 0,
        warning_threshold_ms: 2000,
        critical_threshold_ms: 8000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false,
        safe_mode_active: false,
        legacy_fallback_enabled: false,
        recommended_action: "none".into(),
        sample_stalled: vec![],
    };
    assert!(a.warning_line().is_none());
}

#[test]
fn warning_line_contains_stall_count_and_threshold() {
    let a = ResizeWatchdogAssessment {
        severity: ResizeWatchdogSeverity::Warning,
        stalled_total: 3,
        stalled_critical: 0,
        warning_threshold_ms: 2000,
        critical_threshold_ms: 8000,
        critical_stalled_limit: 2,
        safe_mode_recommended: false,
        safe_mode_active: false,
        legacy_fallback_enabled: false,
        recommended_action: "monitor".into(),
        sample_stalled: vec![],
    };
    let line = a.warning_line().unwrap();
    assert!(line.contains("3"));
    assert!(line.contains("2000"));
}

#[test]
fn critical_warning_line_includes_critical_count() {
    let a = ResizeWatchdogAssessment {
        severity: ResizeWatchdogSeverity::Critical,
        stalled_total: 5,
        stalled_critical: 4,
        warning_threshold_ms: 2000,
        critical_threshold_ms: 8000,
        critical_stalled_limit: 2,
        safe_mode_recommended: true,
        safe_mode_active: false,
        legacy_fallback_enabled: false,
        recommended_action: "enable_safe_mode_fallback".into(),
        sample_stalled: vec![],
    };
    let line = a.warning_line().unwrap();
    assert!(line.contains("4"));
    assert!(line.contains("8000"));
    assert!(line.contains("CRITICAL"));
}

// ===========================================================================
// Serde roundtrip
// ===========================================================================

#[test]
fn watchdog_severity_serde_roundtrip() {
    for severity in [
        ResizeWatchdogSeverity::Healthy,
        ResizeWatchdogSeverity::Warning,
        ResizeWatchdogSeverity::Critical,
        ResizeWatchdogSeverity::SafeModeActive,
    ] {
        let json = serde_json::to_string(&severity).unwrap();
        let parsed: ResizeWatchdogSeverity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, severity);
    }
}

#[test]
fn watchdog_assessment_serde_roundtrip() {
    let a = ResizeWatchdogAssessment {
        severity: ResizeWatchdogSeverity::Critical,
        stalled_total: 5,
        stalled_critical: 3,
        warning_threshold_ms: 2000,
        critical_threshold_ms: 8000,
        critical_stalled_limit: 2,
        safe_mode_recommended: true,
        safe_mode_active: false,
        legacy_fallback_enabled: true,
        recommended_action: "enable_safe_mode_fallback".into(),
        sample_stalled: vec![ResizeStalledTransaction {
            pane_id: 42,
            intent_seq: 7,
            active_phase: Some(ResizeExecutionPhase::Reflowing),
            age_ms: 12_000,
            latest_seq: Some(7),
        }],
    };
    let json = serde_json::to_string_pretty(&a).unwrap();
    let parsed: ResizeWatchdogAssessment = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, a);
}

// ===========================================================================
// Stalled transaction derivation edge cases
// ===========================================================================

#[test]
fn stalled_transactions_empty_when_no_active_phases() {
    let snap = snapshot_with_panes(vec![idle_pane(1), idle_pane(2), idle_pane(3)]);
    let stalled = snap
        .scheduler
        .panes
        .iter()
        .filter(|p| p.active_seq.is_some())
        .count();
    assert_eq!(stalled, 0);
}

#[test]
fn stalled_transactions_exact_threshold_is_counted() {
    // Transaction started exactly 2000ms ago. Age = 2000ms, threshold = 2000ms.
    // The filter is `age_ms < threshold_ms` → return None, so age==threshold
    // is NOT less than threshold → the pane IS included as stalled.
    let snap = snapshot_with_panes(vec![active_pane(1, 1, 98_000)]);
    let stalled = snap.stalled_transactions(100_000, 2_000);
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].age_ms, 2_000);
}

#[test]
fn stalled_transactions_one_below_threshold_not_counted() {
    // Transaction started 1999ms ago. age_ms = 1999 < 2000 → not stalled.
    let snap = snapshot_with_panes(vec![active_pane(1, 1, 98_001)]);
    let stalled = snap.stalled_transactions(100_000, 2_000);
    assert_eq!(stalled.len(), 0);
}

#[test]
fn stalled_transactions_one_past_threshold() {
    // Transaction started 2001ms ago.
    let snap = snapshot_with_panes(vec![active_pane(1, 1, 97_999)]);
    let stalled = snap.stalled_transactions(100_000, 2_000);
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].age_ms, 2_001);
}

#[test]
fn stalled_transactions_mixed_ages() {
    let snap = snapshot_with_panes(vec![
        active_pane(1, 1, 99_500), // 500ms - not stalled
        active_pane(2, 1, 97_000), // 3000ms - stalled
        active_pane(3, 1, 95_000), // 5000ms - stalled
        idle_pane(4),              // no active - not stalled
    ]);
    let stalled_2s = snap.stalled_transactions(100_000, 2_000);
    assert_eq!(stalled_2s.len(), 2);
    let stalled_4s = snap.stalled_transactions(100_000, 4_000);
    assert_eq!(stalled_4s.len(), 1);
    assert_eq!(stalled_4s[0].pane_id, 3);
}

#[test]
fn stalled_transactions_saturating_sub_handles_future_timestamp() {
    // active_phase_started_at_ms > now_ms (clock skew). age_ms should be 0.
    let snap = snapshot_with_panes(vec![active_pane(1, 1, 200_000)]);
    let stalled = snap.stalled_transactions(100_000, 1);
    // saturating_sub: 100_000 - 200_000 = 0; 0 < 1 = true → not stalled.
    assert!(stalled.is_empty());
}

// ===========================================================================
// Debug snapshot global lifecycle
// ===========================================================================

#[test]
fn update_global_then_get_global_returns_same() {
    let snap = snapshot_with_panes(vec![active_pane(1, 42, 50_000)]);
    ResizeSchedulerDebugSnapshot::update_global(snap.clone());
    let retrieved = ResizeSchedulerDebugSnapshot::get_global().unwrap();
    assert_eq!(retrieved, snap);
}

#[test]
fn update_global_overwrites_previous() {
    let snap1 = snapshot_with_panes(vec![active_pane(1, 1, 10_000)]);
    let snap2 = snapshot_with_panes(vec![active_pane(2, 2, 20_000)]);
    ResizeSchedulerDebugSnapshot::update_global(snap1);
    ResizeSchedulerDebugSnapshot::update_global(snap2.clone());
    let retrieved = ResizeSchedulerDebugSnapshot::get_global().unwrap();
    assert_eq!(retrieved, snap2);
}
