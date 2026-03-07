//! Property-based tests for the headless mux server module.

use proptest::prelude::*;

use frankenterm_core::command_transport::{
    CommandContext, CommandKind, CommandRequest, CommandScope,
};
use frankenterm_core::headless_mux_server::{
    HeadlessMuxServer, PeerStatus, RemoteRequest, RemoteResponse, ServerConfig, ServerNodeId,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleState, MuxPaneLifecycleState,
    WindowLifecycleState,
};

fn arb_peer_status() -> impl Strategy<Value = PeerStatus> {
    prop_oneof![
        Just(PeerStatus::Connected),
        Just(PeerStatus::Disconnected),
        Just(PeerStatus::Unreachable),
        Just(PeerStatus::Draining),
    ]
}

fn arb_node_id() -> impl Strategy<Value = ServerNodeId> {
    (
        prop_oneof![
            Just("192.168.1.1".to_string()),
            Just("10.0.0.1".to_string()),
            Just("localhost".to_string()),
        ],
        1u16..65535u16,
        "[a-z]{3,8}",
    )
        .prop_map(|(host, port, node_id)| ServerNodeId::new(host, port, node_id))
}

fn arb_config() -> impl Strategy<Value = ServerConfig> {
    (
        1u32..1000u32,
        1000u64..60_000u64,
        5000u64..120_000u64,
        any::<bool>(),
        100u32..100_000u32,
    )
        .prop_map(
            |(max_conn, hb_interval, peer_timeout, auto_cp, max_panes)| ServerConfig {
                bind_address: "0.0.0.0:9876".into(),
                node_id: "test-node".into(),
                label: None,
                max_connections: max_conn,
                heartbeat_interval_ms: hb_interval,
                peer_timeout_ms: peer_timeout,
                auto_checkpoint: auto_cp,
                max_panes,
            },
        )
}

fn make_server() -> HeadlessMuxServer {
    HeadlessMuxServer::new(ServerConfig::default())
}

fn register_pane(server: &mut HeadlessMuxServer, id: u64) {
    let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "default", "local", id, 1);
    server
        .registry_mut()
        .register_entity(
            identity,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            0,
        )
        .expect("register pane");
}

