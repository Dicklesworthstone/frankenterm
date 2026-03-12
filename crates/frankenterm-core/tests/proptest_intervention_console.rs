//! Property tests for intervention_console module (ft-3681t.9.5).
//!
//! Covers serde roundtrips, pane control state transitions, approval queue
//! lifecycle, emergency stop semantics, audit trail invariants, state count
//! consistency, and snapshot serialization.

use frankenterm_core::intervention_console::*;
use proptest::prelude::*;
use std::collections::HashSet;

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_control_state() -> impl Strategy<Value = PaneControlState> {
    prop_oneof![
        Just(PaneControlState::Active),
        Just(PaneControlState::Paused),
        Just(PaneControlState::ManualTakeover),
        Just(PaneControlState::Quarantined),
    ]
}

fn arb_risk_level() -> impl Strategy<Value = RiskLevel> {
    prop_oneof![
        Just(RiskLevel::Low),
        Just(RiskLevel::Medium),
        Just(RiskLevel::High),
        Just(RiskLevel::Critical),
    ]
}

fn arb_approval_status() -> impl Strategy<Value = ApprovalStatus> {
    prop_oneof![
        Just(ApprovalStatus::Pending),
        Just(ApprovalStatus::Approved),
        Just(ApprovalStatus::Rejected),
        Just(ApprovalStatus::Expired),
    ]
}

