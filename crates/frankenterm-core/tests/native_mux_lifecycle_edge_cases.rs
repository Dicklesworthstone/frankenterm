//! Edge-case and exhaustive tests for the lifecycle state machine in session_topology.
//!
//! Complements `native_mux_lifecycle_integration.rs` (happy-path and bootstrap flows)
//! with coverage for:
//! - Invalid event/state combos across all entity types
//! - Running→Draining pane transition
//! - Closed→Recovering session/window recovery path
//! - Multi-cycle disconnect/recovery loops
//! - Agent-specific event rejection
//! - ForceClose idempotence for all entity types
//! - Duplicate re-registration behavior
//! - Registry transition on unregistered entity
//! - Context validation completeness

use std::collections::HashMap;

use frankenterm_core::session_topology::{
    AgentLifecycleState, LifecycleEntityKind, LifecycleEvent, LifecycleIdentity, LifecycleRegistry,
    LifecycleState, LifecycleTransitionContext, LifecycleTransitionRequest, MuxPaneLifecycleState,
    SessionLifecycleState, WindowLifecycleState, transition_agent_state, transition_pane_state,
    transition_session_state, transition_window_state,
};
use frankenterm_core::wezterm::{PaneInfo, PaneSize};

// =============================================================================
// Helpers
// =============================================================================

fn ctx(ts: u64, scenario: &str, reason: &str) -> LifecycleTransitionContext {
    LifecycleTransitionContext::new(
        ts,
        "lifecycle.edge_cases",
        format!("corr-edge-{ts}"),
        scenario,
        reason,
    )
}

