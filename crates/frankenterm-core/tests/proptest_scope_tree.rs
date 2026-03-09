//! Property-based tests for scope_tree structured concurrency model.
//!
//! Covers:
//! - Scope registration invariants (parent exists, no duplicates)
//! - Lifecycle state machine validity (Created→Running→Draining→Finalizing→Closed)
//! - Shutdown ordering (deepest-first, priority-based, LIFO)
//! - Tree structural invariants (parent-child bidirectional, root always exists)
//! - Serde roundtrip for all types
//! - Snapshot consistency with tree state
//! - Canonical string determinism
//! - ScopeId, ScopeTier, ScopeState enum properties
//! - ScopeHandle thread-safe shutdown coordination
//! - Error paths and invalid transitions
//! - well_known scope ID format correctness

use frankenterm_core::scope_tree::{
    ScopeHandle, ScopeId, ScopeNode, ScopeState, ScopeTier, ScopeTree, ScopeTreeError,
    ScopeTreeSnapshot, register_standard_scopes, well_known,
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

fn arb_all_tiers() -> impl Strategy<Value = ScopeTier> {
    prop_oneof![
        Just(ScopeTier::Root),
        Just(ScopeTier::Daemon),
        Just(ScopeTier::Watcher),
        Just(ScopeTier::Worker),
        Just(ScopeTier::Ephemeral),
    ]
}

fn arb_all_states() -> impl Strategy<Value = ScopeState> {
    prop_oneof![
        Just(ScopeState::Created),
        Just(ScopeState::Running),
        Just(ScopeState::Draining),
        Just(ScopeState::Finalizing),
        Just(ScopeState::Closed),
    ]
}

fn arb_scope_id() -> impl Strategy<Value = ScopeId> {
    "[a-z][a-z0-9_]{1,10}".prop_map(ScopeId)
}

fn arb_timestamp() -> impl Strategy<Value = i64> {
    1000i64..1_000_000i64
}

// ── ScopeId Properties ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ST-01: ScopeId::from_path joins components with ':'
    #[test]
    fn st01_scope_id_from_path(
        a in "[a-z]{2,6}",
        b in "[a-z]{2,6}",
        c in "[a-z]{2,6}",
    ) {
        let id = ScopeId::from_path(&[&a, &b, &c]);
        let expected = format!("{a}:{b}:{c}");
        prop_assert_eq!(&id.0, &expected);
    }

    /// ST-02: ScopeId::root() is_root and Display is "root"
    #[test]
    fn st02_root_id_properties(_dummy in 0u8..1) {
        let root = ScopeId::root();
        prop_assert!(root.is_root());
        prop_assert_eq!(format!("{root}"), "root");
    }

    /// ST-03: Non-root ScopeId is not root
    #[test]
    fn st03_non_root_is_not_root(name in "[a-z]{2,10}") {
        prop_assume!(name != "root");
        let id = ScopeId(name);
        prop_assert!(!id.is_root());
    }

    /// ST-04: ScopeId serde roundtrip
    #[test]
    fn st04_scope_id_serde(name in "[a-z:_]{2,20}") {
        let id = ScopeId(name);
        let json = serde_json::to_string(&id).unwrap();
        let back: ScopeId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(id, back);
    }

    /// ST-05: ScopeId Display matches inner string
    #[test]
    fn st05_scope_id_display(name in "[a-z:_]{2,20}") {
        let id = ScopeId(name.clone());
        prop_assert_eq!(format!("{id}"), name);
    }
}

// ── ScopeTier Properties ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ST-06: ScopeTier shutdown priority is strictly ordered
    #[test]
    fn st06_tier_priority_strict_order(_dummy in 0u8..1) {
        let tiers = [
            ScopeTier::Root,
            ScopeTier::Daemon,
            ScopeTier::Watcher,
            ScopeTier::Worker,
            ScopeTier::Ephemeral,
        ];
        for pair in tiers.windows(2) {
            prop_assert!(pair[0].shutdown_priority() < pair[1].shutdown_priority());
        }
    }

    /// ST-07: can_have_children is true for Root/Daemon/Watcher, false for Worker/Ephemeral
    #[test]
    fn st07_can_have_children_partitions(tier in arb_all_tiers()) {
        let expected = matches!(tier, ScopeTier::Root | ScopeTier::Daemon | ScopeTier::Watcher);
        prop_assert_eq!(tier.can_have_children(), expected);
    }

    /// ST-08: ScopeTier Display is lowercase and non-empty
    #[test]
    fn st08_tier_display(tier in arb_all_tiers()) {
        let display = format!("{tier}");
        prop_assert!(!display.is_empty());
        prop_assert!(display.chars().all(|c| c.is_lowercase() || c == '_'));
    }

    /// ST-09: ScopeTier serde roundtrip
    #[test]
    fn st09_tier_serde(tier in arb_all_tiers()) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: ScopeTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }
}

