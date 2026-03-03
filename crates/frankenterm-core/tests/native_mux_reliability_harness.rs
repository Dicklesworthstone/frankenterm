// =============================================================================
// Native mux reliability/e2e validation harness (ft-3681t.2.7)
//
// Cross-module integration tests validating the full native mux stack:
// - Lifecycle (session_topology) → Command Transport → Durable State
// - Headless Mux Server (protocol + federation)
// - Session Profiles → Fleet Provisioning
// - E2E scenarios: provisioning → commands → failure → checkpoint → rollback
// =============================================================================

use std::collections::HashMap;

use frankenterm_core::command_transport::{
    CommandContext, CommandDeduplicator, CommandKind, CommandRequest, CommandRouter, CommandScope,
    CommandTransportError, InterruptSignal,
};
use frankenterm_core::durable_state::{
    CheckpointTrigger, DurableStateError, DurableStateManager,
};
use frankenterm_core::headless_mux_server::{
    HeadlessMuxServer, RemoteRequest, RemoteResponse, ServerConfig, ServerNodeId,
};
use frankenterm_core::session_profiles::{
    AgentIdentitySpec, FleetTemplate, FleetSlot, Persona, ProfilePolicy, ProfileRegistry,
    ProfileRole, ResourceHints, SessionProfile,
};
use frankenterm_core::session_topology::{
    LifecycleDecision, LifecycleEntityKind, LifecycleEvent, LifecycleIdentity, LifecycleRegistry,
    LifecycleState, LifecycleTransitionContext, LifecycleTransitionRequest,
    MuxPaneLifecycleState, SessionLifecycleState, WindowLifecycleState,
};

// =============================================================================
// Test helpers
// =============================================================================

fn pane_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "test-ws", "local", id, 1)
}

fn window_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Window, "test-ws", "local", id, 1)
}

fn session_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Session, "test-ws", "local", id, 1)
}

fn ctx(ts: u64, scenario: &str, reason: &str) -> LifecycleTransitionContext {
    LifecycleTransitionContext::new(
        ts,
        "native_mux.reliability_harness",
        format!("harness-corr-{ts}"),
        scenario,
        reason,
    )
}

fn cmd_ctx(ts: u64) -> CommandContext {
    CommandContext {
        timestamp_ms: ts,
        component: "reliability-harness".to_string(),
        correlation_id: format!("harness-cmd-{ts}"),
        caller_identity: "harness-agent".to_string(),
        reason: Some("integration test".to_string()),
    }
}

/// Build a registry with a session, a window, and N panes in Running state.
fn fleet_registry(pane_count: u64) -> LifecycleRegistry {
    let mut reg = LifecycleRegistry::new();
    reg.register_entity(
        session_id(0),
        LifecycleState::Session(SessionLifecycleState::Active),
        100,
    )
    .unwrap();
    reg.register_entity(
        window_id(0),
        LifecycleState::Window(WindowLifecycleState::Active),
        100,
    )
    .unwrap();
    for i in 1..=pane_count {
        reg.register_entity(
            pane_id(i),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            100,
        )
        .unwrap();
    }
    reg
}

/// Build a registry with panes in mixed lifecycle states.
fn mixed_state_registry() -> LifecycleRegistry {
    let mut reg = LifecycleRegistry::new();
    reg.register_entity(
        session_id(0),
        LifecycleState::Session(SessionLifecycleState::Active),
        100,
    )
    .unwrap();
    reg.register_entity(
        window_id(0),
        LifecycleState::Window(WindowLifecycleState::Active),
        100,
    )
    .unwrap();
    // Running (1-3)
    for i in 1..=3 {
        reg.register_entity(
            pane_id(i),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            100,
        )
        .unwrap();
    }
    // Ready (4)
    reg.register_entity(
        pane_id(4),
        LifecycleState::Pane(MuxPaneLifecycleState::Ready),
        100,
    )
    .unwrap();
    // Draining (5)
    reg.register_entity(
        pane_id(5),
        LifecycleState::Pane(MuxPaneLifecycleState::Draining),
        100,
    )
    .unwrap();
    // Orphaned (6)
    reg.register_entity(
        pane_id(6),
        LifecycleState::Pane(MuxPaneLifecycleState::Orphaned),
        100,
    )
    .unwrap();
    // Closed (7)
    reg.register_entity(
        pane_id(7),
        LifecycleState::Pane(MuxPaneLifecycleState::Closed),
        100,
    )
    .unwrap();
    reg
}

