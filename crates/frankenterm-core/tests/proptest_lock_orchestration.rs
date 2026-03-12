//! Property tests for lock_orchestration module.
//!
//! Covers serde roundtrips for serializable types, LockEntry expiry logic,
//! LockOrchestrator acquire/release/group/handoff semantics, telemetry
//! consistency, and ResourceId invariants.

use frankenterm_core::lock_orchestration::*;
use proptest::prelude::*;
use std::time::Duration;

// =============================================================================
// Strategies
// =============================================================================

fn arb_resource_id() -> impl Strategy<Value = ResourceId> {
    prop_oneof![
        "[a-z/]{1,30}".prop_map(ResourceId::File),
        (0..1000u64).prop_map(ResourceId::Pane),
        "ft-[a-z0-9]{3,10}".prop_map(ResourceId::Bead),
        "[a-z0-9-]{1,20}".prop_map(ResourceId::Custom),
    ]
}

fn arb_handoff_state() -> impl Strategy<Value = HandoffState> {
    prop_oneof![
        Just(HandoffState::Offered),
        Just(HandoffState::Accepted),
        Just(HandoffState::RolledBack),
    ]
}

fn holder(agent: &str) -> LockHolder {
    LockHolder {
        agent_id: agent.to_string(),
        reason: "test".to_string(),
    }
}

fn orch() -> LockOrchestrator {
    LockOrchestrator::default()
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_resource_id(rid in arb_resource_id()) {
        let json = serde_json::to_string(&rid).unwrap();
        let back: ResourceId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rid, back);
    }

    #[test]
    fn serde_roundtrip_handoff_state(st in arb_handoff_state()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: HandoffState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn serde_roundtrip_lock_entry(
        pane_id in 0..1000u64,
        agent in "[a-z]{3,8}",
        acq in 1000..2_000_000u64,
        exp in 0..5_000_000u64,
    ) {
        let entry = LockEntry {
            resource: ResourceId::Pane(pane_id),
            holder: LockHolder { agent_id: agent, reason: "test".into() },
            acquired_at_ms: acq,
            expires_at_ms: exp,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: LockEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.resource, entry.resource);
        prop_assert_eq!(back.acquired_at_ms, entry.acquired_at_ms);
        prop_assert_eq!(back.expires_at_ms, entry.expires_at_ms);
    }

    #[test]
    fn serde_roundtrip_handoff_record(
        src in "[a-z]{3,8}",
        tgt in "[a-z]{3,8}",
        st in arb_handoff_state(),
    ) {
        let rec = HandoffRecord {
            handoff_id: "hoff-42".into(),
            resource: ResourceId::Pane(1),
            source_agent: src.clone(),
            target_agent: tgt.clone(),
            state: st.clone(),
            initiated_at_ms: 1000,
            deadline_ms: 5000,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: HandoffRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.source_agent, src);
        prop_assert_eq!(back.target_agent, tgt);
        prop_assert_eq!(back.state, st);
    }

    #[test]
    fn serde_roundtrip_lock_telemetry(
        acq in 0..100u64,
        rel in 0..100u64,
        cont in 0..100u64,
    ) {
        let t = LockTelemetry {
            locks_acquired: acq,
            locks_released: rel,
            locks_contended: cont,
            locks_expired: 0,
            deadlocks_detected: 0,
            group_acquisitions: 0,
            group_rollbacks: 0,
            handoffs_initiated: 0,
            handoffs_accepted: 0,
            handoffs_rolled_back: 0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: LockTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.locks_acquired, acq);
        prop_assert_eq!(back.locks_released, rel);
        prop_assert_eq!(back.locks_contended, cont);
    }
}

// =============================================================================
// ResourceId invariants
// =============================================================================

proptest! {
    #[test]
    fn resource_id_display_contains_prefix(rid in arb_resource_id()) {
        let s = rid.to_string();
        match &rid {
            ResourceId::File(_) => prop_assert!(s.starts_with("file:")),
            ResourceId::Pane(_) => prop_assert!(s.starts_with("pane:")),
            ResourceId::Bead(_) => prop_assert!(s.starts_with("bead:")),
            ResourceId::Custom(_) => prop_assert!(s.starts_with("custom:")),
        }
    }

    #[test]
    fn resource_id_self_equality(rid in arb_resource_id()) {
        prop_assert_eq!(&rid, &rid);
    }

    #[test]
    fn pane_different_ids_not_equal(a in 0..500u64, b in 500..1000u64) {
        prop_assert_ne!(ResourceId::Pane(a), ResourceId::Pane(b));
    }
}

// =============================================================================
// LockEntry expiry logic
// =============================================================================