// ── ScopeState Properties ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ST-10: Only Closed is terminal
    #[test]
    fn st10_only_closed_is_terminal(state in arb_all_states()) {
        let expected = state == ScopeState::Closed;
        prop_assert_eq!(state.is_terminal(), expected);
    }

    /// ST-11: Only Draining and Finalizing are shutting down
    #[test]
    fn st11_shutting_down_states(state in arb_all_states()) {
        let expected = matches!(state, ScopeState::Draining | ScopeState::Finalizing);
        prop_assert_eq!(state.is_shutting_down(), expected);
    }

    /// ST-12: Only Created and Running accept children
    #[test]
    fn st12_accepts_children_states(state in arb_all_states()) {
        let expected = matches!(state, ScopeState::Created | ScopeState::Running);
        prop_assert_eq!(state.accepts_children(), expected);
    }

    /// ST-13: ScopeState Display is lowercase and non-empty
    #[test]
    fn st13_state_display(state in arb_all_states()) {
        let display = format!("{state}");
        prop_assert!(!display.is_empty());
        prop_assert!(display.chars().all(|c| c.is_lowercase()));
    }

    /// ST-14: ScopeState serde roundtrip
    #[test]
    fn st14_state_serde(state in arb_all_states()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: ScopeState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, back);
    }
}

// ── ScopeNode Properties ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ST-15: New ScopeNode starts in Created state with no timestamps set
    #[test]
    fn st15_new_node_defaults(
        name in "[a-z]{3,10}",
        tier in arb_child_tier(),
        ts in arb_timestamp(),
    ) {
        let id = ScopeId(name);
        let node = ScopeNode::new(id.clone(), tier, Some(ScopeId::root()), "desc", ts);
        prop_assert_eq!(node.state, ScopeState::Created);
        prop_assert_eq!(node.created_at_ms, ts);
        prop_assert!(node.started_at_ms.is_none());
        prop_assert!(node.shutdown_requested_at_ms.is_none());
        prop_assert!(node.closed_at_ms.is_none());
        prop_assert!(node.children.is_empty());
        prop_assert!(node.tags.is_empty());
    }

    /// ST-16: ScopeNode canonical_string is deterministic
    #[test]
    fn st16_node_canonical_deterministic(
        name in "[a-z]{3,10}",
        tier in arb_child_tier(),
        ts in arb_timestamp(),
    ) {
        let node = ScopeNode::new(
            ScopeId(name),
            tier,
            Some(ScopeId::root()),
            "test",
            ts,
        );
        let s1 = node.canonical_string();
        let s2 = node.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    /// ST-17: ScopeNode canonical_string includes key fields
    #[test]
    fn st17_node_canonical_contains_fields(
        name in "[a-z]{3,10}",
        ts in arb_timestamp(),
    ) {
        let id = ScopeId(name.clone());
        let node = ScopeNode::new(id, ScopeTier::Daemon, Some(ScopeId::root()), "desc", ts);
        let canonical = node.canonical_string();
        let expected_id = format!("scope_id={name}");
        let expected_ts = format!("created={ts}");
        prop_assert!(canonical.contains(&expected_id));
        prop_assert!(canonical.contains("tier=daemon"));
        prop_assert!(canonical.contains("state=created"));
        prop_assert!(canonical.contains("parent=root"));
        prop_assert!(canonical.contains(&expected_ts));
    }

    /// ST-18: ScopeNode serde roundtrip preserves all fields
    #[test]
    fn st18_node_serde_roundtrip(
        name in "[a-z]{3,10}",
        tier in arb_child_tier(),
        ts in arb_timestamp(),
        desc in "[a-z ]{5,30}",
    ) {
        let node = ScopeNode::new(
            ScopeId(name),
            tier,
            Some(ScopeId::root()),
            &desc,
            ts,
        );
        let json = serde_json::to_string(&node).unwrap();
        let back: ScopeNode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(node.id, back.id);
        prop_assert_eq!(node.tier, back.tier);
        prop_assert_eq!(node.state, back.state);
        prop_assert_eq!(node.parent_id, back.parent_id);
        prop_assert_eq!(node.created_at_ms, back.created_at_ms);
        prop_assert_eq!(node.description, back.description);
    }
}