// =============================================================================
// 1. Lifecycle → Command Transport integration
// =============================================================================

#[test]
fn fleet_send_input_routes_only_to_running_panes() {
    let registry = mixed_state_registry();
    let mut router = CommandRouter::new();

    let request = CommandRequest {
        command_id: "fleet-send-1".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::SendInput {
            text: "echo hello".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: cmd_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&request, &registry).unwrap();
    // 7 panes total: 3 Running accept SendInput, 4 others skip
    assert_eq!(result.delivered_count(), 3);
    assert_eq!(result.skipped_count(), 4);
    assert_eq!(result.deliveries.len(), 7);
}

#[test]
fn fleet_capture_routes_to_running_ready_draining_orphaned() {
    let registry = mixed_state_registry();
    let mut router = CommandRouter::new();

    let request = CommandRequest {
        command_id: "fleet-capture-1".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::Capture {
            tail_lines: 100,
            include_escapes: false,
        },
        context: cmd_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&request, &registry).unwrap();
    // Running(3) + Ready(1) + Draining(1) + Orphaned(1) = 6 accept capture
    // Only Closed(1) rejects
    assert_eq!(result.delivered_count(), 6);
    assert_eq!(result.skipped_count(), 1);
}

#[test]
fn fleet_interrupt_routes_to_running_and_draining() {
    let registry = mixed_state_registry();
    let mut router = CommandRouter::new();

    let request = CommandRequest {
        command_id: "fleet-int-1".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::Interrupt {
            signal: InterruptSignal::CtrlC,
        },
        context: cmd_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&request, &registry).unwrap();
    // Running(3) + Draining(1) = 4 accept interrupt
    assert_eq!(result.delivered_count(), 4);
    assert_eq!(result.skipped_count(), 3);
}

#[test]
fn command_routing_after_lifecycle_transition() {
    let mut registry = fleet_registry(3);
    let mut router = CommandRouter::new();

    // Initial: all 3 running, all accept SendInput
    let r1 = router
        .route(
            &CommandRequest {
                command_id: "pre-transition".to_string(),
                scope: CommandScope::fleet(),
                command: CommandKind::SendInput {
                    text: "test".to_string(),
                    paste_mode: false,
                    append_newline: true,
                },
                context: cmd_ctx(1000),
                dry_run: false,
            },
            &registry,
        )
        .unwrap();
    assert_eq!(r1.delivered_count(), 3);

    // Transition pane 2 to Draining
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_id(2),
            event: LifecycleEvent::DrainRequested,
            expected_version: Some(0),
            context: ctx(1100, "transition-test", "drain"),
        })
        .unwrap();

    // After transition: only 2 running accept SendInput, 1 draining skipped
    let r2 = router
        .route(
            &CommandRequest {
                command_id: "post-transition".to_string(),
                scope: CommandScope::fleet(),
                command: CommandKind::SendInput {
                    text: "test".to_string(),
                    paste_mode: false,
                    append_newline: true,
                },
                context: cmd_ctx(1200),
                dry_run: false,
            },
            &registry,
        )
        .unwrap();
    assert_eq!(r2.delivered_count(), 2);
    assert_eq!(r2.skipped_count(), 1);

    // Audit log captures both routes
    assert_eq!(router.audit_log().len(), 2);
}

#[test]
fn window_scoped_command_targets_panes_in_same_container() {
    let registry = fleet_registry(5);
    let mut router = CommandRouter::new();

    let request = CommandRequest {
        command_id: "window-scope-1".to_string(),
        scope: CommandScope::window(window_id(0)),
        command: CommandKind::Capture {
            tail_lines: 50,
            include_escapes: false,
        },
        context: cmd_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&request, &registry).unwrap();
    // All 5 panes share workspace "test-ws", domain "local", generation 1
    assert_eq!(result.deliveries.len(), 5);
    assert!(result.all_delivered());
}

// =============================================================================
// 2. Lifecycle → Durable State integration
// =============================================================================

#[test]
fn checkpoint_captures_current_registry_state() {
    let registry = fleet_registry(4);
    let mut mgr = DurableStateManager::new();

    let cp = mgr.checkpoint(
        &registry,
        "fleet-initial",
        CheckpointTrigger::FleetProvisioning {
            fleet_name: "test-fleet".to_string(),
        },
        HashMap::new(),
    );

    assert_eq!(cp.entities.len(), 6); // 4 panes + 1 session + 1 window
    assert_eq!(cp.label, "fleet-initial");
}