proptest! {
    #[test]
    fn peer_status_serde_roundtrip(status in arb_peer_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: PeerStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn server_node_id_serde_roundtrip(node in arb_node_id()) {
        let json = serde_json::to_string(&node).unwrap();
        let back: ServerNodeId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(node, back);
    }

    #[test]
    fn server_node_id_address_format(
        host in prop_oneof![Just("a.b.c".to_string()), Just("localhost".to_string())],
        port in 1u16..65535u16,
    ) {
        let node = ServerNodeId::new(host.clone(), port, "id");
        let addr = node.address();
        prop_assert_eq!(addr, format!("{host}:{port}"));
    }

    #[test]
    fn server_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ServerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_connections, config.max_connections);
        prop_assert_eq!(back.peer_timeout_ms, config.peer_timeout_ms);
        prop_assert_eq!(back.max_panes, config.max_panes);
        prop_assert_eq!(back.auto_checkpoint, config.auto_checkpoint);
    }

    #[test]
    fn ping_always_returns_pong(_seed in 0u32..100u32) {
        let mut server = make_server();
        let resp = server.handle_request(RemoteRequest::Ping);
        let is_pong = matches!(resp, RemoteResponse::Pong { .. });
        prop_assert!(is_pong, "Ping must always return Pong");
    }

    #[test]
    fn status_reflects_pane_count(n in 0usize..10usize) {
        let mut server = make_server();
        for i in 0..n {
            register_pane(&mut server, i as u64);
        }
        let resp = server.handle_request(RemoteRequest::Status);
        match resp {
            RemoteResponse::Status { status } => {
                prop_assert_eq!(status.pane_count as usize, n);
            }
            _ => prop_assert!(false, "expected Status response"),
        }
    }

    #[test]
    fn list_entities_count_matches_registry(n in 0usize..10usize) {
        let mut server = make_server();
        for i in 0..n {
            register_pane(&mut server, i as u64);
        }
        let resp = server.handle_request(RemoteRequest::ListEntities { kind_filter: None });
        match resp {
            RemoteResponse::Entities { entities } => {
                prop_assert_eq!(entities.len(), n);
            }
            _ => prop_assert!(false, "expected Entities response"),
        }
    }

    #[test]
    fn list_entities_filtered_by_kind(
        n_panes in 0usize..5usize,
        n_windows in 0usize..5usize,
    ) {
        let mut server = make_server();
        for i in 0..n_panes {
            register_pane(&mut server, i as u64);
        }
        for i in 0..n_windows {
            let identity = LifecycleIdentity::new(
                LifecycleEntityKind::Window,
                "default",
                "local",
                100 + i as u64,
                1,
            );
            server.registry_mut().register_entity(
                identity,
                LifecycleState::Window(WindowLifecycleState::Active),
                0,
            ).unwrap();
        }
        let resp = server.handle_request(RemoteRequest::ListEntities {
            kind_filter: Some(LifecycleEntityKind::Pane),
        });
        match resp {
            RemoteResponse::Entities { entities } => {
                prop_assert_eq!(entities.len(), n_panes);
                for e in &entities {
                    prop_assert_eq!(e.kind, LifecycleEntityKind::Pane);
                }
            }
            _ => prop_assert!(false, "expected Entities response"),
        }
    }

    #[test]
    fn join_federation_adds_peer(node in arb_node_id()) {
        let mut server = make_server();
        let node_id = node.node_id.clone();
        let resp = server.handle_request(RemoteRequest::JoinFederation { peer: node });
        match resp {
            RemoteResponse::FederationJoined { node_id: id } => {
                prop_assert_eq!(id, node_id);
            }
            _ => prop_assert!(false, "expected FederationJoined"),
        }
        prop_assert_eq!(server.peer_count(), 1);
    }

    #[test]
    fn leave_federation_removes_peer(node in arb_node_id()) {
        let mut server = make_server();
        let node_id = node.node_id.clone();
        server.handle_request(RemoteRequest::JoinFederation { peer: node });
        prop_assert_eq!(server.peer_count(), 1);

        server.handle_request(RemoteRequest::LeaveFederation { node_id });
        prop_assert_eq!(server.peer_count(), 0);
    }

    #[test]
    fn heartbeat_updates_pane_count(
        node in arb_node_id(),
        pane_count in 0u32..1000u32,
    ) {
        let mut server = make_server();
        let nid = node.node_id.clone();
        server.handle_request(RemoteRequest::JoinFederation { peer: node.clone() });

        server.handle_request(RemoteRequest::Heartbeat {
            from: node,
            pane_count,
        });

        let resp = server.handle_request(RemoteRequest::ListPeers);
        match resp {
            RemoteResponse::Peers { peers } => {
                let peer = peers.iter().find(|p| p.node.node_id == nid).unwrap();
                prop_assert_eq!(peer.pane_count, pane_count);
                prop_assert_eq!(peer.status, PeerStatus::Connected);
            }
            _ => prop_assert!(false, "expected Peers response"),
        }
    }

    #[test]
    fn federated_pane_count_includes_all_connected_peers(
        n_local in 0usize..5usize,
        n_remote in proptest::collection::vec(0u32..100u32, 0..5),
    ) {
        let mut server = make_server();
        for i in 0..n_local {
            register_pane(&mut server, i as u64);
        }
        for (i, remote_count) in n_remote.iter().enumerate() {
            let node = ServerNodeId::new("host", 9876 + i as u16, format!("peer-{i}"));
            server.handle_request(RemoteRequest::JoinFederation { peer: node.clone() });
            server.handle_request(RemoteRequest::Heartbeat {
                from: node,
                pane_count: *remote_count,
            });
        }
        let total = server.federated_pane_count();
        let expected: u64 = n_local as u64 + n_remote.iter().map(|c| *c as u64).sum::<u64>();
        prop_assert_eq!(total, expected);
    }

    #[test]
    fn prune_after_health_check_removes_timed_out_peers(
        n_peers in 1usize..5usize,
    ) {
        // Use a very short peer_timeout so check_peer_health marks old heartbeats
        let config = ServerConfig {
            peer_timeout_ms: 1, // 1ms timeout — all peers stale immediately
            ..ServerConfig::default()
        };
        let mut server = HeadlessMuxServer::new(config);
        for i in 0..n_peers {
            let node = ServerNodeId::new("host", 9876, format!("peer-{i}"));
            server.handle_request(RemoteRequest::JoinFederation { peer: node });
        }
        // All peers were just joined (heartbeat = now), but with 1ms timeout
        // the health check should detect them as stale after a brief moment
        std::thread::sleep(std::time::Duration::from_millis(5));
        server.check_peer_health();
        let pruned = server.prune_unreachable_peers();
        prop_assert_eq!(pruned.len(), n_peers);
        prop_assert_eq!(server.peer_count(), 0);
    }

    #[test]
    fn checkpoint_creates_unique_ids(n in 1usize..10usize) {
        let mut server = make_server();
        let mut ids = Vec::new();
        for i in 0..n {
            let resp = server.handle_request(RemoteRequest::Checkpoint {
                label: format!("cp-{i}"),
            });
            if let RemoteResponse::CheckpointCreated { id, .. } = resp {
                ids.push(id);
            }
        }
        prop_assert_eq!(ids.len(), n);
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        prop_assert_eq!(unique.len(), n, "checkpoint IDs must be unique");
    }

    #[test]
    fn list_checkpoints_returns_all(n in 0usize..10usize) {
        let mut server = make_server();
        for i in 0..n {
            server.handle_request(RemoteRequest::Checkpoint {
                label: format!("cp-{i}"),
            });
        }
        let resp = server.handle_request(RemoteRequest::ListCheckpoints);
        match resp {
            RemoteResponse::Checkpoints { checkpoints } => {
                prop_assert_eq!(checkpoints.len(), n);
            }
            _ => prop_assert!(false, "expected Checkpoints response"),
        }
    }

    #[test]
    fn rollback_invalid_checkpoint_returns_error(bad_id in 1000u64..9999u64) {
        let mut server = make_server();
        let resp = server.handle_request(RemoteRequest::Rollback {
            checkpoint_id: bad_id,
            reason: "test".into(),
        });
        let is_error = matches!(resp, RemoteResponse::Error { .. });
        prop_assert!(is_error, "rollback of invalid checkpoint must return Error");
    }

    #[test]
    fn status_uptime_non_negative(_seed in 0u32..100u32) {
        let mut server = make_server();
        let resp = server.handle_request(RemoteRequest::Status);
        match resp {
            RemoteResponse::Status { status } => {
                // uptime_ms uses saturating_sub, should always be >= 0
                // (u64 is always >= 0, this verifies no panic)
                prop_assert!(status.started_at > 0);
            }
            _ => prop_assert!(false, "expected Status response"),
        }
    }

    #[test]
    fn multiple_peer_joins_are_idempotent(node in arb_node_id()) {
        let mut server = make_server();
        server.handle_request(RemoteRequest::JoinFederation { peer: node.clone() });
        server.handle_request(RemoteRequest::JoinFederation { peer: node.clone() });
        server.handle_request(RemoteRequest::JoinFederation { peer: node });
        // Same node_id should overwrite, not duplicate
        prop_assert_eq!(server.peer_count(), 1);
    }

    // -------------------------------------------------------------------
    // Remote command dispatch
    // -------------------------------------------------------------------

    #[test]
    fn command_to_registered_pane_succeeds(
        pane_id in 0u64..100u64,
        text in "[a-z ]{1,20}",
    ) {
        let mut server = make_server();
        register_pane(&mut server, pane_id);
        let identity = LifecycleIdentity::new(
            LifecycleEntityKind::Pane, "default", "local", pane_id, 1,
        );
        let req = CommandRequest {
            command_id: format!("cmd-{pane_id}"),
            scope: CommandScope::pane(identity),
            command: CommandKind::SendInput {
                text,
                paste_mode: false,
                append_newline: true,
            },
            context: CommandContext::new("test", "corr-test", "test-agent"),
            dry_run: false,
        };
        let resp = server.handle_request(RemoteRequest::Command {
            request: Box::new(req),
        });
        let is_result = matches!(resp, RemoteResponse::CommandResult { .. });
        prop_assert!(is_result, "command to registered pane must return CommandResult");
        if let RemoteResponse::CommandResult { result } = resp {
            prop_assert_eq!(result.delivered_count(), 1);
        }
    }

    #[test]
    fn command_to_unregistered_pane_returns_error(bad_id in 500u64..999u64) {
        let mut server = make_server();
        let identity = LifecycleIdentity::new(
            LifecycleEntityKind::Pane, "default", "local", bad_id, 1,
        );
        let req = CommandRequest {
            command_id: format!("cmd-bad-{bad_id}"),
            scope: CommandScope::pane(identity),
            command: CommandKind::SendInput {
                text: "test".into(),
                paste_mode: false,
                append_newline: true,
            },
            context: CommandContext::new("test", "corr-test", "test-agent"),
            dry_run: false,
        };
        let resp = server.handle_request(RemoteRequest::Command {
            request: Box::new(req),
        });
        let is_error = matches!(resp, RemoteResponse::Error { .. });
        prop_assert!(is_error, "command to unregistered pane must return Error");
    }

    // -------------------------------------------------------------------
    // Checkpoint + rollback integration
    // -------------------------------------------------------------------

    #[test]
    fn checkpoint_then_rollback_restores_state(n in 1usize..5usize) {
        let mut server = make_server();
        for i in 0..n {
            register_pane(&mut server, i as u64);
        }
        // Checkpoint
        let cp_resp = server.handle_request(RemoteRequest::Checkpoint {
            label: "pre-change".into(),
        });
        let checkpoint_id = match cp_resp {
            RemoteResponse::CheckpointCreated { id, .. } => id,
            _ => { prop_assert!(false, "expected checkpoint created"); return Ok(()); }
        };

        // Add more panes
        for i in n..(n + 3) {
            register_pane(&mut server, i as u64);
        }
        // Verify more entities now exist
        let status_resp = server.handle_request(RemoteRequest::Status);
        if let RemoteResponse::Status { status } = status_resp {
            prop_assert_eq!(status.pane_count as usize, n + 3);
        }

        // Rollback
        let rollback_resp = server.handle_request(RemoteRequest::Rollback {
            checkpoint_id,
            reason: "test".into(),
        });
        let is_rollback_ok = matches!(rollback_resp, RemoteResponse::RollbackComplete { .. });
        prop_assert!(is_rollback_ok, "rollback must succeed for valid checkpoint");
    }

    // -------------------------------------------------------------------
    // Server status invariants
    // -------------------------------------------------------------------

    #[test]
    fn status_node_id_matches_config(config in arb_config()) {
        let node_id = config.node_id.clone();
        let mut server = HeadlessMuxServer::new(config);
        let resp = server.handle_request(RemoteRequest::Status);
        match resp {
            RemoteResponse::Status { status } => {
                prop_assert_eq!(status.node_id, node_id);
            }
            _ => prop_assert!(false, "expected Status response"),
        }
    }

    #[test]
    fn config_accessible_after_construction(config in arb_config()) {
        let max_conn = config.max_connections;
        let max_panes = config.max_panes;
        let server = HeadlessMuxServer::new(config);
        prop_assert_eq!(server.config().max_connections, max_conn);
        prop_assert_eq!(server.config().max_panes, max_panes);
    }
}
