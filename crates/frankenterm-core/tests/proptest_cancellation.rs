//! Property-based tests for two-phase cancellation and shutdown protocol.
//!
//! Covers:
//! - CancellationToken hierarchical propagation invariants
//! - ShutdownPolicy tier defaults and custom overrides
//! - Finalizer ordering and status transitions
//! - ShutdownCoordinator event emission consistency
//! - Serde roundtrip for all protocol types
//! - Shutdown lifecycle state machine correctness
//! - Grace period expiry detection
//! - Escalation action correctness

use frankenterm_core::cancellation::{
    CancellationToken, EscalationAction, Finalizer, FinalizerAction, FinalizerStatus,
    ShutdownCoordinator, ShutdownPolicy, ShutdownReason,
};
use frankenterm_core::scope_tree::{
    ScopeId, ScopeState, ScopeTier, ScopeTree, register_standard_scopes, well_known,
};
use proptest::prelude::*;
use std::collections::HashMap;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_scope_id() -> impl Strategy<Value = ScopeId> {
    "[a-z][a-z0-9_:]{1,20}".prop_map(ScopeId)
}

fn arb_tier() -> impl Strategy<Value = ScopeTier> {
    prop_oneof![
        Just(ScopeTier::Root),
        Just(ScopeTier::Daemon),
        Just(ScopeTier::Watcher),
        Just(ScopeTier::Worker),
        Just(ScopeTier::Ephemeral),
    ]
}

fn arb_shutdown_reason() -> impl Strategy<Value = ShutdownReason> {
    prop_oneof![
        Just(ShutdownReason::UserRequested),
        Just(ShutdownReason::GracefulTermination),
        (0i64..100_000, 0i64..200_000).prop_map(|(d, e)| ShutdownReason::Timeout {
            deadline_ms: d,
            elapsed_ms: e,
        }),
        (arb_scope_id(), "[a-z ]{1,30}").prop_map(|(id, msg)| ShutdownReason::ChildError {
            child_id: id,
            error_msg: msg,
        }),
        arb_scope_id().prop_map(|id| ShutdownReason::CascadingFailure { origin_id: id }),
        "[a-z_]{1,15}".prop_map(|r| ShutdownReason::ResourceExhausted { resource: r }),
        "[a-z_-]{1,20}".prop_map(|r| ShutdownReason::PolicyViolation { rule: r }),
        arb_scope_id().prop_map(|id| ShutdownReason::ParentShutdown { parent_id: id }),
    ]
}

fn arb_escalation() -> impl Strategy<Value = EscalationAction> {
    prop_oneof![
        Just(EscalationAction::ForceClose),
        (1u64..60_000).prop_map(|ms| EscalationAction::ExtendGrace { extra_ms: ms }),
        Just(EscalationAction::LogAndWait),
    ]
}

fn arb_finalizer_action() -> impl Strategy<Value = FinalizerAction> {
    prop_oneof![
        "[a-z_]{1,15}".prop_map(|name| FinalizerAction::FlushChannel { channel_name: name }),
        "[a-z_]{1,15}".prop_map(|key| FinalizerAction::PersistState { key }),
        (0u64..10000).prop_map(|id| FinalizerAction::CloseConnection { conn_id: id }),
        "[a-z_]{1,15}".prop_map(|id| FinalizerAction::ReleaseResource { resource_id: id }),
        "[a-z_:]{1,20}".prop_map(|p| FinalizerAction::CancelTimers { scope_prefix: p }),
    ]
}

fn arb_finalizer_status() -> impl Strategy<Value = FinalizerStatus> {
    prop_oneof![
        Just(FinalizerStatus::Pending),
        Just(FinalizerStatus::Running),
        (0u64..10000).prop_map(|d| FinalizerStatus::Completed { duration_ms: d }),
        ("[a-z ]{1,20}", 0u64..10000).prop_map(|(e, d)| FinalizerStatus::Failed {
            error: e,
            duration_ms: d,
        }),
        "[a-z ]{1,20}".prop_map(|r| FinalizerStatus::Skipped { reason: r }),
    ]
}

