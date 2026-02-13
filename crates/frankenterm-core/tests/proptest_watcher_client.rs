//! Property-based tests for watcher client registry invariants.
//!
//! Bead: wa-86ff
//!
//! Validates:
//! 1. ClientRole can_mutate: Interactive=true, Watcher=false
//! 2. Connect increments count correctly
//! 3. Disconnect decrements count correctly
//! 4. Capacity limits enforced: total and watcher
//! 5. Watcher denied all mutating actions
//! 6. Interactive allowed all actions
//! 7. Unknown client denied
//! 8. First interactive becomes leader
//! 9. Leader disconnect promotes next interactive
//! 10. Watcher cannot become leader
//! 11. Mirrored clients follow leader focus
//! 12. Independent clients maintain own focus
//! 13. Summary count matches total_count
//! 14. Connect/disconnect roundtrip returns to zero

use proptest::prelude::*;

use frankenterm_core::policy::ActionKind;
use frankenterm_core::watcher_client::{
    ClientId, ClientPolicyDecision, ClientRegistry, ClientRegistryConfig, ClientRole, ViewMode,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_mutating_action() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Close),
        Just(ActionKind::SendCtrlC),
    ]
}

fn arb_read_action() -> impl Strategy<Value = ActionKind> {
    prop_oneof![Just(ActionKind::ReadOutput), Just(ActionKind::SearchOutput),]
}

fn arb_tab() -> impl Strategy<Value = u64> {
    0_u64..20
}

fn arb_pane() -> impl Strategy<Value = u64> {
    0_u64..100
}

// =============================================================================
// Property: ClientRole can_mutate consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn role_can_mutate_consistent(
        _dummy in 0..1_u32,
    ) {
        prop_assert!(ClientRole::Interactive.can_mutate());
        prop_assert!(!ClientRole::Watcher.can_mutate());
        prop_assert_eq!(ClientRole::Interactive.as_str(), "interactive");
        prop_assert_eq!(ClientRole::Watcher.as_str(), "watcher");
    }
}

// =============================================================================
// Property: Connect increments count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn connect_increments_count(
        n_interactive in 0_usize..10,
        n_watcher in 0_usize..10,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 100,
            max_watchers: 50,
        };
        let mut reg = ClientRegistry::new(config);

        for i in 0..n_interactive {
            reg.connect(&format!("i{}", i), ClientRole::Interactive);
        }
        for i in 0..n_watcher {
            reg.connect(&format!("w{}", i), ClientRole::Watcher);
        }

        prop_assert_eq!(reg.interactive_count(), n_interactive);
        prop_assert_eq!(reg.watcher_count(), n_watcher);
        prop_assert_eq!(reg.total_count(), n_interactive + n_watcher);
    }
}

// =============================================================================
// Property: Disconnect decrements count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn disconnect_decrements_count(
        n in 2_usize..15,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 100,
            max_watchers: 50,
        };
        let mut reg = ClientRegistry::new(config);
        let mut ids = Vec::new();

        for i in 0..n {
            if let Some(id) = reg.connect(&format!("c{}", i), ClientRole::Interactive) {
                ids.push(id);
            }
        }
        prop_assert_eq!(reg.total_count(), n);

        // Remove half.
        let half = n / 2;
        for id in &ids[..half] {
            reg.disconnect(id);
        }
        prop_assert_eq!(reg.total_count(), n - half);
    }
}

// =============================================================================
// Property: Total capacity limit enforced
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn total_capacity_enforced(
        max_clients in 1_usize..10,
    ) {
        let config = ClientRegistryConfig {
            max_clients,
            max_watchers: max_clients,
        };
        let mut reg = ClientRegistry::new(config);

        for i in 0..max_clients + 5 {
            reg.connect(&format!("c{}", i), ClientRole::Interactive);
        }

        prop_assert_eq!(reg.total_count(), max_clients,
            "total should be capped at max_clients {}", max_clients);
    }
}

// =============================================================================
// Property: Watcher capacity limit enforced
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn watcher_capacity_enforced(
        max_watchers in 1_usize..10,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 100,
            max_watchers,
        };
        let mut reg = ClientRegistry::new(config);

        for i in 0..max_watchers + 5 {
            reg.connect(&format!("w{}", i), ClientRole::Watcher);
        }

        prop_assert_eq!(reg.watcher_count(), max_watchers,
            "watcher count should be capped at max_watchers {}", max_watchers);
        // Interactive slots still available.
        let iid = reg.connect("interactive", ClientRole::Interactive);
        prop_assert!(iid.is_some(), "interactive should still be connectable");
    }
}

