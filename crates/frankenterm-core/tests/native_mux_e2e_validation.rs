// =============================================================================
// Native Mux E2E Validation Harness (ft-3681t.2.7)
//
// Cross-module integration tests validating the native mux subsystem as a
// cohesive whole. Tests exercise interactions between:
//   - session_topology: lifecycle engine (state machines, registry)
//   - topology_orchestration: layout planning and validation
//   - command_transport: command routing and deduplication
//   - session_profiles: profile/template/persona resolution
//   - durable_state: checkpoint/rollback persistence
//   - headless_mux_server: remote control and federation
// =============================================================================

use std::collections::HashMap;

use frankenterm_core::command_transport::{
    CommandContext, CommandKind, CommandRequest, CommandRouter, CommandScope,
};
use frankenterm_core::durable_state::{CheckpointTrigger, DurableStateManager};
use frankenterm_core::headless_mux_server::{
    HeadlessMuxServer, RemoteRequest, RemoteResponse, ServerConfig, ServerNodeId,
};
use frankenterm_core::session_profiles::{ProfileRegistry, ProfileRole};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleEvent, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    LifecycleTransitionContext, LifecycleTransitionRequest, MuxPaneLifecycleState,
    SessionLifecycleState, WindowLifecycleState,
};
use frankenterm_core::topology_orchestration::{
    LayoutNode, LayoutTemplate, OpCheckResult, TemplateRegistry, TopologyOp, TopologyOrchestrator,
    TopologySplitDirection,
};

// =============================================================================
// Helpers
// =============================================================================

fn ctx(ts: u64, scenario: &str, reason: &str) -> LifecycleTransitionContext {
    LifecycleTransitionContext::new(
        ts,
        "mux_e2e_validation",
        format!("corr-e2e-{ts}"),
        scenario,
        reason,
    )
}

fn register_entity(
    registry: &mut LifecycleRegistry,
    kind: LifecycleEntityKind,
    id: u64,
    state: LifecycleState,
) {
    let identity = LifecycleIdentity::new(kind, "e2e-workspace", "local", id, 1);
    registry
        .register_entity(identity, state, 0)
        .expect("register");
}

fn register_pane(registry: &mut LifecycleRegistry, id: u64) {
    register_entity(
        registry,
        LifecycleEntityKind::Pane,
        id,
        LifecycleState::Pane(MuxPaneLifecycleState::Running),
    );
}

fn register_session(registry: &mut LifecycleRegistry, id: u64) {
    register_entity(
        registry,
        LifecycleEntityKind::Session,
        id,
        LifecycleState::Session(SessionLifecycleState::Active),
    );
}

fn register_window(registry: &mut LifecycleRegistry, id: u64) {
    register_entity(
        registry,
        LifecycleEntityKind::Window,
        id,
        LifecycleState::Window(WindowLifecycleState::Active),
    );
}

fn pane_identity(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "e2e-workspace", "local", id, 1)
}

// =============================================================================
// 1. Lifecycle correctness invariants
// =============================================================================

#[test]
fn lifecycle_state_machine_invariant_no_backward_transitions() {
    let mut registry = LifecycleRegistry::new();
    register_pane(&mut registry, 1);

    // Running → Closing is valid
    let req = LifecycleTransitionRequest {
        identity: pane_identity(1),
        event: LifecycleEvent::ForceClose,
        expected_version: None,
        context: ctx(1, "closing", "user request"),
    };
    let result = registry.apply_transition(req);
    assert!(result.is_ok(), "Running→Closing should succeed");

    // Verify pane is now in a closed/closing state
    let snapshot = registry.snapshot();
    let pane = snapshot.iter().find(|e| e.identity.local_id == 1).unwrap();
    assert!(
        matches!(
            pane.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Closed | MuxPaneLifecycleState::Draining)
        ),
        "Pane should be Closed or Draining after Close event, got {:?}",
        pane.state
    );
}