fn arb_policy() -> impl Strategy<Value = ShutdownPolicy> {
    (
        100u64..60_000,
        arb_escalation(),
        any::<bool>(),
        any::<bool>(),
        100u64..30_000,
    )
        .prop_map(
            |(grace, escalation, cascade, run_fin, fin_timeout)| ShutdownPolicy {
                grace_period_ms: grace,
                escalation,
                cascade_to_children: cascade,
                run_finalizers: run_fin,
                finalizer_timeout_ms: fin_timeout,
            },
        )
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn shutdown_reason_serde_roundtrip(reason in arb_shutdown_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let restored: ShutdownReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(reason, restored);
    }

    #[test]
    fn shutdown_policy_serde_roundtrip(policy in arb_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let restored: ShutdownPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(policy, restored);
    }

    #[test]
    fn escalation_action_serde_roundtrip(action in arb_escalation()) {
        let json = serde_json::to_string(&action).unwrap();
        let restored: EscalationAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, restored);
    }

    #[test]
    fn finalizer_action_serde_roundtrip(action in arb_finalizer_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let restored: FinalizerAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, restored);
    }

    #[test]
    fn finalizer_status_serde_roundtrip(status in arb_finalizer_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let restored: FinalizerStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, restored);
    }

    #[test]
    fn cancellation_token_idempotent(
        reason1 in arb_shutdown_reason(),
        reason2 in arb_shutdown_reason()
    ) {
        let token = CancellationToken::new(ScopeId("test".into()));
        let r1_clone = reason1.clone();
        token.cancel(reason1);
        token.cancel(reason2);
        // First cancel wins
        prop_assert_eq!(token.reason().unwrap(), r1_clone);
        prop_assert_eq!(token.generation(), 1);
    }

    #[test]
    fn cancellation_propagates_depth(depth in 1usize..8) {
        let root = CancellationToken::new(ScopeId("root".into()));
        let mut tokens = vec![root.clone()];

        for i in 0..depth {
            let parent = &tokens[i];
            let child = parent.child(ScopeId(format!("child_{i}")));
            tokens.push(child);
        }

        prop_assert!(!root.is_cancelled());
        for t in &tokens[1..] {
            prop_assert!(!t.is_cancelled());
        }

        root.cancel(ShutdownReason::UserRequested);

        prop_assert!(root.is_cancelled());
        for t in &tokens[1..] {
            prop_assert!(t.is_cancelled(), "child should be cancelled");
            let is_parent_shutdown = matches!(t.reason(), Some(ShutdownReason::ParentShutdown { .. }));
            prop_assert!(is_parent_shutdown, "child reason should be ParentShutdown");
        }
    }

    #[test]
    fn cancellation_child_independence(n_children in 1usize..10) {
        let parent = CancellationToken::new(ScopeId("parent".into()));
        let mut children = Vec::new();
        for i in 0..n_children {
            children.push(parent.child(ScopeId(format!("child_{i}"))));
        }

        // Cancelling one child doesn't affect others
        if !children.is_empty() {
            children[0].cancel(ShutdownReason::UserRequested);
            prop_assert!(children[0].is_cancelled());
            prop_assert!(!parent.is_cancelled());
            for c in &children[1..] {
                prop_assert!(!c.is_cancelled());
            }
        }
    }

    #[test]
    fn tier_default_policy_grace_ordering(tier_a in arb_tier(), tier_b in arb_tier()) {
        let pa = ShutdownPolicy::for_tier(tier_a);
        let pb = ShutdownPolicy::for_tier(tier_b);

        // Higher-priority tiers (Ephemeral>Worker>Watcher>Daemon>Root) should have
        // shorter grace periods
        if tier_a.shutdown_priority() > tier_b.shutdown_priority() {
            prop_assert!(
                pa.grace_period_ms <= pb.grace_period_ms,
                "tier {:?} (prio {}) should have grace <= tier {:?} (prio {}): {} vs {}",
                tier_a,
                tier_a.shutdown_priority(),
                tier_b,
                tier_b.shutdown_priority(),
                pa.grace_period_ms,
                pb.grace_period_ms,
            );
        }
    }

    #[test]
    fn finalizer_priority_ordering_maintained(
        priorities in proptest::collection::vec(0u32..1000, 1..20)
    ) {
        let mut coord = ShutdownCoordinator::new();
        let scope = ScopeId("test".into());
        coord.register_scope(&scope, ScopeTier::Worker, None).unwrap();

        for (i, prio) in priorities.iter().enumerate() {
            coord.register_finalizer(&scope, Finalizer {
                name: format!("f_{i}"),
                priority: *prio,
                action: FinalizerAction::Custom {
                    action_name: format!("action_{i}"),
                    metadata: HashMap::new(),
                },
                status: FinalizerStatus::Pending,
            }).unwrap();
        }

        let fns = coord.finalizers(&scope);
        for pair in fns.windows(2) {
            prop_assert!(
                pair[0].priority >= pair[1].priority,
                "{} (prio {}) should come before {} (prio {})",
                pair[0].name,
                pair[0].priority,
                pair[1].name,
                pair[1].priority,
            );
        }
    }

    #[test]
    fn coordinator_events_monotonic_timestamps(
        n_scopes in 1usize..5
    ) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let mut coord = ShutdownCoordinator::new();
        coord.register_scope(&ScopeId::root(), ScopeTier::Root, None).unwrap();

        let mut scope_ids = Vec::new();
        for i in 0..n_scopes {
            let id = ScopeId(format!("daemon:test_{i}"));
            tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), format!("d{i}"), 1000).unwrap();
            tree.start(&id, 1100).unwrap();
            coord.register_scope(&id, ScopeTier::Daemon, Some(&ScopeId::root())).unwrap();
            scope_ids.push(id);
        }

        // Shut down each at increasing timestamps
        for (i, id) in scope_ids.iter().enumerate() {
            let ts = 2000 + (i as i64 * 1000);
            let _ = coord.request_shutdown(&mut tree, id, ShutdownReason::UserRequested, ts);
        }

        let events = coord.events();
        for pair in events.windows(2) {
            prop_assert!(
                pair[0].timestamp_ms <= pair[1].timestamp_ms,
                "events should be in timestamp order: {} > {}",
                pair[0].timestamp_ms,
                pair[1].timestamp_ms,
            );
        }
    }

    #[test]
    fn grace_period_detection_consistent(
        grace_ms in 100u64..10_000,
        elapsed_before in 0u64..10_000,
    ) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let id = ScopeId("daemon:gp_test".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "gp", 1000).unwrap();
        tree.start(&id, 1100).unwrap();

        let mut coord = ShutdownCoordinator::new();
        coord.register_scope(&ScopeId::root(), ScopeTier::Root, None).unwrap();
        coord.register_scope(&id, ScopeTier::Daemon, Some(&ScopeId::root())).unwrap();
        coord.set_policy(&id, ShutdownPolicy {
            grace_period_ms: grace_ms,
            ..ShutdownPolicy::for_tier(ScopeTier::Daemon)
        }).unwrap();

        let shutdown_ts = 5000i64;
        coord.request_shutdown(&mut tree, &id, ShutdownReason::UserRequested, shutdown_ts).unwrap();

        let check_ts = shutdown_ts + elapsed_before as i64;
        let is_expired = coord.is_grace_expired(&tree, &id, check_ts);

        if elapsed_before >= grace_ms {
            prop_assert!(is_expired, "should be expired: elapsed {} >= grace {}", elapsed_before, grace_ms);
        } else {
            prop_assert!(!is_expired, "should NOT be expired: elapsed {} < grace {}", elapsed_before, grace_ms);
        }
    }

    #[test]
    fn shutdown_summary_invariants(
        n_finalizers in 0usize..5
    ) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let scope = ScopeId("worker:summary_test".into());
        tree.register(scope.clone(), ScopeTier::Worker, &ScopeId::root(), "test", 1000).unwrap();
        tree.start(&scope, 1100).unwrap();

        let mut coord = ShutdownCoordinator::new();
        coord.register_scope(&ScopeId::root(), ScopeTier::Root, None).unwrap();
        coord.register_scope(&scope, ScopeTier::Worker, Some(&ScopeId::root())).unwrap();

        for i in 0..n_finalizers {
            coord.register_finalizer(&scope, Finalizer {
                name: format!("f_{i}"),
                priority: (n_finalizers - i) as u32,
                action: FinalizerAction::Custom {
                    action_name: format!("a_{i}"),
                    metadata: HashMap::new(),
                },
                status: FinalizerStatus::Pending,
            }).unwrap();
        }

        // Full lifecycle
        coord.request_shutdown(&mut tree, &scope, ShutdownReason::GracefulTermination, 2000).unwrap();
        coord.begin_finalize(&mut tree, &scope, 500, 2500).unwrap();

        // Run all finalizers
        for i in 0..n_finalizers {
            let name = format!("f_{i}");
            coord.mark_finalizer_started(&scope, &name, 2500 + i as i64 * 100).unwrap();
            coord.mark_finalizer_completed(&scope, &name, 50, 2550 + i as i64 * 100).unwrap();
        }

        let summary = coord.complete_shutdown(&mut tree, &scope, 3000).unwrap();

        // Invariants
        prop_assert_eq!(summary.finalizers_run, n_finalizers);
        prop_assert_eq!(summary.finalizers_succeeded, n_finalizers);
        prop_assert_eq!(summary.finalizers_failed, 0);
        prop_assert_eq!(summary.finalizers_skipped, 0);
        prop_assert!(summary.total_elapsed_ms > 0);
        prop_assert_eq!(summary.reason, ShutdownReason::GracefulTermination);
        prop_assert!(!summary.escalated);
    }
}

