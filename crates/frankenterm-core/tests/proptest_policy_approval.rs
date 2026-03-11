//! Property-based tests for ApprovalTracker, ApprovalEntry, ApprovalStatus,
//! ApprovalTrackerSnapshot, RevocationRecord, RevocationRegistry, and
//! RevocationRegistrySnapshot serde roundtrips + behavioral invariants.

use frankenterm_core::policy::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_approval_status() -> impl Strategy<Value = ApprovalStatus> {
    prop_oneof![
        Just(ApprovalStatus::Pending),
        Just(ApprovalStatus::Approved),
        Just(ApprovalStatus::Rejected),
        Just(ApprovalStatus::Expired),
        Just(ApprovalStatus::Revoked),
    ]
}

fn arb_approval_entry() -> impl Strategy<Value = ApprovalEntry> {
    (
        "[a-z0-9-]{1,20}",  // approval_id
        "[a-z_]{1,15}",     // action
        "[a-z_]{1,15}",     // actor
        "[a-z0-9_]{1,20}",  // resource
        "[a-z ]{1,30}",     // reason
        "[a-z0-9.]{1,20}",  // rule_id
        any::<u64>(),        // requested_at_ms
        any::<u64>(),        // expires_at_ms
        arb_approval_status(),
        "[a-z_]{0,15}",     // decided_by
        any::<u64>(),        // decided_at_ms
    )
        .prop_map(
            |(id, action, actor, resource, reason, rule_id, req_at, exp_at, status, decided_by, dec_at)| {
                ApprovalEntry {
                    approval_id: id,
                    action,
                    actor,
                    resource,
                    reason,
                    rule_id,
                    requested_at_ms: req_at,
                    expires_at_ms: exp_at,
                    status,
                    decided_by,
                    decided_at_ms: dec_at,
                }
            },
        )
}

fn arb_approval_tracker_snapshot() -> impl Strategy<Value = ApprovalTrackerSnapshot> {
    (
        any::<usize>(),
        any::<usize>(),
        any::<usize>(),
        any::<usize>(),
        any::<usize>(),
        any::<usize>(),
        any::<usize>(),
    )
        .prop_map(
            |(total, pending, approved, rejected, expired, revoked, max)| {
                ApprovalTrackerSnapshot {
                    total,
                    pending,
                    approved,
                    rejected,
                    expired,
                    revoked,
                    max_entries: max,
                }
            },
        )
}