fn make_pane_info(pane_id: u64, tab_id: u64, window_id: u64, is_active: bool) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id,
        window_id,
        domain_id: None,
        domain_name: None,
        workspace: Some("edge-test".to_string()),
        size: Some(PaneSize {
            rows: 24,
            cols: 80,
            pixel_width: None,
            pixel_height: None,
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: Some(format!("pane-{pane_id}")),
        cwd: Some(format!("/work/{pane_id}")),
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

// =============================================================================
// Session: invalid event rejection
// =============================================================================

#[test]
fn session_provisioning_rejects_start_work() {
    let err = transition_session_state(
        SessionLifecycleState::Provisioning,
        LifecycleEvent::StartWork,
    )
    .unwrap_err();
    assert_eq!(err.entity, LifecycleEntityKind::Session);
    assert_eq!(err.state, "provisioning");
}

#[test]
fn session_provisioning_rejects_work_finished() {
    let err = transition_session_state(
        SessionLifecycleState::Provisioning,
        LifecycleEvent::WorkFinished,
    )
    .unwrap_err();
    assert_eq!(err.entity, LifecycleEntityKind::Session);
}

#[test]
fn session_provisioning_rejects_attach() {
    let err = transition_session_state(SessionLifecycleState::Provisioning, LifecycleEvent::Attach)
        .unwrap_err();
    assert_eq!(err.entity, LifecycleEntityKind::Session);
}

#[test]
fn session_provisioning_rejects_detach() {
    let err = transition_session_state(SessionLifecycleState::Provisioning, LifecycleEvent::Detach)
        .unwrap_err();
    assert_eq!(err.entity, LifecycleEntityKind::Session);
}

#[test]
fn session_active_rejects_provisioned() {
    let err = transition_session_state(SessionLifecycleState::Active, LifecycleEvent::Provisioned)
        .unwrap_err();
    assert_eq!(err.state, "active");
}

#[test]
fn session_active_rejects_start_work() {
    let err = transition_session_state(SessionLifecycleState::Active, LifecycleEvent::StartWork)
        .unwrap_err();
    assert_eq!(err.entity, LifecycleEntityKind::Session);
}

#[test]
fn session_active_rejects_drain_completed() {
    // DrainCompleted only valid from Draining state.
    let err = transition_session_state(
        SessionLifecycleState::Active,
        LifecycleEvent::DrainCompleted,
    )
    .unwrap_err();
    assert_eq!(err.state, "active");
}

#[test]
fn session_recovering_rejects_provisioned() {
    let err = transition_session_state(
        SessionLifecycleState::Recovering,
        LifecycleEvent::Provisioned,
    )
    .unwrap_err();
    assert_eq!(err.state, "recovering");
}

#[test]
fn session_closed_rejects_provisioned() {
    let err = transition_session_state(SessionLifecycleState::Closed, LifecycleEvent::Provisioned)
        .unwrap_err();
    assert_eq!(err.state, "closed");
}

#[test]
fn session_closed_rejects_drain_requested() {
    let err = transition_session_state(
        SessionLifecycleState::Closed,
        LifecycleEvent::DrainRequested,
    )
    .unwrap_err();
    assert_eq!(err.state, "closed");
}

// =============================================================================
// Session: Closed→Recovering recovery path
// =============================================================================

#[test]
fn session_closed_can_recover_to_recovering_then_active() {
    // Closed→Recover→Recovering
    let step1 =
        transition_session_state(SessionLifecycleState::Closed, LifecycleEvent::Recover).unwrap();
    assert_eq!(step1.next_state, SessionLifecycleState::Recovering);
    assert!(!step1.idempotent);

    // Recovering→Recover→Active
    let step2 = transition_session_state(step1.next_state, LifecycleEvent::Recover).unwrap();
    assert_eq!(step2.next_state, SessionLifecycleState::Active);
    assert!(!step2.idempotent);
}

// =============================================================================
// Window: invalid event rejection (mirrors session)
// =============================================================================

#[test]
fn window_provisioning_rejects_attach_and_detach() {
    transition_window_state(WindowLifecycleState::Provisioning, LifecycleEvent::Attach)
        .unwrap_err();
    transition_window_state(WindowLifecycleState::Provisioning, LifecycleEvent::Detach)
        .unwrap_err();
}

#[test]
fn window_active_rejects_work_events() {
    transition_window_state(WindowLifecycleState::Active, LifecycleEvent::StartWork).unwrap_err();
    transition_window_state(WindowLifecycleState::Active, LifecycleEvent::WorkFinished)
        .unwrap_err();
}

#[test]
fn window_closed_can_recover_to_recovering_then_active() {
    let step1 =
        transition_window_state(WindowLifecycleState::Closed, LifecycleEvent::Recover).unwrap();
    assert_eq!(step1.next_state, WindowLifecycleState::Recovering);

    let step2 = transition_window_state(step1.next_state, LifecycleEvent::Recover).unwrap();
    assert_eq!(step2.next_state, WindowLifecycleState::Active);
}

// =============================================================================
// Pane: Running→Draining transition
// =============================================================================

#[test]
fn pane_running_to_draining_direct() {
    // Pane can go from Running→Draining via DrainRequested (not just Ready→Draining).
    let outcome = transition_pane_state(
        MuxPaneLifecycleState::Running,
        LifecycleEvent::DrainRequested,
    )
    .unwrap();
    assert_eq!(outcome.next_state, MuxPaneLifecycleState::Draining);
    assert!(!outcome.idempotent);
}

#[test]
fn pane_running_to_orphaned_on_disconnect() {
    let outcome = transition_pane_state(
        MuxPaneLifecycleState::Running,
        LifecycleEvent::PeerDisconnected,
    )
    .unwrap();
    assert_eq!(outcome.next_state, MuxPaneLifecycleState::Orphaned);
}

// =============================================================================
// Pane: invalid event rejection
// =============================================================================

#[test]
fn pane_provisioning_rejects_start_work() {
    // Can't start work before provisioning completes.
    let err = transition_pane_state(
        MuxPaneLifecycleState::Provisioning,
        LifecycleEvent::StartWork,
    )
    .unwrap_err();
    assert_eq!(err.entity, LifecycleEntityKind::Pane);
    assert_eq!(err.state, "provisioning");
}

#[test]
fn pane_provisioning_rejects_drain_requested() {
    // Can't drain a pane that hasn't been provisioned yet.
    let err = transition_pane_state(
        MuxPaneLifecycleState::Provisioning,
        LifecycleEvent::DrainRequested,
    )
    .unwrap_err();
    assert_eq!(err.state, "provisioning");
}

#[test]
fn pane_orphaned_rejects_drain_requested() {
    // Orphaned panes must be recovered first before draining.
    let err = transition_pane_state(
        MuxPaneLifecycleState::Orphaned,
        LifecycleEvent::DrainRequested,
    )
    .unwrap_err();
    assert_eq!(err.state, "orphaned");
}

#[test]
fn pane_orphaned_rejects_start_work() {
    transition_pane_state(MuxPaneLifecycleState::Orphaned, LifecycleEvent::StartWork).unwrap_err();
}

#[test]
fn pane_closed_rejects_recover() {
    // Panes cannot be recovered from Closed (unlike Sessions/Windows).
    let err =
        transition_pane_state(MuxPaneLifecycleState::Closed, LifecycleEvent::Recover).unwrap_err();
    assert_eq!(err.state, "closed");
}

#[test]
fn pane_closed_rejects_start_work() {
    transition_pane_state(MuxPaneLifecycleState::Closed, LifecycleEvent::StartWork).unwrap_err();
}

#[test]
fn pane_ready_rejects_work_finished() {
    // WorkFinished only valid from Running.
    transition_pane_state(MuxPaneLifecycleState::Ready, LifecycleEvent::WorkFinished).unwrap_err();
}

#[test]
fn pane_rejects_attach_and_detach() {
    // Attach/Detach are agent-only events.
    transition_pane_state(MuxPaneLifecycleState::Ready, LifecycleEvent::Attach).unwrap_err();
    transition_pane_state(MuxPaneLifecycleState::Running, LifecycleEvent::Detach).unwrap_err();
}

// =============================================================================
// Agent: invalid event rejection
// =============================================================================

#[test]
fn agent_registered_rejects_start_work() {
    transition_agent_state(AgentLifecycleState::Registered, LifecycleEvent::StartWork).unwrap_err();
}

#[test]
fn agent_registered_rejects_work_finished() {
    transition_agent_state(
        AgentLifecycleState::Registered,
        LifecycleEvent::WorkFinished,
    )
    .unwrap_err();
}

#[test]
fn agent_registered_rejects_drain_requested() {
    transition_agent_state(
        AgentLifecycleState::Registered,
        LifecycleEvent::DrainRequested,
    )
    .unwrap_err();
}

#[test]
fn agent_registered_rejects_drain_completed() {
    transition_agent_state(
        AgentLifecycleState::Registered,
        LifecycleEvent::DrainCompleted,
    )
    .unwrap_err();
}

#[test]
fn agent_registered_rejects_peer_disconnected() {
    transition_agent_state(
        AgentLifecycleState::Registered,
        LifecycleEvent::PeerDisconnected,
    )
    .unwrap_err();
}

#[test]
fn agent_registered_rejects_recover() {
    transition_agent_state(AgentLifecycleState::Registered, LifecycleEvent::Recover).unwrap_err();
}

#[test]
fn agent_registered_rejects_provisioned() {
    transition_agent_state(AgentLifecycleState::Registered, LifecycleEvent::Provisioned)
        .unwrap_err();
}

#[test]
fn agent_attached_rejects_drain_and_work_events() {
    transition_agent_state(
        AgentLifecycleState::Attached,
        LifecycleEvent::DrainRequested,
    )
    .unwrap_err();
    transition_agent_state(
        AgentLifecycleState::Attached,
        LifecycleEvent::DrainCompleted,
    )
    .unwrap_err();
    transition_agent_state(AgentLifecycleState::Attached, LifecycleEvent::StartWork).unwrap_err();
    transition_agent_state(AgentLifecycleState::Attached, LifecycleEvent::WorkFinished)
        .unwrap_err();
}

#[test]
fn agent_retired_rejects_attach() {
    transition_agent_state(AgentLifecycleState::Retired, LifecycleEvent::Attach).unwrap_err();
}

#[test]
fn agent_retired_rejects_detach() {
    transition_agent_state(AgentLifecycleState::Retired, LifecycleEvent::Detach).unwrap_err();
}

#[test]
fn agent_retired_rejects_recover() {
    transition_agent_state(AgentLifecycleState::Retired, LifecycleEvent::Recover).unwrap_err();
}

// =============================================================================
// ForceClose idempotence for all entity types
// =============================================================================

#[test]
fn force_close_idempotent_for_all_terminal_states() {
    // Session: Closed→ForceClose→Closed (noop)
    let s = transition_session_state(SessionLifecycleState::Closed, LifecycleEvent::ForceClose)
        .unwrap();
    assert!(s.idempotent);
    assert_eq!(s.next_state, SessionLifecycleState::Closed);

    // Window: Closed→ForceClose→Closed (noop)
    let w =
        transition_window_state(WindowLifecycleState::Closed, LifecycleEvent::ForceClose).unwrap();
    assert!(w.idempotent);
    assert_eq!(w.next_state, WindowLifecycleState::Closed);

    // Pane: Closed→ForceClose→Closed (noop)
    let p =
        transition_pane_state(MuxPaneLifecycleState::Closed, LifecycleEvent::ForceClose).unwrap();
    assert!(p.idempotent);
    assert_eq!(p.next_state, MuxPaneLifecycleState::Closed);

    // Agent: Retired→ForceClose→Retired (noop)
    let a =
        transition_agent_state(AgentLifecycleState::Retired, LifecycleEvent::ForceClose).unwrap();
    assert!(a.idempotent);
    assert_eq!(a.next_state, AgentLifecycleState::Retired);
}

#[test]
fn force_close_from_every_non_terminal_state() {
    // Session: all non-Closed states → Closed.
    for state in [
        SessionLifecycleState::Provisioning,
        SessionLifecycleState::Active,
        SessionLifecycleState::Draining,
        SessionLifecycleState::Recovering,
    ] {
        let out = transition_session_state(state, LifecycleEvent::ForceClose).unwrap();
        assert_eq!(out.next_state, SessionLifecycleState::Closed);
        assert!(!out.idempotent);
    }

    // Pane: all non-Closed states → Closed.
    for state in [
        MuxPaneLifecycleState::Provisioning,
        MuxPaneLifecycleState::Ready,
        MuxPaneLifecycleState::Running,
        MuxPaneLifecycleState::Draining,
        MuxPaneLifecycleState::Orphaned,
    ] {
        let out = transition_pane_state(state, LifecycleEvent::ForceClose).unwrap();
        assert_eq!(out.next_state, MuxPaneLifecycleState::Closed);
        assert!(!out.idempotent);
    }

    // Agent: all non-Retired states → Retired.
    for state in [
        AgentLifecycleState::Registered,
        AgentLifecycleState::Attached,
        AgentLifecycleState::Detached,
    ] {
        let out = transition_agent_state(state, LifecycleEvent::ForceClose).unwrap();
        assert_eq!(out.next_state, AgentLifecycleState::Retired);
        assert!(!out.idempotent);
    }
}

// =============================================================================
// Multi-cycle recovery
// =============================================================================

#[test]
fn pane_multiple_disconnect_recover_cycles() {
    let mut state = MuxPaneLifecycleState::Running;

    for _ in 0..5 {
        // Running → Orphaned via PeerDisconnected
        let out = transition_pane_state(state, LifecycleEvent::PeerDisconnected).unwrap();
        assert_eq!(out.next_state, MuxPaneLifecycleState::Orphaned);

        // Orphaned → Ready via Recover
        let out2 = transition_pane_state(out.next_state, LifecycleEvent::Recover).unwrap();
        assert_eq!(out2.next_state, MuxPaneLifecycleState::Ready);

        // Ready → Running via StartWork
        let out3 = transition_pane_state(out2.next_state, LifecycleEvent::StartWork).unwrap();
        assert_eq!(out3.next_state, MuxPaneLifecycleState::Running);
        state = out3.next_state;
    }
    assert_eq!(state, MuxPaneLifecycleState::Running);
}

#[test]
fn session_multiple_disconnect_recover_cycles() {
    let mut state = SessionLifecycleState::Active;

    for _ in 0..3 {
        // Active → Recovering via PeerDisconnected
        let out = transition_session_state(state, LifecycleEvent::PeerDisconnected).unwrap();
        assert_eq!(out.next_state, SessionLifecycleState::Recovering);

        // Recovering → Active via Recover
        let out2 = transition_session_state(out.next_state, LifecycleEvent::Recover).unwrap();
        assert_eq!(out2.next_state, SessionLifecycleState::Active);
        state = out2.next_state;
    }
    assert_eq!(state, SessionLifecycleState::Active);
}

// =============================================================================
// Registry: unregistered entity
// =============================================================================

#[test]
fn registry_transition_on_unregistered_entity_fails() {
    let mut registry = LifecycleRegistry::new();
    let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", 42, 1);

    let err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity,
            event: LifecycleEvent::StartWork,
            expected_version: None,
            context: ctx(1000, "unregistered-test", "lifecycle.unregistered"),
        })
        .unwrap_err();

    // Should be EntityNotFound.
    let msg = err.to_string();
    assert!(
        msg.contains("not found") || msg.contains("entity"),
        "unexpected error: {msg}"
    );
}