proptest! {
    #[test]
    fn lock_entry_no_expiry_never_expires(now in 0..u64::MAX) {
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 0,
            expires_at_ms: 0,
        };
        prop_assert!(!entry.is_expired(now));
        prop_assert_eq!(entry.remaining_ms(now), 0);
    }

    #[test]
    fn lock_entry_expired_when_past_deadline(
        expires in 1..1_000_000u64,
        offset in 0..1_000_000u64,
    ) {
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 0,
            expires_at_ms: expires,
        };
        let now = expires + offset;
        prop_assert!(entry.is_expired(now));
    }

    #[test]
    fn lock_entry_not_expired_before_deadline(
        expires in 2..1_000_000u64,
        now_offset in 1..1_000_000u64,
    ) {
        let expires_val = expires + now_offset; // ensure expires > now
        let now = expires;
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 0,
            expires_at_ms: expires_val,
        };
        prop_assert!(!entry.is_expired(now));
        prop_assert!(entry.remaining_ms(now) > 0);
    }

    #[test]
    fn lock_entry_remaining_ms_saturates(
        expires in 1..1_000_000u64,
        extra in 0..1_000_000u64,
    ) {
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 0,
            expires_at_ms: expires,
        };
        let now = expires + extra;
        prop_assert_eq!(entry.remaining_ms(now), 0);
    }
}

// =============================================================================
// LockResult predicates
// =============================================================================

proptest! {
    #[test]
    fn lock_result_acquired_predicates(_dummy in 0..1u32) {
        let r = LockResult::Acquired;
        prop_assert!(r.is_acquired());
        prop_assert!(!r.is_contended());
        prop_assert!(!r.is_deadlock());
    }

    #[test]
    fn lock_result_contended_predicates(agent in "[a-z]{3,8}") {
        let r = LockResult::Contended {
            held_by: agent,
            reason: "test".into(),
            acquired_at_ms: 0,
        };
        prop_assert!(!r.is_acquired());
        prop_assert!(r.is_contended());
        prop_assert!(!r.is_deadlock());
    }

    #[test]
    fn lock_result_deadlock_predicates(_dummy in 0..1u32) {
        let r = LockResult::DeadlockDetected {
            cycle: vec!["a".into(), "b".into()],
        };
        prop_assert!(!r.is_acquired());
        prop_assert!(!r.is_contended());
        prop_assert!(r.is_deadlock());
    }
}

// =============================================================================
// Orchestrator: acquire/release semantics
// =============================================================================

proptest! {
    #[test]
    fn acquire_free_resource_succeeds(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        let result = o.try_acquire(r.clone(), holder("agent"), None);
        prop_assert!(result.is_acquired());
        prop_assert!(o.is_locked(&r).is_some());
    }

    #[test]
    fn release_returns_to_free(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("agent"), None);
        prop_assert!(o.release(&r, "agent"));
        prop_assert!(o.is_locked(&r).is_none());
    }

    #[test]
    fn same_agent_relock_idempotent(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("a"), None);
        let result = o.try_acquire(r.clone(), holder("a"), None);
        prop_assert!(result.is_acquired());
        prop_assert_eq!(o.active_locks().len(), 1);
    }

    #[test]
    fn contention_between_agents(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("first"), None);
        let result = o.try_acquire(r.clone(), holder("second"), None);
        prop_assert!(result.is_contended());
    }

    #[test]
    fn release_wrong_agent_fails(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("owner"), None);
        prop_assert!(!o.release(&r, "intruder"));
        prop_assert!(o.is_locked(&r).is_some());
    }

    #[test]
    fn release_nonexistent_returns_false(pane_id in 0..100u64) {
        let o = orch();
        prop_assert!(!o.release(&ResourceId::Pane(pane_id), "agent"));
    }

    #[test]
    fn force_release_always_succeeds(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("a"), None);
        let entry = o.force_release(&r);
        prop_assert!(entry.is_some());
        prop_assert!(o.is_locked(&r).is_none());
    }
}

// =============================================================================
// Orchestrator: multi-lock and release_all
// =============================================================================