// ── Tree Registration & Structure ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ST-19: Tree always has root
    #[test]
    fn st19_tree_always_has_root(ts in arb_timestamp()) {
        let tree = ScopeTree::new(ts);
        prop_assert!(tree.get(&ScopeId::root()).is_some());
        prop_assert_eq!(tree.root().tier, ScopeTier::Root);
        prop_assert_eq!(tree.len(), 1);
    }

    /// ST-20: New tree is_empty (only root)
    #[test]
    fn st20_new_tree_is_empty(ts in arb_timestamp()) {
        let tree = ScopeTree::new(ts);
        prop_assert!(tree.is_empty());
    }

    /// ST-21: Tree with children is not empty
    #[test]
    fn st21_tree_with_children_not_empty(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        tree.register(ScopeId("d".into()), ScopeTier::Daemon, &ScopeId::root(), "d", ts).unwrap();
        prop_assert!(!tree.is_empty());
    }

    /// ST-22: Register increments len by 1
    #[test]
    fn st22_register_increments_len(
        n in 1usize..8,
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        for i in 0..n {
            let id = ScopeId(format!("daemon_{i}"));
            tree.register(id, ScopeTier::Daemon, &ScopeId::root(), format!("d{i}"), ts).unwrap();
        }
        prop_assert_eq!(tree.len(), n + 1); // +1 for root
    }

    /// ST-23: Register child preserves invariants
    #[test]
    fn st23_register_child_preserves_invariants(
        id in arb_scope_id(),
        tier in arb_child_tier(),
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        if matches!(tier, ScopeTier::Worker | ScopeTier::Ephemeral) {
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

    /// ST-24: Duplicate registration rejected
    #[test]
    fn st24_duplicate_registration_rejected(
        id in arb_scope_id(),
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "first", ts).unwrap();
        let result = tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "second", ts);
        let is_dup = matches!(result, Err(ScopeTreeError::DuplicateScope { .. }));
        prop_assert!(is_dup);
    }

    /// ST-25: Register under missing parent rejected
    #[test]
    fn st25_missing_parent_rejected(
        id in arb_scope_id(),
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        let result = tree.register(id, ScopeTier::Worker, &ScopeId("ghost".into()), "orphan", ts);
        let is_not_found = matches!(result, Err(ScopeTreeError::ParentNotFound { .. }));
        prop_assert!(is_not_found);
    }

    /// ST-26: Worker tier cannot have children
    #[test]
    fn st26_worker_cannot_have_children(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let daemon_id = ScopeId("d".into());
        let worker_id = ScopeId("w".into());
        tree.register(daemon_id.clone(), ScopeTier::Daemon, &ScopeId::root(), "d", ts).unwrap();
        tree.register(worker_id.clone(), ScopeTier::Worker, &daemon_id, "w", ts).unwrap();
        let result = tree.register(ScopeId("sub".into()), ScopeTier::Ephemeral, &worker_id, "s", ts);
        let is_tier_err = matches!(result, Err(ScopeTreeError::TierCannotHaveChildren { .. }));
        prop_assert!(is_tier_err);
    }

    /// ST-27: Ephemeral tier cannot have children
    #[test]
    fn st27_ephemeral_cannot_have_children(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        tree.start(&ScopeId::root(), ts).unwrap();
        let eph_id = ScopeId("eph".into());
        tree.register(eph_id.clone(), ScopeTier::Ephemeral, &ScopeId::root(), "e", ts).unwrap();
        let result = tree.register(ScopeId("sub".into()), ScopeTier::Ephemeral, &eph_id, "s", ts);
        let is_tier_err = matches!(result, Err(ScopeTreeError::TierCannotHaveChildren { .. }));
        prop_assert!(is_tier_err);
    }

    /// ST-28: Register under draining parent rejected
    #[test]
    fn st28_register_under_draining_parent(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("d".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "d", ts).unwrap();
        tree.start(&id, ts + 1).unwrap();
        tree.request_shutdown(&id, ts + 2).unwrap();
        let result = tree.register(ScopeId("child".into()), ScopeTier::Worker, &id, "c", ts + 3);
        let is_not_accepting = matches!(result, Err(ScopeTreeError::ParentNotAccepting { .. }));
        prop_assert!(is_not_accepting);
    }
}