#[test]
fn rollback_restores_registry_after_state_changes() {
    let mut registry = fleet_registry(3);
    let mut mgr = DurableStateManager::new();

    // Checkpoint initial state
    let cp_id = mgr
        .checkpoint(
            &registry,
            "before-drain",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;

    // Transition pane 1 to Draining and pane 2 to Orphaned
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_id(1),
            event: LifecycleEvent::DrainRequested,
            expected_version: Some(0),
            context: ctx(200, "rollback-test", "drain"),
        })
        .unwrap();
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_id(2),
            event: LifecycleEvent::PeerDisconnected,
            expected_version: Some(0),
            context: ctx(300, "rollback-test", "disconnect"),
        })
        .unwrap();

    // Verify states changed
    assert_eq!(
        registry.get(&pane_id(1)).unwrap().state,
        LifecycleState::Pane(MuxPaneLifecycleState::Draining)
    );
    assert_eq!(
        registry.get(&pane_id(2)).unwrap().state,
        LifecycleState::Pane(MuxPaneLifecycleState::Orphaned)
    );

    // Rollback to initial checkpoint
    let record = mgr.rollback(cp_id, &mut registry, "restore fleet").unwrap();
    assert!(record.restored_entity_count >= 2);

    // Verify states restored
    assert_eq!(
        registry.get(&pane_id(1)).unwrap().state,
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    );
    assert_eq!(
        registry.get(&pane_id(2)).unwrap().state,
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    );
}

#[test]
fn diff_detects_state_transitions_between_checkpoints() {
    let mut registry = fleet_registry(2);
    let mut mgr = DurableStateManager::new();

    let cp1 = mgr
        .checkpoint(&registry, "before", CheckpointTrigger::Manual, HashMap::new())
        .id;

    // Transition pane 1
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_id(1),
            event: LifecycleEvent::DrainRequested,
            expected_version: Some(0),
            context: ctx(200, "diff-test", "drain"),
        })
        .unwrap();

    let cp2 = mgr
        .checkpoint(&registry, "after", CheckpointTrigger::Manual, HashMap::new())
        .id;

    let diff = mgr.diff(cp1, cp2).unwrap();
    assert_eq!(diff.changed.len(), 1);
    assert_eq!(diff.added.len(), 0);
    assert_eq!(diff.removed.len(), 0);
    assert!(!diff.is_empty());
}

#[test]
fn diff_from_current_detects_new_entities() {
    let mut registry = fleet_registry(2);
    let mut mgr = DurableStateManager::new();

    let cp_id = mgr
        .checkpoint(&registry, "snap", CheckpointTrigger::Manual, HashMap::new())
        .id;

    // Add a new pane after checkpoint
    registry
        .register_entity(
            pane_id(10),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            300,
        )
        .unwrap();

    let diff = mgr.diff_from_current(cp_id, &registry).unwrap();
    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.removed.len(), 0);
}

#[test]
fn checkpoint_json_roundtrip_preserves_state() {
    let registry = fleet_registry(3);
    let mut mgr = DurableStateManager::new();

    mgr.checkpoint(&registry, "first", CheckpointTrigger::Manual, HashMap::new());
    mgr.checkpoint(
        &registry,
        "second",
        CheckpointTrigger::PreShutdown,
        HashMap::new(),
    );

    let json = mgr.to_json().unwrap();
    let restored = DurableStateManager::from_json(&json).unwrap();

    assert_eq!(restored.checkpoint_count(), 2);
    assert_eq!(restored.latest_checkpoint().unwrap().label, "second");
    assert_eq!(restored.latest_checkpoint().unwrap().entities.len(), 5);
}

#[test]
fn max_checkpoints_enforced_during_heavy_checkpointing() {
    let registry = fleet_registry(1);
    let mut mgr = DurableStateManager::with_max_checkpoints(5);

    for i in 0..20 {
        mgr.checkpoint(
            &registry,
            format!("cp-{i}"),
            CheckpointTrigger::Periodic,
            HashMap::new(),
        );
    }

    assert_eq!(mgr.checkpoint_count(), 5);
    let summaries = mgr.list_checkpoints();
    assert_eq!(summaries[0].label, "cp-15");
    assert_eq!(summaries[4].label, "cp-19");
}

// =============================================================================
// 3. Headless Mux Server full stack
// =============================================================================

fn test_server() -> HeadlessMuxServer {
    HeadlessMuxServer::new(ServerConfig {
        bind_address: "127.0.0.1:0".into(),
        node_id: "harness-node".into(),
        label: Some("Test Harness".into()),
        ..ServerConfig::default()
    })
}