proptest! {
    #[test]
    fn multiple_resources_independent(a in 0..50u64, b in 50..100u64) {
        let o = orch();
        let r1 = ResourceId::Pane(a);
        let r2 = ResourceId::Pane(b);
        o.try_acquire(r1.clone(), holder("agent-1"), None);
        o.try_acquire(r2.clone(), holder("agent-2"), None);
        prop_assert_eq!(o.active_locks().len(), 2);
        prop_assert_eq!(o.locks_held_by("agent-1").len(), 1);
        prop_assert_eq!(o.locks_held_by("agent-2").len(), 1);
    }

    #[test]
    fn release_all_clears_agent_locks(count in 1..10usize) {
        let o = orch();
        for i in 0..count {
            o.try_acquire(ResourceId::Pane(i as u64), holder("target"), None);
        }
        // Another agent has one lock
        o.try_acquire(ResourceId::Pane(999), holder("other"), None);

        let released = o.release_all("target");
        prop_assert_eq!(released, count);
        prop_assert!(o.locks_held_by("target").is_empty());
        prop_assert_eq!(o.locks_held_by("other").len(), 1);
    }
}

// =============================================================================
// Group acquisition
// =============================================================================

proptest! {
    #[test]
    fn group_acquire_all_free(count in 1..8usize) {
        let o = orch();
        let resources: Vec<ResourceId> = (0..count)
            .map(|i| ResourceId::Pane(i as u64))
            .collect();
        let result = o.try_acquire_group(&resources, holder("a"), None);
        prop_assert!(result.is_all_acquired());
        prop_assert_eq!(o.active_locks().len(), count);
    }

    #[test]
    fn group_acquire_partial_failure_no_leaks(blocked in 0..5u64) {
        let o = orch();
        // Pre-lock one resource
        o.try_acquire(ResourceId::Pane(blocked), holder("blocker"), None);

        let resources: Vec<ResourceId> = (0..5)
            .map(|i| ResourceId::Pane(i))
            .collect();
        let result = o.try_acquire_group(&resources, holder("requester"), None);
        prop_assert!(!result.is_all_acquired());

        // Only the originally blocked resource should be locked (by blocker)
        prop_assert_eq!(o.locks_held_by("requester").len(), 0);
    }

    #[test]
    fn group_too_large_rejected(_dummy in 0..1u32) {
        let config = OrchestratorConfig {
            max_group_size: 2,
            ..Default::default()
        };
        let o = LockOrchestrator::new(config);
        let resources = vec![ResourceId::Pane(1), ResourceId::Pane(2), ResourceId::Pane(3)];
        let result = o.try_acquire_group(&resources, holder("a"), None);
        prop_assert!(!result.is_all_acquired());
    }
}

// =============================================================================
// Telemetry consistency
// =============================================================================

proptest! {
    #[test]
    fn telemetry_acquired_matches_active(count in 1..10usize) {
        let o = orch();
        for i in 0..count {
            o.try_acquire(ResourceId::Pane(i as u64), holder("a"), None);
        }
        let t = o.telemetry();
        prop_assert_eq!(t.locks_acquired as usize, count);
        prop_assert_eq!(o.active_locks().len(), count);
    }

    #[test]
    fn telemetry_contended_increments(attempts in 1..5usize) {
        let o = orch();
        o.try_acquire(ResourceId::Pane(0), holder("owner"), None);
        for _ in 0..attempts {
            o.try_acquire(ResourceId::Pane(0), holder("challenger"), None);
        }
        prop_assert_eq!(o.telemetry().locks_contended as usize, attempts);
    }

    #[test]
    fn telemetry_released_tracks(count in 1..10usize) {
        let o = orch();
        for i in 0..count {
            o.try_acquire(ResourceId::Pane(i as u64), holder("a"), None);
        }
        o.release_all("a");
        prop_assert_eq!(o.telemetry().locks_released as usize, count);
    }

    #[test]
    fn group_telemetry_tracks_group_acquisitions(count in 1..5usize) {
        let o = orch();
        for batch in 0..count {
            let resources: Vec<ResourceId> = (0..3)
                .map(|i| ResourceId::Pane((batch * 10 + i) as u64))
                .collect();
            o.try_acquire_group(&resources, holder("a"), None);
        }
        prop_assert_eq!(o.telemetry().group_acquisitions as usize, count);
    }
}

// =============================================================================
// Handoff protocol
// =============================================================================

