//! Property-based tests for command_transport (ft-3681t.2.3).
//!
//! Coverage: CommandScope, CommandKind, CommandDeduplicator, serde roundtrips,
//! routing invariants, and deduplication TTL semantics.

use proptest::prelude::*;

use frankenterm_core::command_transport::{
    AckOutcome, CommandContext, CommandDeduplicator, CommandKind, CommandRequest, CommandResult,
    CommandRouter, CommandScope, DeliveryStatus, InterruptSignal,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState, SessionLifecycleState, WindowLifecycleState,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pane_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", id, 1)
}

fn window_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Window, "ws", "local", id, 1)
}

fn session_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Session, "ws", "local", id, 1)
}

fn test_ctx(ts: u64) -> CommandContext {
    CommandContext {
        timestamp_ms: ts,
        component: "proptest".to_string(),
        correlation_id: "corr-pt".to_string(),
        caller_identity: "agent-pt".to_string(),
        reason: None,
        policy_trace: None,
    }
}

fn seed_registry() -> LifecycleRegistry {
    let mut reg = LifecycleRegistry::new();
    reg.register_entity(
        session_id(1),
        LifecycleState::Session(SessionLifecycleState::Active),
        0,
    )
    .ok();
    reg.register_entity(
        window_id(10),
        LifecycleState::Window(WindowLifecycleState::Active),
        0,
    )
    .ok();
    for pid in [100, 101, 102] {
        reg.register_entity(
            pane_id(pid),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            0,
        )
        .ok();
    }
    reg
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_interrupt_signal() -> impl Strategy<Value = InterruptSignal> {
    prop_oneof![
        Just(InterruptSignal::CtrlC),
        Just(InterruptSignal::CtrlD),
        Just(InterruptSignal::CtrlZ),
        Just(InterruptSignal::CtrlBackslash),
    ]
}

fn arb_ack_outcome() -> impl Strategy<Value = AckOutcome> {
    prop_oneof![
        Just(AckOutcome::Delivered),
        Just(AckOutcome::Timeout),
        "[a-z]{3,20}".prop_map(|r| AckOutcome::Failed { reason: r }),
    ]
}

fn arb_command_kind() -> impl Strategy<Value = CommandKind> {
    prop_oneof![
        ("[a-z ]{1,20}", any::<bool>(), any::<bool>()).prop_map(|(text, paste, nl)| {
            CommandKind::SendInput {
                text,
                paste_mode: paste,
                append_newline: nl,
            }
        }),
        arb_interrupt_signal().prop_map(|sig| CommandKind::Interrupt { signal: sig }),
        (0u32..100, any::<bool>()).prop_map(|(lines, esc)| {
            CommandKind::Capture {
                tail_lines: lines,
                include_escapes: esc,
            }
        }),
        ("[a-z ]{1,20}", any::<bool>()).prop_map(|(text, paste)| {
            CommandKind::Broadcast {
                text,
                paste_mode: paste,
            }
        }),
        ("[a-z0-9-]{5,15}", arb_ack_outcome()).prop_map(|(id, outcome)| {
            CommandKind::Acknowledge {
                command_id: id,
                outcome,
            }
        }),
    ]
}

fn arb_delivery_status() -> impl Strategy<Value = DeliveryStatus> {
    prop_oneof![
        Just(DeliveryStatus::Delivered),
        "[a-z]{3,20}".prop_map(|r| DeliveryStatus::Skipped { reason: r }),
        "[a-z]{3,20}".prop_map(|r| DeliveryStatus::PolicyDenied { reason: r }),
        "[a-z]{3,20}".prop_map(|r| DeliveryStatus::RoutingError { reason: r }),
    ]
}

// ---------------------------------------------------------------------------
// Serde roundtrips
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn command_kind_serde_roundtrip(kind in arb_command_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let decoded: CommandKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, decoded);
    }

    #[test]
    fn interrupt_signal_serde_roundtrip(signal in arb_interrupt_signal()) {
        let json = serde_json::to_string(&signal).unwrap();
        let decoded: InterruptSignal = serde_json::from_str(&json).unwrap();
        assert_eq!(signal, decoded);
    }

    #[test]
    fn ack_outcome_serde_roundtrip(outcome in arb_ack_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let decoded: AckOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, decoded);
    }

    #[test]
    fn delivery_status_serde_roundtrip(status in arb_delivery_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let decoded: DeliveryStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, decoded);
    }

    /// CommandScope serde roundtrip for all variants.
    #[test]
    fn command_scope_serde_roundtrip(pid in 1u64..1000) {
        let scopes = vec![
            CommandScope::pane(pane_id(pid)),
            CommandScope::window(window_id(pid)),
            CommandScope::session(session_id(pid)),
            CommandScope::fleet(),
        ];
        for scope in scopes {
            let json = serde_json::to_string(&scope).unwrap();
            let decoded: CommandScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, decoded);
        }
    }

    /// CommandContext serde roundtrip.
    #[test]
    fn command_context_serde_roundtrip(ts in 0u64..u64::MAX / 2) {
        let ctx = test_ctx(ts);
        let json = serde_json::to_string(&ctx).unwrap();
        let decoded: CommandContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, decoded);
    }
}