// ── Non-proptest structural tests ──────────────────────────────────────────

#[test]
fn all_shutdown_reasons_display_non_empty() {
    let reasons = vec![
        ShutdownReason::UserRequested,
        ShutdownReason::GracefulTermination,
        ShutdownReason::Timeout {
            deadline_ms: 0,
            elapsed_ms: 0,
        },
        ShutdownReason::ChildError {
            child_id: ScopeId("x".into()),
            error_msg: "e".into(),
        },
        ShutdownReason::CascadingFailure {
            origin_id: ScopeId("o".into()),
        },
        ShutdownReason::ResourceExhausted {
            resource: "r".into(),
        },
        ShutdownReason::PolicyViolation { rule: "p".into() },
        ShutdownReason::ParentShutdown {
            parent_id: ScopeId("p".into()),
        },
    ];

    for r in reasons {
        let display = r.to_string();
        assert!(
            !display.is_empty(),
            "display should not be empty for {:?}",
            r
        );
    }
}

#[test]
fn all_escalation_actions_display_non_empty() {
    let actions = vec![
        EscalationAction::ForceClose,
        EscalationAction::ExtendGrace { extra_ms: 100 },
        EscalationAction::LogAndWait,
    ];
    for a in actions {
        assert!(!a.to_string().is_empty());
    }
}