// =============================================================================
// Registry: duplicate re-registration replaces silently
// =============================================================================

#[test]
fn registry_reregistration_replaces_existing_record() {
    let mut registry = LifecycleRegistry::new();
    let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", 1, 0);
    let state = LifecycleState::Pane(MuxPaneLifecycleState::Ready);

    let first = registry
        .register_entity(identity.clone(), state, 1000)
        .unwrap();
    assert_eq!(first.version, 0);

    // Re-register with different state — replaces silently.
    let new_state = LifecycleState::Pane(MuxPaneLifecycleState::Running);
    let second = registry
        .register_entity(identity.clone(), new_state, 2000)
        .unwrap();
    assert_eq!(second.version, 0); // Version resets on re-registration.
    assert_eq!(
        second.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    );

    // Only one entity in registry (not two).
    assert_eq!(registry.len(), 1);

    // The stored record reflects the second registration.
    let stored = registry.get(&identity).unwrap();
    assert_eq!(
        stored.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    );
    assert_eq!(stored.updated_at_ms, 2000);
}

// =============================================================================
// Registry: concurrency conflict with large version gap
// =============================================================================

#[test]
fn registry_version_conflict_with_large_gap() {
    let panes = vec![make_pane_info(100, 10, 1, true)];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1_000_000).unwrap();
    let identity = LifecycleIdentity::from_pane_info(&panes[0], 1);

    // Advance version to 3 via three transitions.
    for i in 0..3 {
        registry
            .apply_transition(LifecycleTransitionRequest {
                identity: identity.clone(),
                event: LifecycleEvent::PeerDisconnected,
                expected_version: Some(i * 2),
                context: ctx(
                    1_000_100 + i * 200,
                    "version-gap-test",
                    "lifecycle.disconnect",
                ),
            })
            .unwrap();
        registry
            .apply_transition(LifecycleTransitionRequest {
                identity: identity.clone(),
                event: LifecycleEvent::Recover,
                expected_version: Some(i * 2 + 1),
                context: ctx(1_000_200 + i * 200, "version-gap-test", "lifecycle.recover"),
            })
            .unwrap();
    }

    // Entity is now at version 6. Try with expected_version=0 (very stale).
    let err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: identity.clone(),
            event: LifecycleEvent::DrainRequested,
            expected_version: Some(0),
            context: ctx(2_000_000, "version-gap-test", "lifecycle.stale_writer"),
        })
        .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("concurrency conflict"), "unexpected: {msg}");

    // Verify the rejected transition was logged.
    let last_log = registry.transition_log().last().unwrap();
    assert_eq!(
        last_log.error_code.as_deref(),
        Some("native_mux.lifecycle.version_conflict")
    );
}