// ---------------------------------------------------------------------------
// Deduplicator properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// First submission of any ID is never a duplicate.
    #[test]
    fn first_submission_is_not_duplicate(
        ttl in 100u64..10_000,
        ids in prop::collection::vec("[a-z0-9]{4,12}", 1..20),
    ) {
        let mut dedup = CommandDeduplicator::new(ttl);
        for (i, id) in ids.iter().enumerate() {
            let is_dup = dedup.is_duplicate(id, i as u64 * 10);
            // Only true if this ID appeared before (since IDs might repeat in generated vec)
            if ids[..i].contains(id) {
                // It's a repeat — should be duplicate
                assert!(is_dup, "repeated ID '{}' should be duplicate", id);
            } else {
                assert!(!is_dup, "first-seen ID '{}' should NOT be duplicate", id);
            }
        }
    }

    /// Second submission within TTL is a duplicate.
    #[test]
    fn resubmission_within_ttl_is_duplicate(ttl in 100u64..10_000) {
        let mut dedup = CommandDeduplicator::new(ttl);
        assert!(!dedup.is_duplicate("cmd-1", 0));
        assert!(dedup.is_duplicate("cmd-1", ttl / 2));
    }

    /// Submission after TTL expires is not a duplicate.
    #[test]
    fn submission_after_ttl_is_not_duplicate(ttl in 100u64..10_000) {
        let mut dedup = CommandDeduplicator::new(ttl);
        assert!(!dedup.is_duplicate("cmd-1", 0));
        assert!(!dedup.is_duplicate("cmd-1", ttl + 1));
    }

    /// Deduplicator evicts expired entries on each call.
    #[test]
    fn eviction_cleans_expired(
        ttl in 100u64..1000,
        n in 5u32..30,
    ) {
        let mut dedup = CommandDeduplicator::new(ttl);
        // Insert n commands at time 0
        for i in 0..n {
            dedup.is_duplicate(&format!("cmd-{i}"), 0);
        }
        assert_eq!(dedup.len(), n as usize);

        // After TTL, a new call should evict all previous
        dedup.is_duplicate("new-cmd", ttl + 1);
        // Only "new-cmd" should remain
        assert_eq!(dedup.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Router invariants
// ---------------------------------------------------------------------------

#[test]
fn route_pane_scope_to_running_pane_delivers() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-1".to_string(),
        scope: CommandScope::pane(pane_id(100)),
        command: CommandKind::SendInput {
            text: "hello".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&req, &reg).unwrap();
    assert_eq!(result.command_id, "cmd-1");
    assert_eq!(result.delivered_count(), 1);
    assert!(result.all_delivered());
}

#[test]
fn route_fleet_scope_delivers_to_all_running_panes() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-fleet".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::Broadcast {
            text: "alert".to_string(),
            paste_mode: false,
        },
        context: test_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&req, &reg).unwrap();
    // Should deliver to all 3 running panes
    assert_eq!(result.delivered_count(), 3);
    assert!(result.all_delivered());
}