// ── Lifecycle State Machine ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ST-29: Full lifecycle Created→Running→Draining→Finalizing→Closed
    #[test]
    fn st29_full_lifecycle(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("test_scope".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "test", ts).unwrap();

        tree.start(&id, ts + 100).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Running);
        prop_assert_eq!(tree.get(&id).unwrap().started_at_ms, Some(ts + 100));

        tree.request_shutdown(&id, ts + 200).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Draining);
        prop_assert_eq!(tree.get(&id).unwrap().shutdown_requested_at_ms, Some(ts + 200));

        tree.finalize(&id).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Finalizing);

        tree.close(&id, ts + 300).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Closed);
        prop_assert_eq!(tree.get(&id).unwrap().closed_at_ms, Some(ts + 300));
    }

    /// ST-30: Created→Draining shortcut (skip Running)
    #[test]
    fn st30_created_to_draining(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("s".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "s", ts).unwrap();
        // Can go directly from Created to Draining
        tree.request_shutdown(&id, ts + 100).unwrap();
        prop_assert_eq!(tree.get(&id).unwrap().state, ScopeState::Draining);
    }

    /// ST-31: Cannot start from non-Created state
    #[test]
    fn st31_start_only_from_created(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("s".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "s", ts).unwrap();
        tree.start(&id, ts + 1).unwrap();
        // Cannot start again from Running
        let err = tree.start(&id, ts + 2);
        let is_invalid = matches!(err, Err(ScopeTreeError::InvalidTransition { .. }));
        prop_assert!(is_invalid);
    }

    /// ST-32: Cannot request_shutdown from Draining/Finalizing/Closed
    #[test]
    fn st32_shutdown_only_from_created_or_running(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("s".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "s", ts).unwrap();
        tree.start(&id, ts + 1).unwrap();
        tree.request_shutdown(&id, ts + 2).unwrap();
        // Cannot shutdown again from Draining
        let err = tree.request_shutdown(&id, ts + 3);
        let is_invalid = matches!(err, Err(ScopeTreeError::InvalidTransition { .. }));
        prop_assert!(is_invalid);
    }

    /// ST-33: Cannot finalize from Created/Running/Closed
    #[test]
    fn st33_finalize_only_from_draining(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("s".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "s", ts).unwrap();
        // Cannot finalize from Created
        let err = tree.finalize(&id);
        let is_invalid = matches!(err, Err(ScopeTreeError::InvalidTransition { .. }));
        prop_assert!(is_invalid);
    }

    /// ST-34: Cannot close from non-Finalizing state
    #[test]
    fn st34_close_only_from_finalizing(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("s".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "s", ts).unwrap();
        tree.start(&id, ts + 1).unwrap();
        // Cannot close from Running
        let err = tree.close(&id, ts + 2);
        let is_invalid = matches!(err, Err(ScopeTreeError::InvalidTransition { .. }));
        prop_assert!(is_invalid);
    }

    /// ST-35: Operations on nonexistent scope return ScopeNotFound
    #[test]
    fn st35_nonexistent_scope_errors(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let ghost = ScopeId("ghost".into());
        let start_err = matches!(tree.start(&ghost, ts), Err(ScopeTreeError::ScopeNotFound { .. }));
        let shutdown_err = matches!(tree.request_shutdown(&ghost, ts), Err(ScopeTreeError::ScopeNotFound { .. }));
        let finalize_err = matches!(tree.finalize(&ghost), Err(ScopeTreeError::ScopeNotFound { .. }));
        let close_err = matches!(tree.close(&ghost, ts), Err(ScopeTreeError::ScopeNotFound { .. }));
        prop_assert!(start_err);
        prop_assert!(shutdown_err);
        prop_assert!(finalize_err);
        prop_assert!(close_err);
    }

    /// ST-36: Finalize blocked by live children
    #[test]
    fn st36_finalize_blocked_by_live_children(
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

    /// ST-37: Finalize succeeds after all children closed
    #[test]
    fn st37_finalize_after_children_closed(
        num_children in 1usize..4,
        ts in arb_timestamp(),
    ) {
        let mut tree = ScopeTree::new(ts);
        let parent = ScopeId("p".into());
        tree.register(parent.clone(), ScopeTier::Daemon, &ScopeId::root(), "p", ts).unwrap();
        tree.start(&parent, ts + 1).unwrap();

        for i in 0..num_children {
            let cid = ScopeId(format!("c{i}"));
            tree.register(cid.clone(), ScopeTier::Worker, &parent, format!("c{i}"), ts + 2).unwrap();
            tree.start(&cid, ts + 3).unwrap();
        }

        // Shutdown parent, then close all children
        tree.request_shutdown(&parent, ts + 100).unwrap();
        for i in 0..num_children {
            let cid = ScopeId(format!("c{i}"));
            tree.request_shutdown(&cid, ts + 101).unwrap();
            tree.finalize(&cid).unwrap();
            tree.close(&cid, ts + 102).unwrap();
        }

        // Now parent can finalize
        tree.finalize(&parent).unwrap();
        tree.close(&parent, ts + 200).unwrap();
        let is_closed = tree.get(&parent).unwrap().state.is_terminal();
        prop_assert!(is_closed);
    }
}