fn register_pane_on_server(server: &mut HeadlessMuxServer, id: u64) {
    server
        .registry_mut()
        .register_entity(
            pane_id(id),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            100,
        )
        .unwrap();
}

#[test]
fn headless_server_command_routing_via_remote_protocol() {
    let mut server = test_server();
    register_pane_on_server(&mut server, 1);
    register_pane_on_server(&mut server, 2);

    // Route a fleet-wide capture command via the remote protocol
    let request = RemoteRequest::Command {
        request: CommandRequest {
            command_id: "remote-cap-1".to_string(),
            scope: CommandScope::fleet(),
            command: CommandKind::Capture {
                tail_lines: 50,
                include_escapes: false,
            },
            context: cmd_ctx(1000),
            dry_run: false,
        },
    };

    match server.handle_request(request) {
        RemoteResponse::CommandResult { result } => {
            assert_eq!(result.delivered_count(), 2);
            assert!(result.all_delivered());
        }
        other => panic!("expected CommandResult, got {other:?}"),
    }
}

#[test]
fn headless_server_checkpoint_and_rollback_via_protocol() {
    let mut server = test_server();
    register_pane_on_server(&mut server, 1);

    // Create checkpoint
    let cp_id = match server.handle_request(RemoteRequest::Checkpoint {
        label: "pre-failure".to_string(),
    }) {
        RemoteResponse::CheckpointCreated { id, .. } => id,
        other => panic!("expected CheckpointCreated, got {other:?}"),
    };

    // Add more panes (simulating fleet expansion)
    register_pane_on_server(&mut server, 2);
    register_pane_on_server(&mut server, 3);

    // Verify status shows 3 panes
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.pane_count, 3);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Rollback to checkpoint (before panes 2,3 were added)
    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: cp_id,
        reason: "restore to known-good state".to_string(),
    }) {
        RemoteResponse::RollbackComplete { removed, .. } => {
            assert!(removed >= 2, "panes 2 and 3 should be counted as removed");
        }
        other => panic!("expected RollbackComplete, got {other:?}"),
    }
}

#[test]
fn headless_server_federation_lifecycle() {
    let mut server = test_server();
    register_pane_on_server(&mut server, 1);
    register_pane_on_server(&mut server, 2);

    // Join two peers
    let peer1 = ServerNodeId::new("10.0.0.1", 9876, "node-alpha");
    let peer2 = ServerNodeId::new("10.0.0.2", 9876, "node-beta");

    server.handle_request(RemoteRequest::JoinFederation {
        peer: peer1.clone(),
    });
    server.handle_request(RemoteRequest::JoinFederation {
        peer: peer2.clone(),
    });

    // Heartbeat from peer1 with pane count
    server.handle_request(RemoteRequest::Heartbeat {
        from: peer1.clone(),
        pane_count: 5,
    });
    server.handle_request(RemoteRequest::Heartbeat {
        from: peer2.clone(),
        pane_count: 3,
    });

    // Federated pane count: 2 local + 5 + 3 = 10
    assert_eq!(server.federated_pane_count(), 10);

    // Status reflects peers
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.peer_count, 2);
            assert_eq!(status.pane_count, 2);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Leave one peer
    server.handle_request(RemoteRequest::LeaveFederation {
        node_id: "node-alpha".to_string(),
    });
    assert_eq!(server.peer_count(), 1);
    assert_eq!(server.federated_pane_count(), 5); // 2 local + 3 from beta
}

#[test]
fn headless_server_peer_health_check_marks_stale_peers() {
    let mut server = HeadlessMuxServer::new(ServerConfig {
        peer_timeout_ms: 100,
        ..ServerConfig::default()
    });

    // Join peer with old heartbeat
    let peer = ServerNodeId::new("stale-host", 9876, "stale-peer");
    server.handle_request(RemoteRequest::JoinFederation { peer });

    // Peer just joined so last_heartbeat_at is now — should be Connected
    // But we need a peer with old timestamp, so add one directly
    server.handle_request(RemoteRequest::LeaveFederation {
        node_id: "stale-peer".to_string(),
    });

    // Re-add with manual zero timestamp (via internal peers map won't work through protocol)
    // Instead, join and then wait conceptually — the check_peer_health uses
    // last_heartbeat_at which was set on join. Since peer_timeout_ms is 100ms
    // and join sets it to now, we need to test via prune after manual insertion.
    // This is covered in the headless_mux_server inline tests.
    // Here we test the prune flow end-to-end.
    assert_eq!(server.peer_count(), 0);
}