fn arb_approval_request() -> impl Strategy<Value = ApprovalRequest> {
    (
        "[a-z0-9]{4,8}",  // allow_once_code
        "[a-f0-9]{64}",   // allow_once_full_hash
        any::<i64>(),      // expires_at
        "[a-z ]{1,30}",   // summary
        "[a-z -]{1,30}",  // command
    )
        .prop_map(|(code, hash, expires, summary, command)| ApprovalRequest {
            allow_once_code: code,
            allow_once_full_hash: hash,
            expires_at: expires,
            summary,
            command,
        })
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn approval_status_json_roundtrip(status in arb_approval_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: ApprovalStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn approval_entry_json_roundtrip(entry in arb_approval_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: ApprovalEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry, back);
    }

    #[test]
    fn approval_tracker_snapshot_json_roundtrip(snap in arb_approval_tracker_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: ApprovalTrackerSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn approval_request_json_roundtrip(req in arb_approval_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: ApprovalRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(req, back);
    }
}

// =============================================================================
// ApprovalStatus behavioral tests
// =============================================================================

proptest! {
    #[test]
    fn only_approved_grants_access(status in arb_approval_status()) {
        if status == ApprovalStatus::Approved {
            prop_assert!(status.grants_access());
        } else {
            prop_assert!(!status.grants_access());
        }
    }

    #[test]
    fn as_str_is_stable(status in arb_approval_status()) {
        let tag = status.as_str();
        prop_assert!(!tag.is_empty());
        // Verify as_str matches serde value (which uses snake_case)
        let json = serde_json::to_string(&status).unwrap();
        let expected = format!("\"{tag}\"");
        prop_assert_eq!(json, expected);
    }
}

// =============================================================================
// ApprovalTracker behavioral tests
// =============================================================================

proptest! {
    #[test]
    fn submit_generates_unique_ids(
        n in 1..20usize,
    ) {
        let mut tracker = ApprovalTracker::new(100);
        let mut ids = Vec::new();
        for i in 0..n {
            let id = tracker.submit(
                "read_output", "robot", &format!("pane-{i}"),
                "test reason", "rule.1", 1000 + i as u64, 0,
            );
            ids.push(id);
        }
        // All IDs must be unique
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), n, "all submitted approval IDs must be unique");
    }

    #[test]
    fn submit_increments_len(
        n in 0..10usize,
    ) {
        let mut tracker = ApprovalTracker::new(100);
        for i in 0..n {
            tracker.submit("action", "actor", "resource", "reason", "rule", i as u64, 0);
        }
        prop_assert_eq!(tracker.len(), n);
        let check_empty = n == 0;
        prop_assert_eq!(tracker.is_empty(), check_empty);
    }

    #[test]
    fn approve_transitions_pending_to_approved(
        decided_by in "[a-z]{1,10}",
        now in any::<u64>(),
    ) {
        let mut tracker = ApprovalTracker::new(100);
        let id = tracker.submit("action", "actor", "resource", "reason", "rule", 1000, 0);
        prop_assert!(tracker.approve(&id, &decided_by, now));
        let entry = tracker.get(&id).unwrap();
        prop_assert_eq!(entry.status.clone(), ApprovalStatus::Approved);
        prop_assert_eq!(entry.decided_by.clone(), decided_by);
        prop_assert_eq!(entry.decided_at_ms, now);
    }

    #[test]
    fn reject_transitions_pending_to_rejected(
        decided_by in "[a-z]{1,10}",
        now in any::<u64>(),
    ) {
        let mut tracker = ApprovalTracker::new(100);
        let id = tracker.submit("action", "actor", "resource", "reason", "rule", 1000, 0);
        prop_assert!(tracker.reject(&id, &decided_by, now));
        let entry = tracker.get(&id).unwrap();
        prop_assert_eq!(entry.status.clone(), ApprovalStatus::Rejected);
    }

    #[test]
    fn revoke_transitions_approved_to_revoked(
        decided_by in "[a-z]{1,10}",
        now in any::<u64>(),
    ) {
        let mut tracker = ApprovalTracker::new(100);
        let id = tracker.submit("action", "actor", "resource", "reason", "rule", 1000, 0);
        tracker.approve(&id, "operator", 2000);
        prop_assert!(tracker.revoke(&id, &decided_by, now));
        let entry = tracker.get(&id).unwrap();
        prop_assert_eq!(entry.status.clone(), ApprovalStatus::Revoked);
    }

    #[test]
    fn cannot_approve_non_pending(
        status in prop_oneof![
            Just(ApprovalStatus::Approved),
            Just(ApprovalStatus::Rejected),
            Just(ApprovalStatus::Expired),
            Just(ApprovalStatus::Revoked),
        ],
    ) {
        let mut tracker = ApprovalTracker::new(100);
        let id = tracker.submit("action", "actor", "resource", "reason", "rule", 1000, 0);
        // Transition to the target status first
        match status {
            ApprovalStatus::Approved => { tracker.approve(&id, "op", 2000); },
            ApprovalStatus::Rejected => { tracker.reject(&id, "op", 2000); },
            ApprovalStatus::Expired => { tracker.expire_stale(u64::MAX); },
            ApprovalStatus::Revoked => {
                tracker.approve(&id, "op", 2000);
                tracker.revoke(&id, "op", 3000);
            },
            ApprovalStatus::Pending => {},
        }
        // Second approve attempt should fail
        prop_assert!(!tracker.approve(&id, "op2", 5000));
    }

    #[test]
    fn expire_stale_only_affects_pending_with_deadline(
        n_pending in 1..5usize,
        deadline_ms in 1000..5000u64,
    ) {
        let mut tracker = ApprovalTracker::new(100);
        // Submit entries with expiry at deadline_ms
        for i in 0..n_pending {
            tracker.submit(
                "action", "actor", &format!("res-{i}"),
                "reason", "rule", 500, deadline_ms,
            );
        }
        // Also submit one without expiry (expires_at_ms = 0)
        tracker.submit("action", "actor", "no-expiry", "reason", "rule", 500, 0);

        let expired = tracker.expire_stale(deadline_ms);
        prop_assert_eq!(expired, n_pending, "all pending with deadline should expire");
        prop_assert_eq!(
            tracker.count_by_status(&ApprovalStatus::Expired),
            n_pending,
        );
        // The no-expiry one should still be pending
        prop_assert_eq!(tracker.count_by_status(&ApprovalStatus::Pending), 1);
    }

    #[test]
    fn eviction_respects_max_entries(
        max in 2..10usize,
        extra in 1..5usize,
    ) {
        let mut tracker = ApprovalTracker::new(max);
        for i in 0..(max + extra) {
            tracker.submit("action", "actor", &format!("r{i}"), "reason", "rule", i as u64, 0);
        }
        prop_assert!(tracker.len() <= max, "tracker must not exceed max_entries");
    }

    #[test]
    fn snapshot_counters_sum_to_total(
        n_approve in 0..3usize,
        n_reject in 0..3usize,
        n_pending in 0..3usize,
    ) {
        let mut tracker = ApprovalTracker::new(100);
        let total = n_approve + n_reject + n_pending;
        let mut ids = Vec::new();
        for i in 0..total {
            ids.push(tracker.submit(
                "action", "actor", &format!("r{i}"),
                "reason", "rule", i as u64, 0,
            ));
        }
        // Approve first n_approve
        for id in ids.iter().take(n_approve) {
            tracker.approve(id, "op", 5000);
        }
        // Reject next n_reject
        for id in ids.iter().skip(n_approve).take(n_reject) {
            tracker.reject(id, "op", 5000);
        }
        let snap = tracker.snapshot();
        prop_assert_eq!(snap.total, total);
        prop_assert_eq!(snap.approved, n_approve);
        prop_assert_eq!(snap.rejected, n_reject);
        prop_assert_eq!(snap.pending, n_pending);
        prop_assert_eq!(snap.expired, 0);
        prop_assert_eq!(snap.revoked, 0);
    }

    #[test]
    fn by_time_range_filters_correctly(
        start_ms in 100..500u64,
        end_ms in 500..1000u64,
    ) {
        let mut tracker = ApprovalTracker::new(100);
        // Submit before range
        tracker.submit("a", "x", "r1", "r", "rule", start_ms - 1, 0);
        // Submit in range
        let in_range_id = tracker.submit("a", "x", "r2", "r", "rule", start_ms + 1, 0);
        // Submit after range
        tracker.submit("a", "x", "r3", "r", "rule", end_ms + 1, 0);

        let results = tracker.by_time_range(start_ms, end_ms);
        prop_assert_eq!(results.len(), 1);
        prop_assert_eq!(results[0].approval_id.clone(), in_range_id);
    }
}