// =============================================================================
// Property: Watcher denied all mutating actions
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn watcher_denied_mutations(
        action in arb_mutating_action(),
    ) {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        let cid = reg.connect("watcher", ClientRole::Watcher).unwrap();

        let decision = reg.authorize(&cid, action);
        prop_assert!(decision.is_denied(),
            "watcher should be denied mutating action {:?}", action);
        prop_assert!(decision.denial_reason().is_some());
    }
}

// =============================================================================
// Property: Watcher allowed read actions
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn watcher_allowed_reads(
        action in arb_read_action(),
    ) {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        let cid = reg.connect("watcher", ClientRole::Watcher).unwrap();

        let decision = reg.authorize(&cid, action);
        prop_assert!(decision.is_allowed(),
            "watcher should be allowed read action {:?}", action);
    }
}

// =============================================================================
// Property: Interactive allowed all actions
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn interactive_allowed_all(
        action in prop_oneof![arb_mutating_action(), arb_read_action()],
    ) {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        let cid = reg.connect("user", ClientRole::Interactive).unwrap();

        let decision = reg.authorize(&cid, action);
        prop_assert!(decision.is_allowed(),
            "interactive should be allowed action {:?}", action);
    }
}

// =============================================================================
// Property: Unknown client denied
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn unknown_client_denied(
        action in prop_oneof![arb_mutating_action(), arb_read_action()],
    ) {
        let reg = ClientRegistry::new(ClientRegistryConfig::default());
        let fake = ClientId("cl-fake-0000".to_string());

        let decision = reg.authorize(&fake, action);
        prop_assert!(decision.is_denied());
        prop_assert!(decision.denial_reason().unwrap().contains("unknown"));
    }
}

// =============================================================================
// Property: First interactive becomes leader
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn first_interactive_becomes_leader(
        n_watchers in 0_usize..5,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 20,
            max_watchers: 10,
        };
        let mut reg = ClientRegistry::new(config);

        // Add watchers first — no leader yet.
        for i in 0..n_watchers {
            reg.connect(&format!("w{}", i), ClientRole::Watcher);
        }
        prop_assert!(reg.leader().is_none());

        // First interactive becomes leader.
        let leader_id = reg.connect("leader", ClientRole::Interactive).unwrap();
        prop_assert_eq!(reg.leader(), Some(&leader_id));
    }
}

// =============================================================================
// Property: Leader disconnect promotes next interactive
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn leader_disconnect_promotes(
        n_interactive in 2_usize..8,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 20,
            max_watchers: 10,
        };
        let mut reg = ClientRegistry::new(config);
        let mut ids = Vec::new();

        for i in 0..n_interactive {
            if let Some(id) = reg.connect(&format!("i{}", i), ClientRole::Interactive) {
                ids.push(id);
            }
        }

        let first_leader = ids[0].clone();
        prop_assert_eq!(reg.leader(), Some(&first_leader));

        // Disconnect the leader.
        reg.disconnect(&first_leader);
        // New leader should be some other interactive client.
        let new_leader = reg.leader();
        prop_assert!(new_leader.is_some(), "should have a new leader");
        prop_assert_ne!(new_leader.unwrap(), &first_leader,
            "new leader should not be the old one");
    }
}

// =============================================================================
// Property: Watcher cannot become leader
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn watcher_cannot_be_leader(
        _dummy in 0..1_u32,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        let wid = reg.connect("watcher", ClientRole::Watcher).unwrap();

        prop_assert!(!reg.set_leader(&wid));
    }
}

// =============================================================================
// Property: Mirrored clients follow leader focus
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mirrored_follows_leader(
        tab in arb_tab(),
        pane in arb_pane(),
    ) {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        let leader = reg.connect("leader", ClientRole::Interactive).unwrap();
        let watcher = reg.connect("watcher", ClientRole::Watcher).unwrap();

        reg.set_view_mode(&watcher, ViewMode::Mirrored);
        reg.set_focus(&leader, tab, pane);

        let (eff_tab, eff_pane) = reg.effective_focus(&watcher).unwrap();
        prop_assert_eq!(eff_tab, tab, "mirrored tab should follow leader");
        prop_assert_eq!(eff_pane, pane, "mirrored pane should follow leader");
    }
}