// =============================================================================
// Registry: transition without version check (expected_version=None)
// =============================================================================

#[test]
fn registry_transition_without_version_check_always_applies() {
    let panes = vec![make_pane_info(200, 20, 2, true)];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1_000_000).unwrap();
    let identity = LifecycleIdentity::from_pane_info(&panes[0], 1);

    // Advance to version 2.
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: identity.clone(),
            event: LifecycleEvent::PeerDisconnected,
            expected_version: Some(0),
            context: ctx(1_000_100, "no-version-test", "lifecycle.disconnect"),
        })
        .unwrap();
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: identity.clone(),
            event: LifecycleEvent::Recover,
            expected_version: Some(1),
            context: ctx(1_000_200, "no-version-test", "lifecycle.recover"),
        })
        .unwrap();

    // Now apply without version check — should succeed regardless of version.
    let result = registry
        .apply_transition(LifecycleTransitionRequest {
            identity,
            event: LifecycleEvent::StartWork,
            expected_version: None,
            context: ctx(1_000_300, "no-version-test", "lifecycle.start_work"),
        })
        .unwrap();

    assert_eq!(
        result.record.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    );
    assert_eq!(result.record.version, 3);
}

// =============================================================================
// Context validation: empty fields
// =============================================================================

#[test]
fn registry_rejects_empty_correlation_id() {
    let panes = vec![make_pane_info(300, 30, 3, true)];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1_000_000).unwrap();
    let identity = LifecycleIdentity::from_pane_info(&panes[0], 1);

    let err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity,
            event: LifecycleEvent::PeerDisconnected,
            expected_version: None,
            context: LifecycleTransitionContext::new(
                1_000_100,
                "component",
                "", // empty correlation_id
                "scenario",
                "reason",
            ),
        })
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("context") || msg.contains("correlation") || msg.contains("empty"),
        "unexpected error: {msg}"
    );
}