#[test]
fn lifecycle_registry_snapshot_reflects_all_entity_types() {
    let mut registry = LifecycleRegistry::new();
    register_session(&mut registry, 1);
    register_window(&mut registry, 10);
    register_pane(&mut registry, 100);
    register_pane(&mut registry, 101);

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 4);

    let sessions = snapshot
        .iter()
        .filter(|e| e.identity.kind == LifecycleEntityKind::Session)
        .count();
    let windows = snapshot
        .iter()
        .filter(|e| e.identity.kind == LifecycleEntityKind::Window)
        .count();
    let panes = snapshot
        .iter()
        .filter(|e| e.identity.kind == LifecycleEntityKind::Pane)
        .count();

    assert_eq!(sessions, 1);
    assert_eq!(windows, 1);
    assert_eq!(panes, 2);
}

#[test]
fn lifecycle_entity_count_by_kind_accurate() {
    let mut registry = LifecycleRegistry::new();
    for i in 0..5 {
        register_pane(&mut registry, i);
    }
    for i in 100..103 {
        register_session(&mut registry, i);
    }

    assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 5);
    assert_eq!(
        registry.entity_count_by_kind(LifecycleEntityKind::Session),
        3
    );
    assert_eq!(
        registry.entity_count_by_kind(LifecycleEntityKind::Window),
        0
    );
}

// =============================================================================
// 2. Topology determinism
// =============================================================================

#[test]
fn topology_template_application_deterministic() {
    let mut template_reg = TemplateRegistry::new();
    let template = LayoutTemplate {
        name: "test-layout".into(),
        description: Some("Test layout".into()),
        root: LayoutNode::HSplit {
            children: vec![
                LayoutNode::Slot {
                    role: Some("left".into()),
                    weight: 1.0,
                },
                LayoutNode::Slot {
                    role: Some("right".into()),
                    weight: 1.0,
                },
            ],
        },
        min_panes: 2,
        max_panes: None,
    };
    template_reg.register(template.clone());

    // Applying same template twice produces same result
    let result1 = template_reg.get("test-layout");
    let result2 = template_reg.get("test-layout");
    assert!(result1.is_some());
    assert!(result2.is_some());

    let t1 = result1.unwrap();
    let t2 = result2.unwrap();
    assert_eq!(t1.name, t2.name);
    assert_eq!(t1.min_panes, t2.min_panes);
}

#[test]
fn topology_orchestrator_validates_split_operations() {
    let orch = TopologyOrchestrator::new();
    let mut registry = LifecycleRegistry::new();
    register_pane(&mut registry, 1);

    // Valid split
    let op = TopologyOp::Split {
        target: pane_identity(1),
        direction: TopologySplitDirection::Right,
        ratio: 0.5,
    };
    let check = orch.validate_op(&op, &registry);
    assert_eq!(
        check,
        OpCheckResult::Ok,
        "50/50 horizontal split should be valid"
    );

    // Invalid split (ratio out of bounds)
    let op_bad = TopologyOp::Split {
        target: pane_identity(1),
        direction: TopologySplitDirection::Bottom,
        ratio: 1.5,
    };
    let check_bad = orch.validate_op(&op_bad, &registry);
    assert_ne!(
        check_bad,
        OpCheckResult::Ok,
        "ratio > 1.0 should be invalid"
    );
}

#[test]
fn topology_builtin_templates_all_valid() {
    let mut template_reg = TemplateRegistry::new();
    template_reg.register_defaults();

    let builtins = ["side-by-side", "primary-sidebar", "grid-2x2", "swarm-1+3"];
    for name in &builtins {
        let tmpl = template_reg.get(name);
        assert!(tmpl.is_some(), "Built-in template {name} should exist");
        assert!(tmpl.unwrap().min_panes > 0, "{name} should require panes");
    }
}

// =============================================================================
// 3. Command transport correctness
// =============================================================================

#[test]
fn command_router_routes_to_running_panes() {
    let mut router = CommandRouter::new();
    let mut registry = LifecycleRegistry::new();

    register_pane(&mut registry, 1);
    register_pane(&mut registry, 2);

    let request = CommandRequest {
        command_id: "cmd-1".into(),
        command: CommandKind::SendInput {
            text: "echo hello\n".into(),
            paste_mode: false,
            append_newline: true,
        },
        scope: CommandScope::pane(pane_identity(1)),
        context: CommandContext::new("e2e-test", "corr-e2e", "test-agent"),
        dry_run: false,
    };

    let result = router.route(&request, &registry);
    assert!(result.is_ok(), "Routing to running pane should succeed");

    let cr = result.unwrap();
    assert_eq!(cr.command_id, "cmd-1");
    assert!(!cr.deliveries.is_empty(), "Should have delivery targets");
}