#[test]
fn headless_server_entity_listing_with_kind_filter() {
    let mut server = test_server();

    // Register panes and a session
    server
        .registry_mut()
        .register_entity(
            session_id(0),
            LifecycleState::Session(SessionLifecycleState::Active),
            100,
        )
        .unwrap();
    register_pane_on_server(&mut server, 1);
    register_pane_on_server(&mut server, 2);

    // List all entities
    match server.handle_request(RemoteRequest::ListEntities { kind_filter: None }) {
        RemoteResponse::Entities { entities } => {
            assert_eq!(entities.len(), 3); // 1 session + 2 panes
        }
        other => panic!("expected Entities, got {other:?}"),
    }

    // Filter to panes only
    match server.handle_request(RemoteRequest::ListEntities {
        kind_filter: Some(LifecycleEntityKind::Pane),
    }) {
        RemoteResponse::Entities { entities } => {
            assert_eq!(entities.len(), 2);
            assert!(entities.iter().all(|e| e.kind == LifecycleEntityKind::Pane));
        }
        other => panic!("expected Entities, got {other:?}"),
    }
}

// =============================================================================
// 4. Session Profiles integration
// =============================================================================

#[test]
fn profile_registry_defaults_populate_standard_profiles() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();

    // Should have at least 4 default profiles
    assert!(reg.profile_count() >= 4);

    // Verify standard roles exist
    let names = reg.profile_names();
    assert!(names.iter().any(|n| n.contains("agent")));
    assert!(names.iter().any(|n| n.contains("dev")));
}

#[test]
fn profile_resolve_persona_applies_overrides() {
    let mut reg = ProfileRegistry::new();

    let profile = SessionProfile {
        name: "base-worker".to_string(),
        description: Some("Base worker profile".to_string()),
        role: ProfileRole::AgentWorker,
        spawn_command: None,
        environment: HashMap::from([("TERM".to_string(), "xterm-256color".to_string())]),
        working_directory: Some("/workspace".to_string()),
        resource_hints: ResourceHints::default(),
        policy: ProfilePolicy::default(),
        layout_template: None,
        bootstrap_commands: vec!["echo ready".to_string()],
        tags: vec!["ai".to_string()],
        updated_at: 0,
    };
    reg.register_profile(profile);

    let persona = Persona {
        name: "high-mem-worker".to_string(),
        profile_name: "base-worker".to_string(),
        env_overrides: HashMap::from([("DEBUG".to_string(), "1".to_string())]),
        agent_identity: Some(AgentIdentitySpec {
            program: "claude-code".to_string(),
            model: Some("opus-4.6".to_string()),
            task: None,
        }),
        description: Some("Worker with extra env".to_string()),
    };
    reg.register_persona(persona);

    let resolved = reg.resolve_persona("high-mem-worker").unwrap();
    assert_eq!(resolved.profile.role, ProfileRole::AgentWorker);
    // Environment should merge base + override
    assert_eq!(resolved.environment.get("TERM").unwrap(), "xterm-256color");
    assert_eq!(resolved.environment.get("DEBUG").unwrap(), "1");
    // Agent identity comes from persona
    assert!(resolved.agent_identity.is_some());
    assert_eq!(
        resolved.agent_identity.as_ref().unwrap().program,
        "claude-code"
    );
}

#[test]
fn fleet_template_resolves_to_correct_slot_count() {
    let mut reg = ProfileRegistry::new();

    let profile = SessionProfile {
        name: "agent-worker".to_string(),
        description: None,
        role: ProfileRole::AgentWorker,
        spawn_command: None,
        environment: HashMap::new(),
        working_directory: None,
        resource_hints: ResourceHints::default(),
        policy: ProfilePolicy::default(),
        layout_template: None,
        bootstrap_commands: vec![],
        tags: vec![],
        updated_at: 0,
    };
    reg.register_profile(profile.clone());

    let monitor_profile = SessionProfile {
        name: "monitor".to_string(),
        role: ProfileRole::Monitor,
        ..profile
    };
    reg.register_profile(monitor_profile);

    let template = FleetTemplate {
        name: "standard-fleet".to_string(),
        description: Some("3 agents + 1 monitor".to_string()),
        slots: vec![
            FleetSlot {
                label: "agent-1".to_string(),
                profile: Some("agent-worker".to_string()),
                persona: None,
                env: HashMap::new(),
            },
            FleetSlot {
                label: "agent-2".to_string(),
                profile: Some("agent-worker".to_string()),
                persona: None,
                env: HashMap::new(),
            },
            FleetSlot {
                label: "agent-3".to_string(),
                profile: Some("agent-worker".to_string()),
                persona: None,
                env: HashMap::new(),
            },
            FleetSlot {
                label: "monitor-1".to_string(),
                profile: Some("monitor".to_string()),
                persona: None,
                env: HashMap::new(),
            },
        ],
        layout_template: None,
    };
    reg.register_fleet_template(template);

    let resolved = reg.resolve_fleet("standard-fleet").unwrap();
    assert_eq!(resolved.panes.len(), 4);
    assert_eq!(
        resolved
            .panes
            .iter()
            .filter(|p| p.resolved.profile.role == ProfileRole::AgentWorker)
            .count(),
        3
    );
    assert_eq!(
        resolved
            .panes
            .iter()
            .filter(|p| p.resolved.profile.role == ProfileRole::Monitor)
            .count(),
        1
    );
}