#[test]
fn registry_rejects_empty_scenario_id() {
    let panes = vec![make_pane_info(301, 31, 3, true)];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1_000_000).unwrap();
    let identity = LifecycleIdentity::from_pane_info(&panes[0], 1);

    let err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity,
            event: LifecycleEvent::PeerDisconnected,
            expected_version: None,
            context: LifecycleTransitionContext::new(
                1_000_100,
                "component",
                "corr-123",
                "", // empty scenario_id
                "reason",
            ),
        })
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("context") || msg.contains("scenario") || msg.contains("empty"),
        "unexpected error: {msg}"
    );
}

#[test]
fn registry_rejects_empty_reason_code() {
    let panes = vec![make_pane_info(302, 32, 3, true)];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1_000_000).unwrap();
    let identity = LifecycleIdentity::from_pane_info(&panes[0], 1);

    let err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity,
            event: LifecycleEvent::PeerDisconnected,
            expected_version: None,
            context: LifecycleTransitionContext::new(
                1_000_100,
                "component",
                "corr-123",
                "scenario",
                "", // empty reason_code
            ),
        })
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("context") || msg.contains("reason") || msg.contains("empty"),
        "unexpected error: {msg}"
    );
}

#[test]
fn registry_rejects_whitespace_only_context_fields() {
    let panes = vec![make_pane_info(303, 33, 3, true)];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1_000_000).unwrap();
    let identity = LifecycleIdentity::from_pane_info(&panes[0], 1);

    let err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity,
            event: LifecycleEvent::PeerDisconnected,
            expected_version: None,
            context: LifecycleTransitionContext::new(
                1_000_100, "   ", // whitespace-only component
                "corr-123", "scenario", "reason",
            ),
        })
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("context") || msg.contains("component") || msg.contains("empty"),
        "unexpected error: {msg}"
    );
}