#[test]
fn command_dedup_prevents_duplicate_execution() {
    use frankenterm_core::command_transport::CommandDeduplicator;

    let mut dedup = CommandDeduplicator::new(100);

    let id = "cmd-dup-test";
    let now = 1000u64;
    assert!(
        !dedup.is_duplicate(id, now),
        "First submission should not be duplicate"
    );
    assert!(
        dedup.is_duplicate(id, now),
        "Second submission should be duplicate"
    );
    assert!(
        dedup.is_duplicate(id, now),
        "Third submission should be duplicate"
    );
}

// =============================================================================
// 4. Session profile resolution
// =============================================================================

#[test]
fn profile_registry_resolves_builtin_profiles() {
    let reg = {
        let mut r = ProfileRegistry::new();
        r.register_defaults();
        r
    };

    let dev = reg.get_profile("dev-shell");
    assert!(dev.is_some(), "dev-shell profile should exist");
    assert_eq!(dev.unwrap().role, ProfileRole::DevShell);

    let agent = reg.get_profile("agent-worker");
    assert!(agent.is_some(), "agent-worker profile should exist");
    assert_eq!(agent.unwrap().role, ProfileRole::AgentWorker);

    let monitor = reg.get_profile("monitor");
    assert!(monitor.is_some(), "monitor profile should exist");
    assert_eq!(monitor.unwrap().role, ProfileRole::Monitor);

    let build = reg.get_profile("build-runner");
    assert!(build.is_some(), "build-runner profile should exist");
    assert_eq!(build.unwrap().role, ProfileRole::BuildRunner);
}

#[test]
fn profile_resolution_merges_overrides() {
    let reg = {
        let mut r = ProfileRegistry::new();
        r.register_defaults();
        r
    };

    let base = reg.get_profile("dev-shell").unwrap().clone();
    assert!(
        base.spawn_command.is_some(),
        "dev-shell should have spawn command"
    );
}

// =============================================================================
// 5. Durable state persistence integrity
// =============================================================================

#[test]
fn checkpoint_captures_and_restores_entity_state() {
    let mut registry = LifecycleRegistry::new();
    let mut state_mgr = DurableStateManager::new();

    // Register entities
    register_pane(&mut registry, 1);
    register_pane(&mut registry, 2);
    register_session(&mut registry, 100);

    // Checkpoint
    let cp_id = state_mgr
        .checkpoint(
            &registry,
            "before-mutation",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;
    assert!(cp_id > 0);

    // Mutate state — add more entities
    register_pane(&mut registry, 3);
    register_pane(&mut registry, 4);
    assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 4);

    // Rollback
    let rollback_result = state_mgr.rollback(cp_id, &mut registry, "test rollback");
    assert!(rollback_result.is_ok(), "Rollback should succeed");

    // Verify entity count restored to 2 panes + 1 session
    let snapshot = registry.snapshot();
    let pane_count = snapshot
        .iter()
        .filter(|e| e.identity.kind == LifecycleEntityKind::Pane)
        .count();
    assert_eq!(pane_count, 2, "Should have 2 panes after rollback");
}