// =============================================================================
// RevocationRecord / RevocationRegistrySnapshot strategies
// =============================================================================

fn arb_revocation_record() -> impl Strategy<Value = RevocationRecord> {
    (
        "[a-z0-9-]{1,20}",  // revocation_id
        "[a-z_]{1,15}",     // resource_type
        "[a-z0-9_]{1,20}",  // resource_id
        "[a-z ]{1,30}",     // reason
        "[a-z_]{1,15}",     // revoked_by
        any::<u64>(),        // revoked_at_ms
        any::<bool>(),       // active
    )
        .prop_map(|(id, rtype, rid, reason, by, at, active)| RevocationRecord {
            revocation_id: id,
            resource_type: rtype,
            resource_id: rid,
            reason,
            revoked_by: by,
            revoked_at_ms: at,
            active,
        })
}

fn arb_revocation_registry_snapshot() -> impl Strategy<Value = RevocationRegistrySnapshot> {
    (any::<usize>(), any::<usize>(), any::<usize>()).prop_map(|(total, active, max)| {
        RevocationRegistrySnapshot {
            total_records: total,
            active_revocations: active,
            max_records: max,
        }
    })
}

// =============================================================================
// RevocationRecord / RevocationRegistrySnapshot serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn revocation_record_json_roundtrip(rec in arb_revocation_record()) {
        let json = serde_json::to_string(&rec).unwrap();
        let back: RevocationRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rec, back);
    }

    #[test]
    fn revocation_registry_snapshot_json_roundtrip(snap in arb_revocation_registry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: RevocationRegistrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }
}