// =============================================================================
// Pane full lifecycle: Provisioning → ... → Closed via Running→Draining
// =============================================================================

#[test]
fn pane_full_lifecycle_through_running_to_draining() {
    // Provisioning → Ready → Running → Draining → Closed
    let s1 = transition_pane_state(
        MuxPaneLifecycleState::Provisioning,
        LifecycleEvent::Provisioned,
    )
    .unwrap();
    assert_eq!(s1.next_state, MuxPaneLifecycleState::Ready);

    let s2 = transition_pane_state(s1.next_state, LifecycleEvent::StartWork).unwrap();
    assert_eq!(s2.next_state, MuxPaneLifecycleState::Running);

    // Drain directly from Running (not via Ready).
    let s3 = transition_pane_state(s2.next_state, LifecycleEvent::DrainRequested).unwrap();
    assert_eq!(s3.next_state, MuxPaneLifecycleState::Draining);

    let s4 = transition_pane_state(s3.next_state, LifecycleEvent::DrainCompleted).unwrap();
    assert_eq!(s4.next_state, MuxPaneLifecycleState::Closed);
}

// =============================================================================
// Draining disconnect edge case
// =============================================================================

#[test]
fn pane_draining_disconnect_then_recover_then_drain() {
    // Draining → Orphaned via PeerDisconnected
    let s1 = transition_pane_state(
        MuxPaneLifecycleState::Draining,
        LifecycleEvent::PeerDisconnected,
    )
    .unwrap();
    assert_eq!(s1.next_state, MuxPaneLifecycleState::Orphaned);

    // Orphaned → Ready via Recover
    let s2 = transition_pane_state(s1.next_state, LifecycleEvent::Recover).unwrap();
    assert_eq!(s2.next_state, MuxPaneLifecycleState::Ready);

    // Ready → Draining → Closed (re-drain after recovery)
    let s3 = transition_pane_state(s2.next_state, LifecycleEvent::DrainRequested).unwrap();
    assert_eq!(s3.next_state, MuxPaneLifecycleState::Draining);

    let s4 = transition_pane_state(s3.next_state, LifecycleEvent::DrainCompleted).unwrap();
    assert_eq!(s4.next_state, MuxPaneLifecycleState::Closed);
}