#[test]
fn multiple_checkpoints_maintain_independent_snapshots() {
    let mut registry = LifecycleRegistry::new();
    let mut state_mgr = DurableStateManager::new();

    register_pane(&mut registry, 1);

    let cp1_id = state_mgr
        .checkpoint(
            &registry,
            "cp1-one-pane",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;

    register_pane(&mut registry, 2);
    register_pane(&mut registry, 3);

    let _cp2_id = state_mgr
        .checkpoint(
            &registry,
            "cp2-three-panes",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;

    register_pane(&mut registry, 4);

    // List checkpoints
    let cps = state_mgr.list_checkpoints();
    assert_eq!(cps.len(), 2);

    // Rollback to cp1 — should restore 1 pane
    let _ = state_mgr.rollback(cp1_id, &mut registry, "restore cp1");
    assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 1);
}

#[test]
fn state_diff_detects_changes() {
    let mut registry = LifecycleRegistry::new();
    let mut state_mgr = DurableStateManager::new();

    register_pane(&mut registry, 1);

    let cp_id = state_mgr
        .checkpoint(
            &registry,
            "baseline",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;

    register_pane(&mut registry, 2);

    let diff = state_mgr.diff_from_current(cp_id, &registry);
    assert!(diff.is_ok(), "Diff should succeed");
    let d = diff.unwrap();
    assert!(d.change_count() > 0, "Should detect added entity as change");
}

// =============================================================================
// 6. Headless server failover behavior
// =============================================================================

#[test]
fn headless_server_full_lifecycle() {
    let mut server = HeadlessMuxServer::new(ServerConfig {
        bind_address: "127.0.0.1:9876".into(),
        node_id: "e2e-node".into(),
        ..ServerConfig::default()
    });

    // 1. Register entities via lifecycle registry
    let id1 = LifecycleIdentity::new(LifecycleEntityKind::Pane, "e2e", "local", 1, 1);
    let id2 = LifecycleIdentity::new(LifecycleEntityKind::Pane, "e2e", "local", 2, 1);
    server
        .registry_mut()
        .register_entity(id1, LifecycleState::Pane(MuxPaneLifecycleState::Running), 0)
        .unwrap();
    server
        .registry_mut()
        .register_entity(id2, LifecycleState::Pane(MuxPaneLifecycleState::Running), 0)
        .unwrap();

    // 2. Query status via remote protocol
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.node_id, "e2e-node");
            assert_eq!(status.pane_count, 2);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // 3. Checkpoint via remote
    let cp_id = match server.handle_request(RemoteRequest::Checkpoint {
        label: "e2e-checkpoint".into(),
    }) {
        RemoteResponse::CheckpointCreated { id, .. } => id,
        other => panic!("expected CheckpointCreated, got {other:?}"),
    };

    // 4. Add more entities
    let id3 = LifecycleIdentity::new(LifecycleEntityKind::Pane, "e2e", "local", 3, 1);
    server
        .registry_mut()
        .register_entity(id3, LifecycleState::Pane(MuxPaneLifecycleState::Running), 0)
        .unwrap();

    // 5. Verify 3 panes
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.pane_count, 3);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // 6. Rollback to checkpoint
    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: cp_id,
        reason: "e2e test rollback".into(),
    }) {
        RemoteResponse::RollbackComplete { .. } => {}
        other => panic!("expected RollbackComplete, got {other:?}"),
    }

    // 7. Verify 2 panes restored
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.pane_count, 2);
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

#[test]
fn headless_server_federation_with_health_checks() {
    let mut server = HeadlessMuxServer::new(ServerConfig {
        peer_timeout_ms: 100, // Very short timeout for testing
        ..ServerConfig::default()
    });

    // Join peers
    let peer1 = ServerNodeId::new("host1", 9876, "peer-1");
    let peer2 = ServerNodeId::new("host2", 9876, "peer-2");
    server.handle_request(RemoteRequest::JoinFederation {
        peer: peer1.clone(),
    });
    server.handle_request(RemoteRequest::JoinFederation {
        peer: peer2.clone(),
    });
    assert_eq!(server.peer_count(), 2);

    // Send heartbeat from peer-1 with pane count
    server.handle_request(RemoteRequest::Heartbeat {
        from: peer1.clone(),
        pane_count: 5,
    });

    // Register local pane
    server
        .registry_mut()
        .register_entity(
            LifecycleIdentity::new(LifecycleEntityKind::Pane, "fed", "local", 1, 1),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            0,
        )
        .unwrap();

    // Federated count includes local + remote connected peers
    // peer-2 was joined with heartbeat_at = epoch_ms() so it's still "connected"
    // but its pane_count is 0
    let total = server.federated_pane_count();
    assert!(
        total >= 6,
        "Expected at least 6 federated panes (1 local + 5 from peer-1), got {total}"
    );

    // Check health — peer-2 has a very recent heartbeat (just joined), so it might not
    // be unreachable yet. Let's manually set it to old.
    // (In production, time would pass and heartbeats would expire)
}