fn arb_emergency_scope() -> impl Strategy<Value = EmergencyScope> {
    prop_oneof![
        Just(EmergencyScope::Global),
        (0..100u64).prop_map(EmergencyScope::Pane),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_pane_control_state(state in arb_pane_control_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: PaneControlState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, back);
    }

    #[test]
    fn serde_roundtrip_risk_level(level in arb_risk_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: RiskLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
    }

    #[test]
    fn serde_roundtrip_approval_status(status in arb_approval_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: ApprovalStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn serde_roundtrip_emergency_scope(scope in arb_emergency_scope()) {
        let json = serde_json::to_string(&scope).unwrap();
        let back: EmergencyScope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(scope, back);
    }
}

// =============================================================================
// PaneControlState semantics
// =============================================================================

proptest! {
    #[test]
    fn agent_can_act_only_when_active(state in arb_pane_control_state()) {
        let expected = state == PaneControlState::Active;
        prop_assert_eq!(state.agent_can_act(), expected);
    }
}

// =============================================================================
// PendingApproval expiry
// =============================================================================

proptest! {
    #[test]
    fn approval_no_ttl_never_expires(
        created_at in 0..1_000_000u64,
        now_offset in 0..10_000_000u64,
    ) {
        let approval = PendingApproval {
            request_id: 1,
            pane_id: 1,
            description: "test".into(),
            risk_level: RiskLevel::Low,
            created_at_ms: created_at,
            ttl_ms: 0,
            status: ApprovalStatus::Pending,
        };
        prop_assert!(!approval.is_expired(created_at + now_offset));
    }

    #[test]
    fn approval_expires_after_ttl(
        created_at in 0..1_000_000u64,
        ttl in 1..100_000u64,
        extra in 0..100_000u64,
    ) {
        let approval = PendingApproval {
            request_id: 1,
            pane_id: 1,
            description: "test".into(),
            risk_level: RiskLevel::Low,
            created_at_ms: created_at,
            ttl_ms: ttl,
            status: ApprovalStatus::Pending,
        };
        let deadline = created_at + ttl;
        // At or past deadline → expired
        prop_assert!(approval.is_expired(deadline + extra));
        // Before deadline → not expired
        if ttl > 1 {
            prop_assert!(!approval.is_expired(created_at + ttl / 2));
        }
    }
}

// =============================================================================
// RiskLevel ordering
// =============================================================================

proptest! {
    #[test]
    fn risk_level_total_order(a in arb_risk_level(), b in arb_risk_level()) {
        // PartialOrd + Ord means we always get Some for cmp
        let _ = a.cmp(&b);
        prop_assert!(a <= b || a > b);
    }

    #[test]
    fn risk_level_low_is_minimum(level in arb_risk_level()) {
        prop_assert!(level >= RiskLevel::Low);
    }

    #[test]
    fn risk_level_critical_is_maximum(level in arb_risk_level()) {
        prop_assert!(level <= RiskLevel::Critical);
    }
}

// =============================================================================
// Console pane registration
// =============================================================================

proptest! {
    #[test]
    fn register_panes_tracked_count(
        pane_ids in prop::collection::hash_set(0..100u64, 0..10),
    ) {
        let mut console = InterventionConsole::new();
        for &pid in &pane_ids {
            console.register_pane(pid);
        }
        prop_assert_eq!(console.tracked_pane_count(), pane_ids.len());
    }

    #[test]
    fn unregister_decrements_count(
        n_panes in 1..8usize,
        remove_idx in 0..8usize,
    ) {
        let mut console = InterventionConsole::new();
        let pane_ids: Vec<u64> = (0..n_panes as u64).collect();
        for &pid in &pane_ids {
            console.register_pane(pid);
        }

        let target = pane_ids[remove_idx.min(n_panes - 1)];
        console.unregister_pane(target);
        prop_assert_eq!(console.tracked_pane_count(), n_panes - 1);
    }

    #[test]
    fn unregistered_pane_defaults_active(pane_id in 0..1000u64) {
        let console = InterventionConsole::new();
        prop_assert_eq!(console.pane_state(pane_id), PaneControlState::Active);
    }
}

// =============================================================================
// State transitions via execute
// =============================================================================

proptest! {
    #[test]
    fn pause_sets_paused(pane_id in 0..50u64) {
        let mut console = InterventionConsole::new();
        console.register_pane(pane_id);

        let r = console.execute("op", InterventionAction::PausePane { pane_id });
        prop_assert!(r.success);
        prop_assert_eq!(r.new_state, Some(PaneControlState::Paused));
        prop_assert_eq!(console.pane_state(pane_id), PaneControlState::Paused);
    }

    #[test]
    fn resume_requires_paused(pane_id in 0..50u64) {
        let mut console = InterventionConsole::new();
        console.register_pane(pane_id);

        // Resume from Active → failure
        let r = console.execute("op", InterventionAction::ResumePane { pane_id });
        prop_assert!(!r.success);

        // Pause then resume → success
        console.execute("op", InterventionAction::PausePane { pane_id });
        let r = console.execute("op", InterventionAction::ResumePane { pane_id });
        prop_assert!(r.success);
        prop_assert_eq!(console.pane_state(pane_id), PaneControlState::Active);
    }

    #[test]
    fn takeover_and_release_cycle(pane_id in 0..50u64) {
        let mut console = InterventionConsole::new();
        console.register_pane(pane_id);

        let r = console.execute("op", InterventionAction::TakeoverPane { pane_id });
        prop_assert!(r.success);
        prop_assert_eq!(console.pane_state(pane_id), PaneControlState::ManualTakeover);
        prop_assert!(!console.pane_state(pane_id).agent_can_act());

        let r = console.execute("op", InterventionAction::ReleaseTakeover { pane_id });
        prop_assert!(r.success);
        prop_assert_eq!(console.pane_state(pane_id), PaneControlState::Active);
    }

    #[test]
    fn release_takeover_requires_takeover_state(pane_id in 0..50u64) {
        let mut console = InterventionConsole::new();
        console.register_pane(pane_id);
        let r = console.execute("op", InterventionAction::ReleaseTakeover { pane_id });
        prop_assert!(!r.success);
    }

    #[test]
    fn quarantine_and_release_cycle(pane_id in 0..50u64) {
        let mut console = InterventionConsole::new();
        console.register_pane(pane_id);

        let r = console.execute("op", InterventionAction::QuarantinePane {
            pane_id,
            reason: "suspicious".into(),
        });
        prop_assert!(r.success);
        prop_assert_eq!(console.pane_state(pane_id), PaneControlState::Quarantined);

        let r = console.execute("op", InterventionAction::ReleaseQuarantine { pane_id });
        prop_assert!(r.success);
        prop_assert_eq!(console.pane_state(pane_id), PaneControlState::Active);
    }

    #[test]
    fn release_quarantine_requires_quarantined_state(pane_id in 0..50u64) {
        let mut console = InterventionConsole::new();
        console.register_pane(pane_id);
        let r = console.execute("op", InterventionAction::ReleaseQuarantine { pane_id });
        prop_assert!(!r.success);
    }
}

// =============================================================================
// Approval queue lifecycle
// =============================================================================

proptest! {
    #[test]
    fn submit_increments_pending(
        n_requests in 0..5usize,
    ) {
        let mut console = InterventionConsole::new();
        for i in 0..n_requests {
            console.submit_approval(i as u64, format!("req-{}", i), RiskLevel::Low, 0);
        }
        prop_assert_eq!(console.pending_approvals().len(), n_requests);
    }

    #[test]
    fn approve_decrements_pending(
        n_requests in 1..5usize,
    ) {
        let mut console = InterventionConsole::new();
        let mut ids = Vec::new();
        for i in 0..n_requests {
            ids.push(console.submit_approval(i as u64, format!("req-{}", i), RiskLevel::Low, 0));
        }

        // Approve first request
        let r = console.execute("op", InterventionAction::ApproveRequest { request_id: ids[0] });
        prop_assert!(r.success);
        prop_assert_eq!(console.pending_approvals().len(), n_requests - 1);
    }

    #[test]
    fn reject_decrements_pending(
        n_requests in 1..5usize,
    ) {
        let mut console = InterventionConsole::new();
        let mut ids = Vec::new();
        for i in 0..n_requests {
            ids.push(console.submit_approval(i as u64, format!("req-{}", i), RiskLevel::Medium, 0));
        }

        let r = console.execute("op", InterventionAction::RejectRequest {
            request_id: ids[0],
            reason: "denied".into(),
        });
        prop_assert!(r.success);
        prop_assert_eq!(console.pending_approvals().len(), n_requests - 1);
    }

    #[test]
    fn approve_nonexistent_fails(request_id in 100..1000u64) {
        let mut console = InterventionConsole::new();
        let r = console.execute("op", InterventionAction::ApproveRequest { request_id });
        prop_assert!(!r.success);
    }

    #[test]
    fn double_approve_fails(_dummy in 0..1u32) {
        let mut console = InterventionConsole::new();
        let id = console.submit_approval(1, "action", RiskLevel::Low, 0);
        let r1 = console.execute("op", InterventionAction::ApproveRequest { request_id: id });
        prop_assert!(r1.success);
        let r2 = console.execute("op", InterventionAction::ApproveRequest { request_id: id });
        prop_assert!(!r2.success);
    }

    #[test]
    fn request_ids_are_unique(n_requests in 0..10usize) {
        let mut console = InterventionConsole::new();
        let mut ids = HashSet::new();
        for i in 0..n_requests {
            let id = console.submit_approval(i as u64, format!("req-{}", i), RiskLevel::Low, 0);
            ids.insert(id);
        }
        prop_assert_eq!(ids.len(), n_requests);
    }
}

// =============================================================================
// Emergency stop semantics
// =============================================================================

proptest! {
    #[test]
    fn global_emergency_pauses_active_panes(
        pane_ids in prop::collection::hash_set(0..20u64, 1..6),
    ) {
        let mut console = InterventionConsole::new();
        for &pid in &pane_ids {
            console.register_pane(pid);
        }

        let r = console.execute("op", InterventionAction::EmergencyStop {
            scope: EmergencyScope::Global,
        });
        prop_assert!(r.success);
        prop_assert!(console.is_emergency_stop_active());

        // All panes that were Active should now be Paused
        for &pid in &pane_ids {
            prop_assert_eq!(console.pane_state(pid), PaneControlState::Paused);
        }
    }

    #[test]
    fn pane_scoped_emergency_only_affects_target(
        target in 0..10u64,
        others in prop::collection::hash_set(10..20u64, 1..5),
    ) {
        let mut console = InterventionConsole::new();
        console.register_pane(target);
        for &pid in &others {
            console.register_pane(pid);
        }

        let r = console.execute("op", InterventionAction::EmergencyStop {
            scope: EmergencyScope::Pane(target),
        });
        prop_assert!(r.success);
        prop_assert_eq!(console.pane_state(target), PaneControlState::Paused);

        // Others remain Active
        for &pid in &others {
            prop_assert_eq!(console.pane_state(pid), PaneControlState::Active);
        }
    }

    #[test]
    fn release_emergency_stop_clears_flag(_dummy in 0..1u32) {
        let mut console = InterventionConsole::new();
        console.execute("op", InterventionAction::EmergencyStop {
            scope: EmergencyScope::Global,
        });
        prop_assert!(console.is_emergency_stop_active());

        let r = console.execute("op", InterventionAction::ReleaseEmergencyStop);
        prop_assert!(r.success);
        prop_assert!(!console.is_emergency_stop_active());
    }

    #[test]
    fn release_inactive_emergency_fails(_dummy in 0..1u32) {
        let mut console = InterventionConsole::new();
        let r = console.execute("op", InterventionAction::ReleaseEmergencyStop);
        prop_assert!(!r.success);
    }
}

// =============================================================================
// Audit trail invariants
// =============================================================================

proptest! {
    #[test]
    fn every_execute_produces_audit_record(
        n_ops in 1..8usize,
    ) {
        let mut console = InterventionConsole::new();
        for i in 0..n_ops {
            console.register_pane(i as u64);
            console.execute("op", InterventionAction::PausePane { pane_id: i as u64 });
        }
        prop_assert_eq!(console.audit_log().len(), n_ops);
    }

    #[test]
    fn audit_sequence_monotonic(
        n_ops in 1..8usize,
    ) {
        let mut console = InterventionConsole::new();
        for i in 0..n_ops {
            console.register_pane(i as u64);
            console.execute("op", InterventionAction::PausePane { pane_id: i as u64 });
        }

        let log = console.audit_log();
        for i in 1..log.len() {
            prop_assert!(log[i].sequence > log[i - 1].sequence);
        }
    }

    #[test]
    fn failed_actions_also_audited(pane_id in 0..50u64) {
        let mut console = InterventionConsole::new();
        console.register_pane(pane_id);
        // Resume without pausing first → fails
        let r = console.execute("op", InterventionAction::ResumePane { pane_id });
        prop_assert!(!r.success);
        // But it's still in the audit log
        prop_assert_eq!(console.audit_log().len(), 1);
        prop_assert!(!console.audit_log()[0].result.success);
    }
}

// =============================================================================
// State counts consistency
// =============================================================================

proptest! {
    #[test]
    fn state_counts_sum_to_tracked(
        n_panes in 0..8usize,
        n_paused in 0..3usize,
    ) {
        let mut console = InterventionConsole::new();
        for i in 0..n_panes {
            console.register_pane(i as u64);
        }
        // Pause some panes
        let pause_count = n_paused.min(n_panes);
        for i in 0..pause_count {
            console.execute("op", InterventionAction::PausePane { pane_id: i as u64 });
        }

        let counts = console.state_counts();
        let total: usize = counts.values().sum();
        prop_assert_eq!(total, console.tracked_pane_count());
    }
}

// =============================================================================
// Snapshot serialization
// =============================================================================

proptest! {
    #[test]
    fn snapshot_serde_roundtrip(
        n_panes in 0..5usize,
    ) {
        let mut console = InterventionConsole::new();
        for i in 0..n_panes {
            console.register_pane(i as u64);
        }
        if n_panes > 0 {
            console.execute("op", InterventionAction::PausePane { pane_id: 0 });
        }

        let snap = console.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: InterventionConsoleSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.pane_states.len(), back.pane_states.len());
        prop_assert_eq!(snap.pending_approvals, back.pending_approvals);
        prop_assert_eq!(snap.emergency_stop_active, back.emergency_stop_active);
        prop_assert_eq!(snap.emergency_scope, back.emergency_scope);
        prop_assert_eq!(snap.audit_log_size, back.audit_log_size);
        prop_assert_eq!(snap.total_approvals_processed, back.total_approvals_processed);
    }

    #[test]
    fn snapshot_reflects_emergency(scope in arb_emergency_scope()) {
        let mut console = InterventionConsole::new();
        console.register_pane(0);
        console.execute("op", InterventionAction::EmergencyStop { scope });

        let snap = console.snapshot();
        prop_assert!(snap.emergency_stop_active);
        prop_assert_eq!(snap.emergency_scope, Some(scope));
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn new_console_empty() {
    let console = InterventionConsole::new();
    assert_eq!(console.tracked_pane_count(), 0);
    assert!(!console.is_emergency_stop_active());
    assert!(console.audit_log().is_empty());
    assert!(console.pending_approvals().is_empty());
}

#[test]
fn default_same_as_new() {
    let a = InterventionConsole::new();
    let b = InterventionConsole::default();
    assert_eq!(a.tracked_pane_count(), b.tracked_pane_count());
    assert_eq!(a.is_emergency_stop_active(), b.is_emergency_stop_active());
}

#[test]
fn default_pane_state_is_active() {
    assert_eq!(PaneControlState::default(), PaneControlState::Active);
}