#[test]
fn route_skips_non_running_panes() {
    let mut reg = seed_registry();
    // Mark pane 101 as closed
    reg.register_entity(
        pane_id(101),
        LifecycleState::Pane(MuxPaneLifecycleState::Closed),
        100,
    )
    .ok();

    let mut router = CommandRouter::new();
    let req = CommandRequest {
        command_id: "cmd-2".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::SendInput {
            text: "test".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&req, &reg).unwrap();
    assert_eq!(result.delivered_count(), 2); // 100, 102
    assert_eq!(result.skipped_count(), 1); // 101 (closed)
    assert!(!result.all_delivered());
}

#[test]
fn dry_run_does_not_change_delivered_count() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-dry".to_string(),
        scope: CommandScope::pane(pane_id(100)),
        command: CommandKind::SendInput {
            text: "test".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: true,
    };

    let result = router.route(&req, &reg).unwrap();
    assert!(result.dry_run);
    assert_eq!(result.delivered_count(), 1);
}

#[test]
fn audit_log_grows_with_routes() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    assert!(router.audit_log().is_empty());

    for i in 0..5 {
        let req = CommandRequest {
            command_id: format!("cmd-{i}"),
            scope: CommandScope::pane(pane_id(100)),
            command: CommandKind::SendInput {
                text: "x".to_string(),
                paste_mode: false,
                append_newline: true,
            },
            context: test_ctx(1000 + i),
            dry_run: false,
        };
        router.route(&req, &reg).unwrap();
    }

    assert_eq!(router.audit_log().len(), 5);
}

#[test]
fn audit_log_serializes_to_json() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-audit".to_string(),
        scope: CommandScope::pane(pane_id(100)),
        command: CommandKind::SendInput {
            text: "hello".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: false,
    };
    router.route(&req, &reg).unwrap();

    let json = router.audit_log_json().unwrap();
    assert!(json.contains("cmd-audit"));
    assert!(json.contains("proptest"));
}

// ---------------------------------------------------------------------------
// InterruptSignal coverage
// ---------------------------------------------------------------------------

#[test]
fn interrupt_signal_bytes_nonempty() {
    let signals = [
        InterruptSignal::CtrlC,
        InterruptSignal::CtrlD,
        InterruptSignal::CtrlZ,
        InterruptSignal::CtrlBackslash,
    ];
    for sig in signals {
        assert!(!sig.as_bytes().is_empty());
        assert!(!sig.label().is_empty());
    }
}

// ---------------------------------------------------------------------------
// CommandScope label coverage
// ---------------------------------------------------------------------------

#[test]
fn scope_labels_include_type_prefix() {
    assert!(CommandScope::pane(pane_id(1)).label().starts_with("pane:"));
    assert!(
        CommandScope::window(window_id(1))
            .label()
            .starts_with("window:")
    );
    assert!(
        CommandScope::session(session_id(1))
            .label()
            .starts_with("session:")
    );
    assert_eq!(CommandScope::fleet().label(), "fleet:*");
}

// ---------------------------------------------------------------------------
// DeliveryStatus predicates
// ---------------------------------------------------------------------------

#[test]
fn delivery_status_predicates() {
    assert!(DeliveryStatus::Delivered.is_delivered());
    assert!(!DeliveryStatus::Delivered.is_skipped());

    assert!(DeliveryStatus::Skipped { reason: "x".into() }.is_skipped());
    assert!(!DeliveryStatus::Skipped { reason: "x".into() }.is_delivered());

    assert!(!DeliveryStatus::PolicyDenied { reason: "x".into() }.is_delivered());
    assert!(!DeliveryStatus::RoutingError { reason: "x".into() }.is_delivered());
}

// ---------------------------------------------------------------------------
// CommandResult predicates
// ---------------------------------------------------------------------------

#[test]
fn command_result_all_delivered_requires_nonempty() {
    let empty = CommandResult {
        command_id: "x".into(),
        deliveries: vec![],
        dry_run: false,
        elapsed_us: 0,
    };
    assert!(
        !empty.all_delivered(),
        "empty deliveries → not all_delivered"
    );
}