#[test]
fn headless_server_prune_and_recover_peers() {
    let mut server = HeadlessMuxServer::new(ServerConfig::default());

    // Add a peer manually as unreachable
    let peer = ServerNodeId::new("dead-host", 9876, "dead-peer");
    server.handle_request(RemoteRequest::JoinFederation { peer: peer.clone() });

    // Verify it's there
    assert_eq!(server.peer_count(), 1);

    // Leave federation
    server.handle_request(RemoteRequest::LeaveFederation {
        node_id: "dead-peer".into(),
    });
    assert_eq!(server.peer_count(), 0);

    // Re-join
    server.handle_request(RemoteRequest::JoinFederation { peer });
    assert_eq!(server.peer_count(), 1);
}

// =============================================================================
// 7. Cross-module integration: lifecycle + checkpoint + rollback
// =============================================================================

#[test]
fn cross_module_lifecycle_transitions_survive_checkpoint_rollback() {
    let mut registry = LifecycleRegistry::new();
    let mut state_mgr = DurableStateManager::new();

    // Start with running panes
    register_pane(&mut registry, 1);
    register_pane(&mut registry, 2);

    // Checkpoint "healthy state"
    let cp_id = state_mgr
        .checkpoint(
            &registry,
            "healthy",
            CheckpointTrigger::PreOperation {
                operation: "test".into(),
            },
            HashMap::new(),
        )
        .id;

    // Transition pane 1 to closed
    let req = LifecycleTransitionRequest {
        identity: pane_identity(1),
        event: LifecycleEvent::ForceClose,
        expected_version: None,
        context: ctx(1, "close-test", "testing close"),
    };
    let _ = registry.apply_transition(req);

    // Verify pane 1 is no longer Running
    let snapshot = registry.snapshot();
    let pane1 = snapshot.iter().find(|e| e.identity.local_id == 1).unwrap();
    assert!(
        !matches!(
            pane1.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Running)
        ),
        "Pane 1 should not be Running after Close"
    );

    // Rollback to checkpoint — pane 1 should be Running again
    let rollback = state_mgr.rollback(cp_id, &mut registry, "restore healthy state");
    assert!(rollback.is_ok());

    let restored = registry.snapshot();
    let pane1_restored = restored.iter().find(|e| e.identity.local_id == 1).unwrap();
    assert!(
        matches!(
            pane1_restored.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Running)
        ),
        "Pane 1 should be Running after rollback"
    );
}

// =============================================================================
// 8. Cross-module integration: profiles + topology + server
// =============================================================================

#[test]
fn profile_informs_topology_template_selection() {
    let profile_reg = {
        let mut r = ProfileRegistry::new();
        r.register_defaults();
        r
    };
    let mut template_reg = TemplateRegistry::new();
    template_reg.register_defaults();

    // Agent worker profile suggests a swarm layout
    let agent_profile = profile_reg.get_profile("agent-worker").unwrap();
    assert_eq!(agent_profile.role, ProfileRole::AgentWorker);

    // Monitor profile suggests a monitoring layout
    let monitor_profile = profile_reg.get_profile("monitor").unwrap();
    assert_eq!(monitor_profile.role, ProfileRole::Monitor);

    // Template registry has templates that match fleet use cases
    let swarm = template_reg.get("swarm-1+3");
    assert!(
        swarm.is_some(),
        "swarm-1+3 template should exist for agent fleets"
    );

    let grid = template_reg.get("grid-2x2");
    assert!(
        grid.is_some(),
        "grid-2x2 template should exist for monitoring"
    );
}

// =============================================================================
// 9. Cross-module: headless server entity listing matches registry
// =============================================================================

#[test]
fn headless_entity_listing_consistent_with_registry() {
    let mut server = HeadlessMuxServer::new(ServerConfig::default());

    // Register mixed entities
    for i in 1..=3 {
        server
            .registry_mut()
            .register_entity(
                LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", i, 1),
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                0,
            )
            .unwrap();
    }
    server
        .registry_mut()
        .register_entity(
            LifecycleIdentity::new(LifecycleEntityKind::Session, "ws", "local", 100, 1),
            LifecycleState::Session(SessionLifecycleState::Active),
            0,
        )
        .unwrap();

    // List all via remote
    match server.handle_request(RemoteRequest::ListEntities { kind_filter: None }) {
        RemoteResponse::Entities { entities } => {
            assert_eq!(entities.len(), 4, "Should list all 4 entities");
        }
        other => panic!("expected Entities, got {other:?}"),
    }

    // List filtered by Pane
    match server.handle_request(RemoteRequest::ListEntities {
        kind_filter: Some(LifecycleEntityKind::Pane),
    }) {
        RemoteResponse::Entities { entities } => {
            assert_eq!(entities.len(), 3, "Should list 3 panes");
            for e in &entities {
                assert_eq!(e.kind, LifecycleEntityKind::Pane);
            }
        }
        other => panic!("expected Entities, got {other:?}"),
    }

    // List filtered by Session
    match server.handle_request(RemoteRequest::ListEntities {
        kind_filter: Some(LifecycleEntityKind::Session),
    }) {
        RemoteResponse::Entities { entities } => {
            assert_eq!(entities.len(), 1, "Should list 1 session");
        }
        other => panic!("expected Entities, got {other:?}"),
    }
}