// ── Shutdown Ordering ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ST-38: Shutdown order children before parents
    #[test]
    fn st38_shutdown_order_children_before_parents(
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

        let daemon_pos = order.iter().position(|id| *id == daemon_id);
        for i in 0..num_workers {
            let wid = ScopeId(format!("worker_{i}"));
            let worker_pos = order.iter().position(|id| *id == wid);
            if let (Some(wp), Some(dp)) = (worker_pos, daemon_pos) {
                prop_assert!(wp < dp, "worker {} at pos {} must be before daemon at pos {}", i, wp, dp);
            }
        }
    }

    /// ST-39: Root is always last in shutdown order
    #[test]
    fn st39_root_last_in_shutdown(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        tree.start(&ScopeId::root(), ts + 1).unwrap();
        for id in tree.root().children.clone() {
            tree.start(&id, ts + 2).unwrap();
        }

        let order = tree.shutdown_order();
        prop_assert!(!order.is_empty());
        let last = order.last().unwrap();
        prop_assert!(last.is_root(), "root should be last, got {last}");
    }

    /// ST-40: Closed scopes excluded from shutdown order
    #[test]
    fn st40_closed_excluded_from_shutdown(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("d".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "d", ts).unwrap();
        tree.start(&ScopeId::root(), ts).unwrap();
        tree.start(&id, ts + 1).unwrap();
        tree.request_shutdown(&id, ts + 2).unwrap();
        tree.finalize(&id).unwrap();
        tree.close(&id, ts + 3).unwrap();

        let order = tree.shutdown_order();
        let contains_closed = order.iter().any(|sid| *sid == id);
        prop_assert!(!contains_closed, "closed scope should not be in shutdown order");
    }
}

// ── Depth & Descendants ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ST-41: Depth monotonically increases parent→child
    #[test]
    fn st41_depth_parent_less_than_child(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "w0", ts,
        ).unwrap();

        prop_assert_eq!(tree.depth(&ScopeId::root()), 0);
        prop_assert_eq!(tree.depth(&well_known::capture()), 1);
        prop_assert_eq!(tree.depth(&well_known::capture_worker(0)), 2);
    }

    /// ST-42: Descendants of root = all non-root nodes
    #[test]
    fn st42_root_descendants_complete(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        let desc = tree.descendants(&ScopeId::root());
        prop_assert_eq!(desc.len(), tree.len() - 1);
    }

    /// ST-43: Descendants of leaf node is empty
    #[test]
    fn st43_leaf_no_descendants(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let id = ScopeId("leaf".into());
        tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), "leaf", ts).unwrap();
        let desc = tree.descendants(&id);
        prop_assert!(desc.is_empty());
    }

    /// ST-44: children_of returns direct children only
    #[test]
    fn st44_children_of_direct_only(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        let d = ScopeId("d".into());
        tree.register(d.clone(), ScopeTier::Daemon, &ScopeId::root(), "d", ts).unwrap();
        tree.register(ScopeId("w0".into()), ScopeTier::Worker, &d, "w0", ts).unwrap();
        tree.register(ScopeId("w1".into()), ScopeTier::Worker, &d, "w1", ts).unwrap();

        let children = tree.children_of(&d);
        prop_assert_eq!(children.len(), 2);
        // Root has 1 child (d)
        let root_children = tree.children_of(&ScopeId::root());
        prop_assert_eq!(root_children.len(), 1);
    }
}