proptest! {
    #[test]
    fn handoff_happy_path(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("source"), None);

        let hid = o.initiate_handoff(&r, "source", "target", Duration::from_secs(60)).unwrap();
        prop_assert!(hid.starts_with("hoff-"));

        o.accept_handoff(&hid, "target").unwrap();

        let entry = o.is_locked(&r).unwrap();
        prop_assert_eq!(entry.holder.agent_id, "target");
    }

    #[test]
    fn handoff_rollback_preserves_holder(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("source"), None);

        let hid = o.initiate_handoff(&r, "source", "target", Duration::from_secs(60)).unwrap();
        o.rollback_handoff(&hid).unwrap();

        let entry = o.is_locked(&r).unwrap();
        prop_assert_eq!(entry.holder.agent_id, "source");
    }

    #[test]
    fn handoff_not_holder_rejected(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("actual"), None);

        let result = o.initiate_handoff(&r, "imposter", "target", Duration::from_secs(60));
        let is_not_holder = matches!(result, Err(HandoffError::NotHolder { .. }));
        prop_assert!(is_not_holder);
    }

    #[test]
    fn handoff_no_lock_rejected(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        let result = o.initiate_handoff(&r, "a", "b", Duration::from_secs(60));
        let is_not_held = matches!(result, Err(HandoffError::LockNotHeld));
        prop_assert!(is_not_held);
    }

    #[test]
    fn handoff_wrong_acceptor_rejected(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("source"), None);
        let hid = o.initiate_handoff(&r, "source", "target", Duration::from_secs(60)).unwrap();

        let result = o.accept_handoff(&hid, "wrong_agent");
        let is_wrong_target = matches!(result, Err(HandoffError::WrongTarget { .. }));
        prop_assert!(is_wrong_target);
    }

    #[test]
    fn handoff_telemetry_increments(pane_id in 0..100u64) {
        let o = orch();
        let r = ResourceId::Pane(pane_id);
        o.try_acquire(r.clone(), holder("s"), None);
        let hid = o.initiate_handoff(&r, "s", "t", Duration::from_secs(60)).unwrap();
        o.accept_handoff(&hid, "t").unwrap();

        let t = o.telemetry();
        prop_assert_eq!(t.handoffs_initiated, 1);
        prop_assert_eq!(t.handoffs_accepted, 1);
    }
}

// =============================================================================
// HandoffRecord expiry
// =============================================================================

proptest! {
    #[test]
    fn handoff_record_expired_past_deadline(
        deadline in 1000..100_000u64,
        extra in 0..100_000u64,
    ) {
        let rec = HandoffRecord {
            handoff_id: "hoff-1".into(),
            resource: ResourceId::Pane(1),
            source_agent: "a".into(),
            target_agent: "b".into(),
            state: HandoffState::Offered,
            initiated_at_ms: 0,
            deadline_ms: deadline,
        };
        prop_assert!(rec.is_expired(deadline + extra));
    }

    #[test]
    fn handoff_record_not_expired_before_deadline(
        deadline in 2000..100_000u64,
        before in 0..1000u64,
    ) {
        let rec = HandoffRecord {
            handoff_id: "hoff-1".into(),
            resource: ResourceId::Pane(1),
            source_agent: "a".into(),
            target_agent: "b".into(),
            state: HandoffState::Offered,
            initiated_at_ms: 0,
            deadline_ms: deadline,
        };
        prop_assert!(!rec.is_expired(before));
    }

    #[test]
    fn handoff_record_accepted_never_expired(now in 0..u64::MAX) {
        let rec = HandoffRecord {
            handoff_id: "hoff-1".into(),
            resource: ResourceId::Pane(1),
            source_agent: "a".into(),
            target_agent: "b".into(),
            state: HandoffState::Accepted,
            initiated_at_ms: 0,
            deadline_ms: 1, // past deadline but not Offered
        };
        prop_assert!(!rec.is_expired(now));
    }
}

// =============================================================================
// HandoffError Display
// =============================================================================

proptest! {
    #[test]
    fn handoff_error_display_not_empty(_dummy in 0..1u32) {
        let errors = vec![
            HandoffError::LockNotHeld,
            HandoffError::HandoffNotFound,
            HandoffError::Expired,
            HandoffError::NotHolder { actual_holder: "x".into() },
            HandoffError::WrongTarget { expected: "y".into() },
            HandoffError::InvalidState { current: HandoffState::Accepted },
        ];
        for e in errors {
            prop_assert!(!e.to_string().is_empty());
        }
    }
}

// =============================================================================
// Snapshot diagnostic
// =============================================================================

proptest! {
    #[test]
    fn snapshot_reflects_state(count in 1..10usize) {
        let o = orch();
        for i in 0..count {
            o.try_acquire(ResourceId::Pane(i as u64), holder("a"), None);
        }
        let snap = o.snapshot();
        prop_assert_eq!(snap.active_locks.len(), count);
        prop_assert_eq!(snap.telemetry.locks_acquired as usize, count);
    }

    #[test]
    fn snapshot_serde_roundtrip(count in 1..5usize) {
        let o = orch();
        for i in 0..count {
            o.try_acquire(ResourceId::Pane(i as u64), holder("a"), None);
        }
        let snap = o.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: OrchestratorSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.active_locks.len(), count);
    }
}
