//! Property-based tests for scope_tree structured concurrency model.
//!
//! Covers:
//! - Scope registration invariants (parent exists, no duplicates)
//! - Lifecycle state machine validity (Created→Running→Draining→Finalizing→Closed)
//! - Shutdown ordering (deepest-first, priority-based)
//! - Tree structural invariants (parent-child bidirectional, root always exists)
//! - Serde roundtrip for all types
//! - Snapshot consistency with tree state
//! - Canonical string determinism

use frankenterm_core::scope_tree::{
    ScopeHandle, ScopeId, ScopeState, ScopeTier, ScopeTree, ScopeTreeError, ScopeTreeSnapshot,
    register_standard_scopes, well_known,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_child_tier() -> impl Strategy<Value = ScopeTier> {
    prop_oneof![
        Just(ScopeTier::Daemon),
        Just(ScopeTier::Watcher),
        Just(ScopeTier::Worker),
        Just(ScopeTier::Ephemeral),
    ]
}

fn arb_scope_id() -> impl Strategy<Value = ScopeId> {
    "[a-z][a-z0-9_]{1,10}".prop_map(ScopeId)
}

fn arb_timestamp() -> impl Strategy<Value = i64> {
    1000i64..1_000_000i64
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tree_always_has_root(ts in arb_timestamp()) {
        let tree = ScopeTree::new(ts);
        prop_assert!(tree.get(&ScopeId::root()).is_some());
        prop_assert_eq!(tree.root().tier, ScopeTier::Root);
        prop_assert_eq!(tree.len(), 1);
    }

    #[test]
    fn register_child_preserves_invariants(
        id in arb_scope_id(),
        tier in arb_child_tier(),
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        // Daemons and watchers can be children of root
        let parent_tier = tree.root().tier;
        prop_assert!(parent_tier.can_have_children());

        // Workers and ephemeral can only go under daemon/watcher parents
        if matches!(tier, ScopeTier::Worker | ScopeTier::Ephemeral) {
            // Register a daemon parent first
            let daemon_id = ScopeId("daemon_parent".into());
            tree.register(daemon_id.clone(), ScopeTier::Daemon, &ScopeId::root(), "parent", ts).unwrap();
            tree.register(id.clone(), tier, &daemon_id, "child", ts).unwrap();
        } else {
            tree.register(id.clone(), tier, &ScopeId::root(), "child", ts).unwrap();
        }

        let node = tree.get(&id).unwrap();
        prop_assert_eq!(node.tier, tier);
        prop_assert_eq!(node.state, ScopeState::Created);
    }

    #[test]
    fn duplicate_registration_rejected(
        id in arb_scope_id(),
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "first", ts).unwrap();
        let result = tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "second", ts);
        let is_dup = matches!(result, Err(ScopeTreeError::DuplicateScope { .. }));
        prop_assert!(is_dup);
    }

    #[test]
    fn lifecycle_roundtrip(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("test_scope".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "test", ts).unwrap();

        // Created → Running
        tree.start(&id, ts + 100).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Running);

        // Running → Draining
        tree.request_shutdown(&id, ts + 200).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Draining);

        // Draining → Finalizing (no children)
        tree.finalize(&id).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Finalizing);

        // Finalizing → Closed
        tree.close(&id, ts + 300).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Closed);
        let is_terminal = tree.get(&id).unwrap().state.is_terminal();
        prop_assert!(is_terminal);
    }

    #[test]
    fn shutdown_order_children_before_parents(
        num_workers in 0usize..5,
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        let daemon_id = ScopeId("daemon_host".into());
        tree.register(daemon_id.clone(), ScopeTier::Daemon, &ScopeId::root(), "host", ts).unwrap();
        tree.start(&ScopeId::root(), ts).unwrap();
        tree.start(&daemon_id, ts + 1).unwrap();

        for i in 0..num_workers {
            let wid = ScopeId(format!("worker_{i}"));
            tree.register(wid.clone(), ScopeTier::Worker, &daemon_id, format!("w{i}"), ts + 2).unwrap();
            tree.start(&wid, ts + 3).unwrap();
        }

        let order = tree.shutdown_order();

        // Workers must appear before their parent daemon
        let daemon_pos = order.iter().position(|id| *id == daemon_id);
        for i in 0..num_workers {
            let wid = ScopeId(format!("worker_{i}"));
            let worker_pos = order.iter().position(|id| *id == wid);
            if let (Some(wp), Some(dp)) = (worker_pos, daemon_pos) {
                prop_assert!(wp < dp, "worker {} at pos {} must be before daemon at pos {}", i, wp, dp);
            }
        }
    }

    #[test]
    fn depth_monotonically_increases_in_tree(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();

        // Add workers under capture
        let capture = well_known::capture();
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &capture,
            "w0",
            ts,
        ).unwrap();

        // Root at depth 0
        prop_assert_eq!(tree.depth(&ScopeId::root()), 0);
        // Daemons at depth 1
        prop_assert_eq!(tree.depth(&capture), 1);
        // Workers at depth 2
        prop_assert_eq!(tree.depth(&well_known::capture_worker(0)), 2);
    }

    #[test]
    fn snapshot_matches_tree_state(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();

        let snap = tree.snapshot();
        prop_assert_eq!(snap.total_scopes, tree.len());
        prop_assert_eq!(snap.daemons, tree.count_by_tier(ScopeTier::Daemon));
        prop_assert_eq!(snap.watchers, tree.count_by_tier(ScopeTier::Watcher));
        prop_assert_eq!(snap.workers, tree.count_by_tier(ScopeTier::Worker));
        prop_assert_eq!(snap.ephemeral, tree.count_by_tier(ScopeTier::Ephemeral));
    }

    #[test]
    fn serde_roundtrip_tree(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        tree.start(&ScopeId::root(), ts + 100).unwrap();

        let json = serde_json::to_string(&tree).unwrap();
        let restored: ScopeTree = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(tree.len(), restored.len());
        prop_assert_eq!(tree.canonical_string(), restored.canonical_string());
    }

    #[test]
    fn serde_roundtrip_snapshot(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();

        let snap = tree.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let restored: ScopeTreeSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, restored);
    }

    #[test]
    fn canonical_string_is_deterministic(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();

        let s1 = tree.canonical_string();
        let s2 = tree.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn scope_handle_shutdown_flag_coherent(id in arb_scope_id()) {
        let handle = ScopeHandle::new(id);
        prop_assert!(!handle.is_shutdown_requested());
        let gen_before = handle.current_generation();

        handle.request_shutdown();
        prop_assert!(handle.is_shutdown_requested());
        prop_assert_eq!(handle.current_generation(), gen_before + 1);
    }

    #[test]
    fn finalize_blocked_by_live_children(
        num_children in 1usize..5,
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        let parent = ScopeId("parent_daemon".into());
        tree.register(parent.clone(), ScopeTier::Daemon, &ScopeId::root(), "parent", ts).unwrap();
        tree.start(&parent, ts + 1).unwrap();

        for i in 0..num_children {
            let cid = ScopeId(format!("child_{i}"));
            tree.register(cid.clone(), ScopeTier::Worker, &parent, format!("c{i}"), ts + 2).unwrap();
            tree.start(&cid, ts + 3).unwrap();
        }

        tree.request_shutdown(&parent, ts + 100).unwrap();
        let result = tree.finalize(&parent);
        let has_live = matches!(result, Err(ScopeTreeError::HasLiveChildren { .. }));
        prop_assert!(has_live, "finalize must fail with {} live children", num_children);
    }

    #[test]
    fn descendants_count_consistent(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();

        // Root descendants should be all non-root nodes
        let desc = tree.descendants(&ScopeId::root());
        prop_assert_eq!(desc.len(), tree.len() - 1);
    }

}

#[test]
fn tier_shutdown_priority_ordering() {
    // Ephemeral > Worker > Watcher > Daemon > Root
    assert!(ScopeTier::Ephemeral.shutdown_priority() > ScopeTier::Worker.shutdown_priority());
    assert!(ScopeTier::Worker.shutdown_priority() > ScopeTier::Watcher.shutdown_priority());
    assert!(ScopeTier::Watcher.shutdown_priority() > ScopeTier::Daemon.shutdown_priority());
    assert!(ScopeTier::Daemon.shutdown_priority() > ScopeTier::Root.shutdown_priority());
}