// ── Counting & Querying ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ST-45: Snapshot matches tree state
    #[test]
    fn st45_snapshot_matches_tree(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        let snap = tree.snapshot();
        prop_assert_eq!(snap.total_scopes, tree.len());
        prop_assert_eq!(snap.daemons, tree.count_by_tier(ScopeTier::Daemon));
        prop_assert_eq!(snap.watchers, tree.count_by_tier(ScopeTier::Watcher));
        prop_assert_eq!(snap.workers, tree.count_by_tier(ScopeTier::Worker));
        prop_assert_eq!(snap.ephemeral, tree.count_by_tier(ScopeTier::Ephemeral));
        prop_assert_eq!(snap.running, tree.count_by_state(ScopeState::Running));
        prop_assert_eq!(snap.draining, tree.count_by_state(ScopeState::Draining));
        prop_assert_eq!(snap.closed, tree.count_by_state(ScopeState::Closed));
    }

    /// ST-46: scopes_for_tier returns correct count
    #[test]
    fn st46_scopes_for_tier(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        let daemons = tree.scopes_for_tier(ScopeTier::Daemon);
        prop_assert_eq!(daemons.len(), 5);
        let watchers = tree.scopes_for_tier(ScopeTier::Watcher);
        prop_assert_eq!(watchers.len(), 3);
        let workers = tree.scopes_for_tier(ScopeTier::Worker);
        prop_assert!(workers.is_empty());
    }

    /// ST-47: count_by_state sums correctly after lifecycle changes
    #[test]
    fn st47_count_by_state_after_changes(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();

        // All 9 nodes start as Created
        prop_assert_eq!(tree.count_by_state(ScopeState::Created), 9);
        prop_assert_eq!(tree.count_by_state(ScopeState::Running), 0);

        // Start root
        tree.start(&ScopeId::root(), ts + 1).unwrap();
        prop_assert_eq!(tree.count_by_state(ScopeState::Created), 8);
        prop_assert_eq!(tree.count_by_state(ScopeState::Running), 1);
    }
}

// ── ScopeHandle Properties ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ST-48: ScopeHandle starts not shutdown with generation 0
    #[test]
    fn st48_handle_initial_state(id in arb_scope_id()) {
        let handle = ScopeHandle::new(id.clone());
        prop_assert!(!handle.is_shutdown_requested());
        prop_assert_eq!(handle.current_generation(), 0);
        prop_assert_eq!(handle.scope_id, id);
    }

    /// ST-49: request_shutdown sets flag and increments generation
    #[test]
    fn st49_handle_shutdown_flag(id in arb_scope_id()) {
        let handle = ScopeHandle::new(id);
        handle.request_shutdown();
        prop_assert!(handle.is_shutdown_requested());
        prop_assert_eq!(handle.current_generation(), 1);
    }

    /// ST-50: Multiple request_shutdown calls increment generation
    #[test]
    fn st50_handle_multiple_shutdowns(n in 1u64..10) {
        let handle = ScopeHandle::new(ScopeId("test".into()));
        for _ in 0..n {
            handle.request_shutdown();
        }
        prop_assert!(handle.is_shutdown_requested());
        prop_assert_eq!(handle.current_generation(), n);
    }

    /// ST-51: Cloned handle shares shutdown flag (Arc)
    #[test]
    fn st51_handle_clone_shares_flag(id in arb_scope_id()) {
        let h1 = ScopeHandle::new(id);
        let h2 = h1.clone();
        prop_assert!(!h2.is_shutdown_requested());
        h1.request_shutdown();
        prop_assert!(h2.is_shutdown_requested());
        prop_assert_eq!(h2.current_generation(), 1);
    }
}