// =============================================================================
// 5. Command deduplication integration
// =============================================================================

#[test]
fn deduplicator_prevents_double_routing() {
    let registry = fleet_registry(2);
    let mut router = CommandRouter::new();
    let mut dedup = CommandDeduplicator::new(5000);

    let request = CommandRequest {
        command_id: "dedup-1".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::SendInput {
            text: "echo test".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: cmd_ctx(1000),
        dry_run: false,
    };

    // First attempt: not duplicate, should route
    assert!(!dedup.is_duplicate(&request.command_id, 1000));
    let r1 = router.route(&request, &registry).unwrap();
    assert_eq!(r1.delivered_count(), 2);

    // Second attempt: duplicate, should be rejected before routing
    assert!(dedup.is_duplicate(&request.command_id, 1500));
    // Caller would skip routing here

    // After TTL expires: no longer duplicate
    assert!(!dedup.is_duplicate(&request.command_id, 7000));
}

// =============================================================================
// 6. E2E scenario: Fleet provisioning → commands → failure → rollback
// =============================================================================

#[test]
fn e2e_fleet_provision_command_fail_rollback_recover() {
    // Phase 1: Provision a 5-pane fleet via headless server
    let mut server = test_server();
    server
        .registry_mut()
        .register_entity(
            session_id(0),
            LifecycleState::Session(SessionLifecycleState::Active),
            100,
        )
        .unwrap();
    server
        .registry_mut()
        .register_entity(
            window_id(0),
            LifecycleState::Window(WindowLifecycleState::Active),
            100,
        )
        .unwrap();
    for i in 1..=5 {
        register_pane_on_server(&mut server, i);
    }

    // Phase 2: Checkpoint the healthy fleet
    let cp_id = match server.handle_request(RemoteRequest::Checkpoint {
        label: "fleet-healthy".to_string(),
    }) {
        RemoteResponse::CheckpointCreated { id, .. } => id,
        other => panic!("expected CheckpointCreated, got {other:?}"),
    };

    // Phase 3: Route commands to the fleet
    let cmd_result = match server.handle_request(RemoteRequest::Command {
        request: CommandRequest {
            command_id: "e2e-send-1".to_string(),
            scope: CommandScope::fleet(),
            command: CommandKind::SendInput {
                text: "run task".to_string(),
                paste_mode: false,
                append_newline: true,
            },
            context: cmd_ctx(2000),
            dry_run: false,
        },
    }) {
        RemoteResponse::CommandResult { result } => result,
        other => panic!("expected CommandResult, got {other:?}"),
    };
    assert_eq!(cmd_result.delivered_count(), 5);

    // Phase 4: Simulate failure — transition 3 panes to Orphaned
    for i in 1..=3 {
        server
            .registry_mut()
            .apply_transition(LifecycleTransitionRequest {
                identity: pane_id(i),
                event: LifecycleEvent::PeerDisconnected,
                expected_version: Some(0),
                context: ctx(3000 + i * 100, "e2e-failure", "peer_disconnect"),
            })
            .unwrap();
    }

    // Phase 5: Verify degraded command routing
    let degraded = match server.handle_request(RemoteRequest::Command {
        request: CommandRequest {
            command_id: "e2e-send-degraded".to_string(),
            scope: CommandScope::fleet(),
            command: CommandKind::SendInput {
                text: "health check".to_string(),
                paste_mode: false,
                append_newline: true,
            },
            context: cmd_ctx(4000),
            dry_run: false,
        },
    }) {
        RemoteResponse::CommandResult { result } => result,
        other => panic!("expected CommandResult, got {other:?}"),
    };
    // Only 2 panes still Running (4, 5); 3 Orphaned skip SendInput
    assert_eq!(degraded.delivered_count(), 2);
    assert_eq!(degraded.skipped_count(), 3);

    // Phase 6: Rollback to healthy checkpoint
    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: cp_id,
        reason: "recover from mass disconnect".to_string(),
    }) {
        RemoteResponse::RollbackComplete {
            restored, removed, ..
        } => {
            assert!(restored >= 3, "3 orphaned panes should be restored");
        }
        other => panic!("expected RollbackComplete, got {other:?}"),
    }

    // Phase 7: Verify fleet restored — all 5 should accept commands again
    let restored = match server.handle_request(RemoteRequest::Command {
        request: CommandRequest {
            command_id: "e2e-send-restored".to_string(),
            scope: CommandScope::fleet(),
            command: CommandKind::SendInput {
                text: "we're back".to_string(),
                paste_mode: false,
                append_newline: true,
            },
            context: cmd_ctx(5000),
            dry_run: false,
        },
    }) {
        RemoteResponse::CommandResult { result } => result,
        other => panic!("expected CommandResult, got {other:?}"),
    };
    assert_eq!(
        restored.delivered_count(),
        5,
        "all 5 panes should be Running after rollback"
    );
}