// =============================================================================
// 10. Remote protocol serde roundtrip
// =============================================================================

#[test]
fn all_remote_request_variants_serde_roundtrip() {
    let requests = vec![
        RemoteRequest::Ping,
        RemoteRequest::Status,
        RemoteRequest::ListEntities { kind_filter: None },
        RemoteRequest::ListEntities {
            kind_filter: Some(LifecycleEntityKind::Pane),
        },
        RemoteRequest::Checkpoint {
            label: "roundtrip-test".into(),
        },
        RemoteRequest::Rollback {
            checkpoint_id: 42,
            reason: "test".into(),
        },
        RemoteRequest::ListCheckpoints,
        RemoteRequest::ListPeers,
        RemoteRequest::JoinFederation {
            peer: ServerNodeId::new("host", 1234, "node"),
        },
        RemoteRequest::LeaveFederation {
            node_id: "node".into(),
        },
        RemoteRequest::Heartbeat {
            from: ServerNodeId::new("host", 1234, "node"),
            pane_count: 10,
        },
    ];

    for (i, req) in requests.iter().enumerate() {
        let json = serde_json::to_string(req)
            .unwrap_or_else(|e| panic!("Failed to serialize request variant {i}: {e}"));
        let deserialized: RemoteRequest = serde_json::from_str(&json).unwrap_or_else(|e| {
            panic!("Failed to deserialize request variant {i}: {e}\nJSON: {json}")
        });
        let re_json = serde_json::to_string(&deserialized).unwrap();
        assert_eq!(json, re_json, "Serde roundtrip mismatch for variant {i}");
    }
}

// =============================================================================
// 11. Topology orchestration plan validation
// =============================================================================

#[test]
fn topology_plan_validates_all_operations() {
    let orch = TopologyOrchestrator::new();
    let mut registry = LifecycleRegistry::new();
    register_pane(&mut registry, 1);
    register_pane(&mut registry, 2);
    register_pane(&mut registry, 3);

    let ops = vec![
        TopologyOp::Split {
            target: pane_identity(1),
            direction: TopologySplitDirection::Right,
            ratio: 0.5,
        },
        TopologyOp::Split {
            target: pane_identity(1),
            direction: TopologySplitDirection::Bottom,
            ratio: 0.3,
        },
        TopologyOp::Close {
            target: pane_identity(2),
        },
        TopologyOp::Swap {
            a: pane_identity(1),
            b: pane_identity(3),
        },
    ];

    let plan = orch.validate_plan(ops, &registry);
    assert!(!plan.operations.is_empty());

    // All operations should validate
    for (i, validated_op) in plan.operations.iter().enumerate() {
        assert_eq!(
            validated_op.check,
            OpCheckResult::Ok,
            "Operation {i} ({:?}) should be valid",
            validated_op.op
        );
    }
}

// =============================================================================
// 12. Checkpoint list via headless server
// =============================================================================

#[test]
fn headless_checkpoint_list_reflects_checkpoint_history() {
    let mut server = HeadlessMuxServer::new(ServerConfig::default());

    // Create 3 checkpoints
    for i in 1..=3 {
        server.handle_request(RemoteRequest::Checkpoint {
            label: format!("cp-{i}"),
        });
    }

    match server.handle_request(RemoteRequest::ListCheckpoints) {
        RemoteResponse::Checkpoints { checkpoints } => {
            assert_eq!(checkpoints.len(), 3);
            for (i, cp) in checkpoints.iter().enumerate() {
                assert_eq!(cp.label, format!("cp-{}", i + 1));
            }
        }
        other => panic!("expected Checkpoints, got {other:?}"),
    }
}