// ── Serde Roundtrips ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ST-52: ScopeTree serde roundtrip
    #[test]
    fn st52_tree_serde_roundtrip(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        tree.start(&ScopeId::root(), ts + 100).unwrap();

        let json = serde_json::to_string(&tree).unwrap();
        let restored: ScopeTree = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(tree.len(), restored.len());
        prop_assert_eq!(tree.canonical_string(), restored.canonical_string());
    }

    /// ST-53: ScopeTreeSnapshot serde roundtrip
    #[test]
    fn st53_snapshot_serde_roundtrip(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();

        let snap = tree.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let restored: ScopeTreeSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, restored);
    }

    /// ST-54: ScopeTreeSnapshot serde roundtrip with arbitrary values
    #[test]
    fn st54_snapshot_arbitrary_serde(
        total in 0usize..100,
        running in 0usize..50,
        draining in 0usize..50,
        closed in 0usize..50,
        daemons in 0usize..20,
        watchers in 0usize..20,
        workers in 0usize..20,
        ephemeral in 0usize..20,
        max_depth in 0usize..10,
    ) {
        let snap = ScopeTreeSnapshot {
            total_scopes: total,
            running,
            draining,
            closed,
            daemons,
            watchers,
            workers,
            ephemeral,
            max_depth,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ScopeTreeSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }
}

// ── Canonical String & Display ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ST-55: Tree canonical_string is deterministic
    #[test]
    fn st55_tree_canonical_deterministic(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        let s1 = tree.canonical_string();
        let s2 = tree.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    /// ST-56: ScopeTreeError Display is non-empty for all variants
    #[test]
    fn st56_error_display_non_empty(name in "[a-z]{3,10}") {
        let id = ScopeId(name);
        let errors: Vec<ScopeTreeError> = vec![
            ScopeTreeError::DuplicateScope { scope_id: id.clone() },
            ScopeTreeError::ParentNotFound { parent_id: id.clone() },
            ScopeTreeError::ParentNotAccepting { parent_id: id.clone(), state: ScopeState::Draining },
            ScopeTreeError::ScopeNotFound { scope_id: id.clone() },
            ScopeTreeError::InvalidTransition { scope_id: id.clone(), from: ScopeState::Created, to: ScopeState::Closed },
            ScopeTreeError::HasLiveChildren { scope_id: id.clone(), live_count: 3 },
            ScopeTreeError::TierCannotHaveChildren { scope_id: id, tier: ScopeTier::Worker },
        ];
        for err in &errors {
            let display = format!("{err}");
            prop_assert!(!display.is_empty());
        }
    }
}

// ── well_known Scope IDs ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// ST-57: well_known IDs are unique
    #[test]
    fn st57_well_known_ids_unique(_dummy in 0u8..1) {
        let ids = vec![
            well_known::root(),
            well_known::discovery(),
            well_known::capture(),
            well_known::relay(),
            well_known::persistence(),
            well_known::maintenance(),
            well_known::native_events(),
            well_known::snapshot(),
            well_known::config_reload(),
        ];
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            let is_new = seen.insert(id.clone());
            prop_assert!(is_new, "duplicate well_known ID: {id}");
        }
    }

    /// ST-58: well_known::capture_worker IDs vary by index
    #[test]
    fn st58_capture_worker_ids_vary(a in 0usize..100, b in 0usize..100) {
        prop_assume!(a != b);
        let id_a = well_known::capture_worker(a);
        let id_b = well_known::capture_worker(b);
        prop_assert_ne!(id_a, id_b);
    }

    /// ST-59: well_known::ephemeral_query IDs contain query string
    #[test]
    fn st59_ephemeral_query_contains_name(name in "[a-z]{3,10}") {
        let id = well_known::ephemeral_query(&name);
        prop_assert!(id.0.contains(&name));
        prop_assert!(id.0.starts_with("ephemeral:query:"));
    }

    /// ST-60: register_standard_scopes registers exactly 8 children of root
    #[test]
    fn st60_standard_scopes_count(ts in arb_timestamp()) {
        let mut tree = ScopeTree::new(ts);
        register_standard_scopes(&mut tree, ts).unwrap();
        // 5 daemons + 3 watchers = 8 children + 1 root = 9 total
        prop_assert_eq!(tree.len(), 9);
        prop_assert_eq!(tree.root().children.len(), 8);
    }
}

// ── Non-proptest supplemental ───────────────────────────────────────────────

#[test]
fn tier_shutdown_priority_ordering() {
    // Ephemeral > Worker > Watcher > Daemon > Root
    assert!(ScopeTier::Ephemeral.shutdown_priority() > ScopeTier::Worker.shutdown_priority());
    assert!(ScopeTier::Worker.shutdown_priority() > ScopeTier::Watcher.shutdown_priority());
    assert!(ScopeTier::Watcher.shutdown_priority() > ScopeTier::Daemon.shutdown_priority());
    assert!(ScopeTier::Daemon.shutdown_priority() > ScopeTier::Root.shutdown_priority());
}