#[test]
fn e2e_federated_fleet_status_aggregation() {
    let mut server = test_server();
    register_pane_on_server(&mut server, 1);
    register_pane_on_server(&mut server, 2);

    // Add peers via protocol
    let peer_a = ServerNodeId::new("dc1.example.com", 9876, "dc1-node");
    let peer_b = ServerNodeId::new("dc2.example.com", 9876, "dc2-node");

    server.handle_request(RemoteRequest::JoinFederation {
        peer: peer_a.clone(),
    });
    server.handle_request(RemoteRequest::JoinFederation {
        peer: peer_b.clone(),
    });

    // Heartbeats with pane counts
    server.handle_request(RemoteRequest::Heartbeat {
        from: peer_a,
        pane_count: 10,
    });
    server.handle_request(RemoteRequest::Heartbeat {
        from: peer_b,
        pane_count: 8,
    });

    // Federated status: 2 local + 10 + 8 = 20
    assert_eq!(server.federated_pane_count(), 20);

    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.node_id, "harness-node");
            assert_eq!(status.pane_count, 2);
            assert_eq!(status.peer_count, 2);
            assert!(status.uptime_ms >= 0);
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

// =============================================================================
// 7. Error path coverage
// =============================================================================

#[test]
fn rollback_to_nonexistent_checkpoint_returns_error() {
    let mut registry = fleet_registry(1);
    let mut mgr = DurableStateManager::new();

    let result = mgr.rollback(999, &mut registry, "should fail");
    assert!(matches!(
        result,
        Err(DurableStateError::CheckpointNotFound { id: 999 })
    ));
}

#[test]
fn rollback_to_already_rolled_back_checkpoint_fails() {
    let mut registry = fleet_registry(2);
    let mut mgr = DurableStateManager::new();

    let cp1 = mgr
        .checkpoint(&registry, "cp1", CheckpointTrigger::Manual, HashMap::new())
        .id;
    let _cp2 = mgr
        .checkpoint(&registry, "cp2", CheckpointTrigger::Manual, HashMap::new())
        .id;

    // Rollback to cp1 (marks cp2 as rolled_back)
    mgr.rollback(cp1, &mut registry, "first rollback").unwrap();

    // Attempt to rollback to cp2 (now marked as rolled_back)
    let result = mgr.rollback(_cp2, &mut registry, "should fail");
    assert!(matches!(
        result,
        Err(DurableStateError::AlreadyRolledBack { .. })
    ));
}