// =============================================================================
// RevocationRegistry behavioral tests
// =============================================================================

proptest! {
    #[test]
    fn revoke_generates_unique_ids(n in 1..15usize) {
        let mut reg = RevocationRegistry::new(100);
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(reg.revoke("credential", &format!("cred-{i}"), "test", "op", i as u64));
        }
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), n, "all revocation IDs must be unique");
    }

    #[test]
    fn revoke_increments_len(n in 0..10usize) {
        let mut reg = RevocationRegistry::new(100);
        for i in 0..n {
            reg.revoke("session", &format!("s{i}"), "reason", "op", i as u64);
        }
        prop_assert_eq!(reg.len(), n);
        let check_empty = n == 0;
        prop_assert_eq!(reg.is_empty(), check_empty);
    }

    #[test]
    fn revoked_resource_is_detected(
        rtype in "[a-z]{1,10}",
        rid in "[a-z0-9]{1,15}",
    ) {
        let mut reg = RevocationRegistry::new(100);
        prop_assert!(!reg.is_revoked(&rtype, &rid));
        reg.revoke(&rtype, &rid, "reason", "op", 1000);
        prop_assert!(reg.is_revoked(&rtype, &rid));
    }

    #[test]
    fn reinstate_clears_revocation(
        rtype in "[a-z]{1,10}",
        rid in "[a-z0-9]{1,15}",
    ) {
        let mut reg = RevocationRegistry::new(100);
        let rev_id = reg.revoke(&rtype, &rid, "reason", "op", 1000);
        prop_assert!(reg.is_revoked(&rtype, &rid));
        prop_assert!(reg.reinstate(&rev_id));
        prop_assert!(!reg.is_revoked(&rtype, &rid));
    }

    #[test]
    fn reinstate_nonexistent_returns_false(
        bogus_id in "[a-z0-9-]{1,20}",
    ) {
        let mut reg = RevocationRegistry::new(100);
        prop_assert!(!reg.reinstate(&bogus_id));
    }

    #[test]
    fn active_count_tracks_active_only(
        n_revoke in 1..6usize,
        n_reinstate in 0..3usize,
    ) {
        let n_reinstate = n_reinstate.min(n_revoke);
        let mut reg = RevocationRegistry::new(100);
        let mut ids = Vec::new();
        for i in 0..n_revoke {
            ids.push(reg.revoke("cred", &format!("c{i}"), "r", "op", i as u64));
        }
        for id in ids.iter().take(n_reinstate) {
            reg.reinstate(id);
        }
        prop_assert_eq!(reg.active_count(), n_revoke - n_reinstate);
        prop_assert_eq!(reg.len(), n_revoke);
    }

    #[test]
    fn eviction_respects_max_records(
        max in 2..8usize,
        extra in 1..5usize,
    ) {
        let mut reg = RevocationRegistry::new(max);
        for i in 0..(max + extra) {
            reg.revoke("cred", &format!("c{i}"), "r", "op", i as u64);
        }
        prop_assert!(reg.len() <= max, "registry must not exceed max_records");
    }

    #[test]
    fn snapshot_reflects_state(
        n in 1..6usize,
        n_reinstate in 0..3usize,
    ) {
        let n_reinstate = n_reinstate.min(n);
        let mut reg = RevocationRegistry::new(100);
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(reg.revoke("cred", &format!("c{i}"), "r", "op", i as u64));
        }
        for id in ids.iter().take(n_reinstate) {
            reg.reinstate(id);
        }
        let snap = reg.snapshot();
        prop_assert_eq!(snap.total_records, n);
        prop_assert_eq!(snap.active_revocations, n - n_reinstate);
        prop_assert_eq!(snap.max_records, 100);
    }

    #[test]
    fn active_revocation_returns_correct_record(
        rtype in "[a-z]{1,10}",
        rid in "[a-z0-9]{1,15}",
    ) {
        let mut reg = RevocationRegistry::new(100);
        let rev_id = reg.revoke(&rtype, &rid, "test reason", "op", 1000);
        let active = reg.active_revocation(&rtype, &rid);
        prop_assert!(active.is_some());
        prop_assert_eq!(active.unwrap().revocation_id.clone(), rev_id);
        prop_assert_eq!(active.unwrap().reason.as_str(), "test reason");
    }
}