// =============================================================================
// Property: Independent clients maintain own focus
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn independent_keeps_own_focus(
        leader_tab in arb_tab(),
        leader_pane in arb_pane(),
        agent_tab in arb_tab(),
        agent_pane in arb_pane(),
    ) {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        let leader = reg.connect("leader", ClientRole::Interactive).unwrap();
        let agent = reg.connect("agent", ClientRole::Interactive).unwrap();

        reg.set_focus(&leader, leader_tab, leader_pane);
        reg.set_focus(&agent, agent_tab, agent_pane);

        let (lt, lp) = reg.effective_focus(&leader).unwrap();
        let (at, ap) = reg.effective_focus(&agent).unwrap();

        prop_assert_eq!(lt, leader_tab);
        prop_assert_eq!(lp, leader_pane);
        prop_assert_eq!(at, agent_tab);
        prop_assert_eq!(ap, agent_pane);
    }
}

// =============================================================================
// Property: Summary count matches total_count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn summary_count_matches(
        n_interactive in 0_usize..5,
        n_watcher in 0_usize..5,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 20,
            max_watchers: 10,
        };
        let mut reg = ClientRegistry::new(config);

        for i in 0..n_interactive {
            reg.connect(&format!("i{}", i), ClientRole::Interactive);
        }
        for i in 0..n_watcher {
            reg.connect(&format!("w{}", i), ClientRole::Watcher);
        }

        let summary = reg.summary();
        prop_assert_eq!(summary.len(), reg.total_count());
        prop_assert_eq!(summary.len(), n_interactive + n_watcher);
    }
}

// =============================================================================
// Property: Connect/disconnect roundtrip returns to zero
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn connect_disconnect_roundtrip(
        n in 1_usize..15,
    ) {
        let config = ClientRegistryConfig {
            max_clients: 100,
            max_watchers: 50,
        };
        let mut reg = ClientRegistry::new(config);
        let mut ids = Vec::new();

        for i in 0..n {
            let role = if i % 2 == 0 { ClientRole::Interactive } else { ClientRole::Watcher };
            if let Some(id) = reg.connect(&format!("c{}", i), role) {
                ids.push(id);
            }
        }

        for id in &ids {
            reg.disconnect(id);
        }

        prop_assert_eq!(reg.total_count(), 0);
        prop_assert_eq!(reg.interactive_count(), 0);
        prop_assert_eq!(reg.watcher_count(), 0);
    }
}

// =============================================================================
// Property: ClientPolicyDecision is_allowed / is_denied consistent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn policy_decision_consistency(
        _dummy in 0..1_u32,
    ) {
        let allow = ClientPolicyDecision::Allow;
        prop_assert!(allow.is_allowed());
        prop_assert!(!allow.is_denied());
        prop_assert!(allow.denial_reason().is_none());

        let denied_watcher = ClientPolicyDecision::DeniedWatcher {
            action: ActionKind::SendText,
            client_id: ClientId("cl-test".to_string()),
        };
        prop_assert!(!denied_watcher.is_allowed());
        prop_assert!(denied_watcher.is_denied());
        prop_assert!(denied_watcher.denial_reason().is_some());

        let denied_unknown = ClientPolicyDecision::DeniedUnknown {
            client_id: ClientId("cl-unknown".to_string()),
        };
        prop_assert!(!denied_unknown.is_allowed());
        prop_assert!(denied_unknown.is_denied());
        prop_assert!(denied_unknown.denial_reason().is_some());
    }
}

// =============================================================================
// Property: Mirrored client cannot set own focus
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mirrored_cannot_set_focus(
        tab in arb_tab(),
        pane in arb_pane(),
    ) {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        let _leader = reg.connect("leader", ClientRole::Interactive).unwrap();
        let watcher = reg.connect("watcher", ClientRole::Watcher).unwrap();

        reg.set_view_mode(&watcher, ViewMode::Mirrored);
        let result = reg.set_focus(&watcher, tab, pane);
        prop_assert!(!result, "mirrored client should not be able to set focus");
    }
}