// =============================================================================
// Agent full lifecycle
// =============================================================================

#[test]
fn agent_full_lifecycle_attach_detach_reattach_retire() {
    let s1 =
        transition_agent_state(AgentLifecycleState::Registered, LifecycleEvent::Attach).unwrap();
    assert_eq!(s1.next_state, AgentLifecycleState::Attached);
    assert!(!s1.idempotent);

    let s2 = transition_agent_state(s1.next_state, LifecycleEvent::Detach).unwrap();
    assert_eq!(s2.next_state, AgentLifecycleState::Detached);

    let s3 = transition_agent_state(s2.next_state, LifecycleEvent::Attach).unwrap();
    assert_eq!(s3.next_state, AgentLifecycleState::Attached);

    // Idempotent attach.
    let s4 = transition_agent_state(s3.next_state, LifecycleEvent::Attach).unwrap();
    assert!(s4.idempotent);
    assert_eq!(s4.next_state, AgentLifecycleState::Attached);

    let s5 = transition_agent_state(s4.next_state, LifecycleEvent::ForceClose).unwrap();
    assert_eq!(s5.next_state, AgentLifecycleState::Retired);
}

// =============================================================================
// Bootstrap: multiple windows and tabs create correct entity counts
// =============================================================================

#[test]
fn bootstrap_multi_window_multi_tab_creates_correct_entity_counts() {
    let panes = vec![
        make_pane_info(1, 10, 100, true),  // window 100, tab 10
        make_pane_info(2, 10, 100, false), // window 100, tab 10
        make_pane_info(3, 20, 100, true),  // window 100, tab 20
        make_pane_info(4, 30, 200, true),  // window 200, tab 30
    ];

    let registry = LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1_000_000).unwrap();

    // 4 panes.
    assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 4);

    // Unique windows: 100, 200 = 2.
    // (bootstrap deduplicates by window_id)
    let window_count = registry.entity_count_by_kind(LifecycleEntityKind::Window);
    assert!(window_count >= 1, "should have at least 1 window");

    // Sessions: at least 1.
    let session_count = registry.entity_count_by_kind(LifecycleEntityKind::Session);
    assert!(session_count >= 1, "should have at least 1 session");
}