#[test]
fn all_finalizer_statuses_display_non_empty() {
    let statuses = vec![
        FinalizerStatus::Pending,
        FinalizerStatus::Running,
        FinalizerStatus::Completed { duration_ms: 100 },
        FinalizerStatus::Failed {
            error: "err".into(),
            duration_ms: 50,
        },
        FinalizerStatus::Skipped {
            reason: "test".into(),
        },
    ];
    for s in statuses {
        assert!(!s.to_string().is_empty());
    }
}

#[test]
fn coordinator_canonical_string_updates() {
    let mut coord = ShutdownCoordinator::new();
    let s1 = coord.canonical_string();
    assert!(s1.contains("scopes=0"));

    coord
        .register_scope(&ScopeId("a".into()), ScopeTier::Worker, None)
        .unwrap();
    let s2 = coord.canonical_string();
    assert!(s2.contains("scopes=1"));
    assert_ne!(s1, s2);
}

#[test]
fn full_two_phase_with_cascade_and_finalizers() {
    let mut tree = ScopeTree::new(1000);
    tree.start(&ScopeId::root(), 1000).unwrap();
    register_standard_scopes(&mut tree, 1000).unwrap();

    // Start capture daemon
    tree.start(&well_known::capture(), 1100).unwrap();

    // Add 2 workers
    for i in 0..2 {
        tree.register(
            well_known::capture_worker(i),
            ScopeTier::Worker,
            &well_known::capture(),
            format!("w{i}"),
            1200,
        )
        .unwrap();
        tree.start(&well_known::capture_worker(i), 1300).unwrap();
    }

    let mut coord = ShutdownCoordinator::new();
    coord.set_correlation_prefix("e2e-test");
    coord
        .register_scope(&ScopeId::root(), ScopeTier::Root, None)
        .unwrap();
    coord
        .register_scope(
            &well_known::capture(),
            ScopeTier::Daemon,
            Some(&ScopeId::root()),
        )
        .unwrap();
    for i in 0..2 {
        coord
            .register_scope(
                &well_known::capture_worker(i),
                ScopeTier::Worker,
                Some(&well_known::capture()),
            )
            .unwrap();

        coord
            .register_finalizer(
                &well_known::capture_worker(i),
                Finalizer {
                    name: format!("flush-w{i}"),
                    priority: 100,
                    action: FinalizerAction::FlushChannel {
                        channel_name: format!("capture-{i}"),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();
    }

    // Phase 1: Shutdown capture daemon — cascades to workers
    let cascaded = coord
        .request_shutdown(
            &mut tree,
            &well_known::capture(),
            ShutdownReason::UserRequested,
            5000,
        )
        .unwrap();
    assert_eq!(cascaded.len(), 2, "should cascade to both workers");

    // Workers draining
    for i in 0..2 {
        assert_eq!(
            tree.get(&well_known::capture_worker(i)).unwrap().state,
            ScopeState::Draining
        );
    }

    // Close workers (bottom-up)
    for i in 0..2 {
        coord
            .begin_finalize(&mut tree, &well_known::capture_worker(i), 200, 5200)
            .unwrap();

        let fname = format!("flush-w{i}");
        coord
            .mark_finalizer_started(&well_known::capture_worker(i), &fname, 5200)
            .unwrap();
        coord
            .mark_finalizer_completed(&well_known::capture_worker(i), &fname, 30, 5230)
            .unwrap();

        let summary = coord
            .complete_shutdown(&mut tree, &well_known::capture_worker(i), 5250)
            .unwrap();
        assert_eq!(summary.finalizers_succeeded, 1);
    }

    // Now capture daemon can finalize
    coord
        .begin_finalize(&mut tree, &well_known::capture(), 250, 5250)
        .unwrap();
    let summary = coord
        .complete_shutdown(&mut tree, &well_known::capture(), 5300)
        .unwrap();
    assert_eq!(summary.cascaded_children, 2);
    assert_eq!(
        tree.get(&well_known::capture()).unwrap().state,
        ScopeState::Closed
    );

    // Verify events
    let all_events = coord.events();
    assert!(
        all_events.len() >= 10,
        "should have many events: {}",
        all_events.len()
    );

    // All events should have correlation IDs
    for event in all_events {
        assert!(event.correlation_id.is_some());
    }
}

#[test]
fn force_close_skips_finalizers() {
    let mut tree = ScopeTree::new(1000);
    tree.start(&ScopeId::root(), 1000).unwrap();

    let scope = ScopeId("daemon:fc_test".into());
    tree.register(
        scope.clone(),
        ScopeTier::Daemon,
        &ScopeId::root(),
        "fc",
        1000,
    )
    .unwrap();
    tree.start(&scope, 1100).unwrap();

    let mut coord = ShutdownCoordinator::new();
    coord
        .register_scope(&ScopeId::root(), ScopeTier::Root, None)
        .unwrap();
    coord
        .register_scope(&scope, ScopeTier::Daemon, Some(&ScopeId::root()))
        .unwrap();

    // Set ultra-short grace with force-close escalation
    coord
        .set_policy(
            &scope,
            ShutdownPolicy {
                grace_period_ms: 50,
                escalation: EscalationAction::ForceClose,
                cascade_to_children: false,
                run_finalizers: true,
                finalizer_timeout_ms: 1000,
            },
        )
        .unwrap();

    // Register a finalizer
    coord
        .register_finalizer(
            &scope,
            Finalizer {
                name: "important-flush".into(),
                priority: 100,
                action: FinalizerAction::PersistState {
                    key: "state".into(),
                },
                status: FinalizerStatus::Pending,
            },
        )
        .unwrap();

    // Request shutdown
    coord
        .request_shutdown(&mut tree, &scope, ShutdownReason::UserRequested, 5000)
        .unwrap();

    // Grace expired → force close
    coord.handle_grace_expiry(&mut tree, &scope, 5100).unwrap();

    // Scope should be closed, finalizer should be skipped
    assert_eq!(tree.get(&scope).unwrap().state, ScopeState::Closed);
    let fns = coord.finalizers(&scope);
    let is_skipped = matches!(fns[0].status, FinalizerStatus::Skipped { .. });
    assert!(is_skipped, "finalizer should be skipped on force-close");
}

#[test]
fn shutdown_reason_equality() {
    assert_eq!(ShutdownReason::UserRequested, ShutdownReason::UserRequested);
    assert_ne!(
        ShutdownReason::UserRequested,
        ShutdownReason::GracefulTermination
    );
}

#[test]
fn coordinator_cancelled_count_tracks() {
    let mut coord = ShutdownCoordinator::new();
    let s1 = ScopeId("s1".into());
    let s2 = ScopeId("s2".into());
    let s3 = ScopeId("s3".into());

    let t1 = coord.register_scope(&s1, ScopeTier::Worker, None).unwrap();
    let _t2 = coord.register_scope(&s2, ScopeTier::Worker, None).unwrap();
    let _t3 = coord.register_scope(&s3, ScopeTier::Worker, None).unwrap();

    assert_eq!(coord.cancelled_count(), 0);

    t1.cancel(ShutdownReason::UserRequested);
    assert_eq!(coord.cancelled_count(), 1);
}