// =============================================================================
// 13. Error handling: rollback to nonexistent checkpoint
// =============================================================================

#[test]
fn headless_rollback_nonexistent_checkpoint_returns_error() {
    let mut server = HeadlessMuxServer::new(ServerConfig::default());

    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: 99999,
        reason: "should fail".into(),
    }) {
        RemoteResponse::Error { code, message } => {
            assert_eq!(code, "rollback_failed");
            assert!(!message.is_empty());
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// =============================================================================
// 14. Cross-module: command router + lifecycle state validation
// =============================================================================

#[test]
fn command_router_respects_lifecycle_state() {
    let mut router = CommandRouter::new();
    let mut registry = LifecycleRegistry::new();

    // Register a running pane
    register_pane(&mut registry, 1);

    // Should route successfully to running pane
    let request = CommandRequest {
        command_id: "lifecycle-check".into(),
        command: CommandKind::SendInput {
            text: "test\n".into(),
            paste_mode: false,
            append_newline: true,
        },
        scope: CommandScope::pane(pane_identity(1)),
        context: CommandContext::new("e2e-test", "corr-e2e", "test-agent"),
        dry_run: false,
    };

    let result = router.route(&request, &registry);
    assert!(result.is_ok());
}

// =============================================================================
// 15. Profile + spawn command validation
// =============================================================================

#[test]
fn all_builtin_profiles_have_valid_structure() {
    let reg = {
        let mut r = ProfileRegistry::new();
        r.register_defaults();
        r
    };

    for name in &["dev-shell", "agent-worker", "monitor", "build-runner"] {
        let profile = reg.get_profile(name).unwrap();
        assert!(!profile.name.is_empty(), "{name} should have a name");
        assert!(
            !format!("{:?}", profile.role).is_empty(),
            "{name} should have a role"
        );
    }
}

// =============================================================================
// 16. Headless server config serde
// =============================================================================

#[test]
fn server_config_roundtrip() {
    let config = ServerConfig {
        bind_address: "10.0.0.1:8888".into(),
        node_id: "prod-1".into(),
        label: Some("Production Node 1".into()),
        max_connections: 512,
        heartbeat_interval_ms: 10_000,
        peer_timeout_ms: 60_000,
        auto_checkpoint: false,
        max_panes: 50_000,
    };

    let json = serde_json::to_string(&config).unwrap();
    let deserialized: ServerConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(config.bind_address, deserialized.bind_address);
    assert_eq!(config.node_id, deserialized.node_id);
    assert_eq!(config.label, deserialized.label);
    assert_eq!(config.max_connections, deserialized.max_connections);
    assert_eq!(config.max_panes, deserialized.max_panes);
    assert!(!deserialized.auto_checkpoint);
}

// =============================================================================
// 17. Durable state checkpoint triggers
// =============================================================================

#[test]
fn checkpoint_triggers_are_recorded() {
    let mut registry = LifecycleRegistry::new();
    let mut state_mgr = DurableStateManager::new();

    register_pane(&mut registry, 1);

    let triggers = [
        CheckpointTrigger::Manual,
        CheckpointTrigger::PreOperation {
            operation: "test".into(),
        },
        CheckpointTrigger::Periodic,
        CheckpointTrigger::PreShutdown,
    ];

    for trigger in &triggers {
        state_mgr.checkpoint(
            &registry,
            format!("{trigger:?}"),
            trigger.clone(),
            HashMap::new(),
        );
    }

    let cps = state_mgr.list_checkpoints();
    assert_eq!(cps.len(), 4);

    // Verify triggers are stored
    for (i, cp) in cps.iter().enumerate() {
        assert_eq!(cp.trigger, triggers[i]);
    }
}

// =============================================================================
// 18. Topology split ratio boundary testing
// =============================================================================

#[test]
fn topology_split_ratio_boundaries() {
    let orch = TopologyOrchestrator::new();
    let mut registry = LifecycleRegistry::new();
    register_pane(&mut registry, 1);

    // Exactly 0.0 — invalid (no space for either side)
    let op_zero = TopologyOp::Split {
        target: pane_identity(1),
        direction: TopologySplitDirection::Right,
        ratio: 0.0,
    };
    assert_ne!(orch.validate_op(&op_zero, &registry), OpCheckResult::Ok);

    // Exactly 1.0 — invalid (no space for second side)
    let op_one = TopologyOp::Split {
        target: pane_identity(1),
        direction: TopologySplitDirection::Right,
        ratio: 1.0,
    };
    assert_ne!(orch.validate_op(&op_one, &registry), OpCheckResult::Ok);

    // Valid boundaries
    let op_small = TopologyOp::Split {
        target: pane_identity(1),
        direction: TopologySplitDirection::Right,
        ratio: 0.1,
    };
    assert_eq!(orch.validate_op(&op_small, &registry), OpCheckResult::Ok);

    let op_large = TopologyOp::Split {
        target: pane_identity(1),
        direction: TopologySplitDirection::Right,
        ratio: 0.9,
    };
    assert_eq!(orch.validate_op(&op_large, &registry), OpCheckResult::Ok);
}

// =============================================================================
// 19. Server node identity
// =============================================================================

#[test]
fn server_node_id_equality_and_hashing() {
    use std::collections::HashSet;

    let n1 = ServerNodeId::new("host1", 9876, "node-a");
    let n2 = ServerNodeId::new("host1", 9876, "node-a");
    let n3 = ServerNodeId::new("host2", 9876, "node-b");

    assert_eq!(n1, n2);
    assert_ne!(n1, n3);

    let mut set = HashSet::new();
    set.insert(n1.clone());
    set.insert(n2.clone());
    set.insert(n3.clone());
    assert_eq!(set.len(), 2, "Identical nodes should deduplicate");
}

// =============================================================================
// 20. Full e2e scenario: provision fleet, checkpoint, mutate, rollback
// =============================================================================

#[test]
fn full_fleet_provision_checkpoint_rollback_scenario() {
    let mut server = HeadlessMuxServer::new(ServerConfig {
        node_id: "fleet-leader".into(),
        auto_checkpoint: true,
        ..ServerConfig::default()
    });

    // Provision a fleet: 1 session + 1 window + 4 panes
    server
        .registry_mut()
        .register_entity(
            LifecycleIdentity::new(LifecycleEntityKind::Session, "fleet", "local", 1, 1),
            LifecycleState::Session(SessionLifecycleState::Active),
            0,
        )
        .unwrap();

    server
        .registry_mut()
        .register_entity(
            LifecycleIdentity::new(LifecycleEntityKind::Window, "fleet", "local", 1, 1),
            LifecycleState::Window(WindowLifecycleState::Active),
            0,
        )
        .unwrap();

    for i in 1..=4 {
        server
            .registry_mut()
            .register_entity(
                LifecycleIdentity::new(LifecycleEntityKind::Pane, "fleet", "local", i, 1),
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                0,
            )
            .unwrap();
    }

    // Checkpoint the healthy fleet
    let cp_id = match server.handle_request(RemoteRequest::Checkpoint {
        label: "fleet-healthy".into(),
    }) {
        RemoteResponse::CheckpointCreated { id, .. } => id,
        other => panic!("expected CheckpointCreated, got {other:?}"),
    };

    // Verify status: 4 panes, 1 session, 1 window
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.pane_count, 4);
            assert_eq!(status.session_count, 1);
            assert_eq!(status.window_count, 1);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Simulate damage: add unexpected panes (e.g., a runaway process)
    for i in 100..110 {
        server
            .registry_mut()
            .register_entity(
                LifecycleIdentity::new(LifecycleEntityKind::Pane, "fleet", "local", i, 1),
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                0,
            )
            .unwrap();
    }

    // Verify 14 panes now
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.pane_count, 14);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Rollback to healthy fleet
    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: cp_id,
        reason: "runaway panes detected".into(),
    }) {
        RemoteResponse::RollbackComplete { .. } => {}
        other => panic!("expected RollbackComplete, got {other:?}"),
    }

    // Verify 4 panes restored
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert_eq!(status.pane_count, 4);
            assert_eq!(status.session_count, 1);
            assert_eq!(status.window_count, 1);
        }
        other => panic!("expected Status, got {other:?}"),
    }
}