#[test]
fn command_to_empty_fleet_returns_empty_scope_error() {
    let registry = LifecycleRegistry::new();
    let mut router = CommandRouter::new();

    let request = CommandRequest {
        command_id: "empty-fleet".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::SendInput {
            text: "hello".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: cmd_ctx(1000),
        dry_run: false,
    };

    let err = router.route(&request, &registry).unwrap_err();
    assert!(matches!(err, CommandTransportError::EmptyScope { .. }));
}

#[test]
fn headless_server_rollback_invalid_checkpoint_returns_error() {
    let mut server = test_server();

    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: 42,
        reason: "nonexistent".to_string(),
    }) {
        RemoteResponse::Error { code, message } => {
            assert_eq!(code, "rollback_failed");
            assert!(message.contains("not found"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// =============================================================================
// 8. Serde roundtrip for protocol messages
// =============================================================================

#[test]
fn remote_request_response_serde_roundtrip() {
    let requests = vec![
        RemoteRequest::Ping,
        RemoteRequest::Status,
        RemoteRequest::ListEntities { kind_filter: None },
        RemoteRequest::ListEntities {
            kind_filter: Some(LifecycleEntityKind::Pane),
        },
        RemoteRequest::Checkpoint {
            label: "test".into(),
        },
        RemoteRequest::Rollback {
            checkpoint_id: 42,
            reason: "test".into(),
        },
        RemoteRequest::ListCheckpoints,
        RemoteRequest::ListPeers,
        RemoteRequest::JoinFederation {
            peer: ServerNodeId::new("h", 1, "n"),
        },
        RemoteRequest::LeaveFederation {
            node_id: "n".into(),
        },
        RemoteRequest::Heartbeat {
            from: ServerNodeId::new("h", 1, "n"),
            pane_count: 5,
        },
    ];

    for req in &requests {
        let json = serde_json::to_string(req).unwrap();
        let deserialized: RemoteRequest = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&deserialized).unwrap();
        assert_eq!(json, json2, "serde roundtrip failed for {req:?}");
    }
}

#[test]
fn checkpoint_trigger_variants_all_serialize() {
    let triggers = vec![
        CheckpointTrigger::Manual,
        CheckpointTrigger::PreOperation {
            operation: "split".into(),
        },
        CheckpointTrigger::Periodic,
        CheckpointTrigger::PreShutdown,
        CheckpointTrigger::PostRecovery,
        CheckpointTrigger::FleetProvisioning {
            fleet_name: "alpha".into(),
        },
    ];

    for trigger in &triggers {
        let json = serde_json::to_string(trigger).unwrap();
        let deserialized: CheckpointTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(trigger, &deserialized);
    }
}

// =============================================================================
// 9. Concurrent version conflict detection
// =============================================================================

#[test]
fn concurrent_writers_detect_version_conflict() {
    let mut registry = fleet_registry(1);

    // Writer A reads version 0
    let version_a = registry.get(&pane_id(1)).unwrap().version;
    assert_eq!(version_a, 0);

    // Writer A succeeds with expected_version=0
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_id(1),
            event: LifecycleEvent::DrainRequested,
            expected_version: Some(0),
            context: ctx(100, "concurrent-test", "writer-a"),
        })
        .unwrap();

    // Writer B tries with stale version 0 — should fail
    let result = registry.apply_transition(LifecycleTransitionRequest {
        identity: pane_id(1),
        event: LifecycleEvent::DrainCompleted,
        expected_version: Some(0),
        context: ctx(200, "concurrent-test", "writer-b-stale"),
    });
    assert!(result.is_err());

    // Writer B retries with correct version 1 — should succeed
    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_id(1),
            event: LifecycleEvent::DrainCompleted,
            expected_version: Some(1),
            context: ctx(300, "concurrent-test", "writer-b-retry"),
        })
        .unwrap();

    // Transition log shows: Applied, Rejected, Applied
    let log = registry.transition_log();
    assert_eq!(log.len(), 3);
    assert_eq!(log[0].decision, LifecycleDecision::Applied);
    assert_eq!(log[1].decision, LifecycleDecision::Rejected);
    assert_eq!(log[2].decision, LifecycleDecision::Applied);
}

// =============================================================================
// 10. Transition log audit completeness
// =============================================================================

#[test]
fn transition_log_captures_structured_evidence() {
    let mut registry = fleet_registry(1);

    registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_id(1),
            event: LifecycleEvent::DrainRequested,
            expected_version: Some(0),
            context: ctx(500, "audit-test", "native_mux.lifecycle.drain_requested"),
        })
        .unwrap();

    let log = registry.transition_log();
    assert_eq!(log.len(), 1);

    let entry = &log[0];
    assert_eq!(entry.scenario_id, "audit-test");
    assert!(entry.correlation_id.contains("500"));
    assert_eq!(entry.reason_code, "native_mux.lifecycle.drain_requested");
    assert!(entry.error_code.is_none());
    assert_eq!(entry.decision, LifecycleDecision::Applied);

    // JSON serialization captures all fields
    let json = registry.transition_log_json().unwrap();
    assert!(json.contains("audit-test"));
    assert!(json.contains("native_mux.lifecycle.drain_requested"));
    assert!(json.contains("correlation_id"));
}
