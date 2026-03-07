// =============================================================================
// Headless/federated mux server for remote fleet control (ft-3681t.2.6)
//
// Production-grade headless mux server mode with remote control channels,
// enabling multi-host swarm operations and connector mesh adjacency without
// GUI coupling. Provides the protocol layer for federated fleet management.
// =============================================================================

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::command_transport::{CommandRequest, CommandResult, CommandRouter};
use crate::durable_state::{CheckpointId, CheckpointTrigger, DurableStateManager};
use crate::session_topology::{LifecycleEntityKind, LifecycleRegistry};

// =============================================================================
// Server identity and federation
// =============================================================================

/// Unique identity for a headless mux server node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerNodeId {
    /// Hostname or IP address.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Unique node ID (UUID or similar).
    pub node_id: String,
    /// Human-readable label.
    #[serde(default)]
    pub label: Option<String>,
}

impl ServerNodeId {
    pub fn new(host: impl Into<String>, port: u16, node_id: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            node_id: node_id.into(),
            label: None,
        }
    }

    /// Stable address string for this node.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Status of a federated peer node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerStatus {
    /// Peer is connected and healthy.
    Connected,
    /// Peer is known but not currently connected.
    Disconnected,
    /// Peer is unreachable.
    Unreachable,
    /// Peer is draining (shutting down gracefully).
    Draining,
}

/// Information about a federated peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// The peer's node identity.
    pub node: ServerNodeId,
    /// Current status.
    pub status: PeerStatus,
    /// Number of panes managed by this peer.
    pub pane_count: u32,
    /// When we last heard from this peer (epoch ms).
    pub last_heartbeat_at: u64,
    /// When this peer was first seen (epoch ms).
    pub first_seen_at: u64,
    /// Peer capabilities.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

// =============================================================================
// Server configuration
// =============================================================================

/// Configuration for a headless mux server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address (host:port).
    pub bind_address: String,
    /// Node identity.
    pub node_id: String,
    /// Human-readable label.
    #[serde(default)]
    pub label: Option<String>,
    /// Maximum concurrent client connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    /// Heartbeat interval for peer federation (ms).
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_ms: u64,
    /// Peer timeout (ms) — peer considered unreachable after this.
    #[serde(default = "default_peer_timeout")]
    pub peer_timeout_ms: u64,
    /// Whether to auto-checkpoint before risky operations.
    #[serde(default = "default_true")]
    pub auto_checkpoint: bool,
    /// Maximum panes this server will manage.
    #[serde(default = "default_max_panes")]
    pub max_panes: u32,
}

fn default_max_connections() -> u32 {
    256
}
fn default_heartbeat_interval() -> u64 {
    5_000
}
fn default_peer_timeout() -> u64 {
    30_000
}
fn default_true() -> bool {
    true
}
fn default_max_panes() -> u32 {
    10_000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0:9876".into(),
            node_id: "default".into(),
            label: None,
            max_connections: default_max_connections(),
            heartbeat_interval_ms: default_heartbeat_interval(),
            peer_timeout_ms: default_peer_timeout(),
            auto_checkpoint: true,
            max_panes: default_max_panes(),
        }
    }
}

// =============================================================================
// Remote control protocol
// =============================================================================

/// A remote control request from a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteRequest {
    /// Execute a command on the mux.
    Command { request: Box<CommandRequest> },
    /// Query server status.
    Status,
    /// List all managed entities.
    ListEntities {
        #[serde(default)]
        kind_filter: Option<LifecycleEntityKind>,
    },
    /// Create a checkpoint.
    Checkpoint { label: String },
    /// Rollback to a checkpoint.
    Rollback {
        checkpoint_id: CheckpointId,
        reason: String,
    },
    /// List checkpoints.
    ListCheckpoints,
    /// List federated peers.
    ListPeers,
    /// Join a federation (register as peer).
    JoinFederation { peer: ServerNodeId },
    /// Leave federation.
    LeaveFederation { node_id: String },
    /// Ping (health check).
    Ping,
    /// Heartbeat from a federated peer.
    Heartbeat { from: ServerNodeId, pane_count: u32 },
}

/// A remote control response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteResponse {
    /// Command execution result.
    CommandResult { result: CommandResult },
    /// Server status.
    Status { status: ServerStatus },
    /// Entity listing.
    Entities { entities: Vec<EntityInfo> },
    /// Checkpoint created.
    CheckpointCreated { id: CheckpointId, label: String },
    /// Rollback completed.
    RollbackComplete { restored: usize, removed: usize },
    /// Checkpoint listing.
    Checkpoints { checkpoints: Vec<CheckpointInfo> },
    /// Peer listing.
    Peers { peers: Vec<PeerInfo> },
    /// Federation join acknowledged.
    FederationJoined { node_id: String },
    /// Federation leave acknowledged.
    FederationLeft { node_id: String },
    /// Pong response.
    Pong { server_time: u64 },
    /// Heartbeat acknowledged.
    HeartbeatAck,
    /// Error response.
    Error { code: String, message: String },
}

/// Summary of server status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub node_id: String,
    pub label: Option<String>,
    pub uptime_ms: u64,
    pub pane_count: u32,
    pub session_count: u32,
    pub window_count: u32,
    pub peer_count: u32,
    pub checkpoint_count: usize,
    pub started_at: u64,
}

/// Summary of an entity for remote listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityInfo {
    pub kind: LifecycleEntityKind,
    pub stable_key: String,
    pub state: String,
}

/// Summary of a checkpoint for remote listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub id: CheckpointId,
    pub label: String,
    pub created_at: u64,
    pub entity_count: usize,
}

// =============================================================================
// Headless mux server
// =============================================================================

/// The headless mux server engine.
///
/// Processes remote control requests against the lifecycle registry,
/// command router, and durable state manager. Manages federation with peers.
pub struct HeadlessMuxServer {
    config: ServerConfig,
    registry: LifecycleRegistry,
    router: CommandRouter,
    state_manager: DurableStateManager,
    peers: HashMap<String, PeerInfo>,
    started_at: u64,
}

impl HeadlessMuxServer {
    /// Create a new headless mux server.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            registry: LifecycleRegistry::new(),
            router: CommandRouter::new(),
            state_manager: DurableStateManager::new(),
            peers: HashMap::new(),
            started_at: epoch_ms(),
        }
    }

    /// Access the lifecycle registry.
    pub fn registry(&self) -> &LifecycleRegistry {
        &self.registry
    }

    /// Mutable access to the lifecycle registry.
    pub fn registry_mut(&mut self) -> &mut LifecycleRegistry {
        &mut self.registry
    }

    /// Access the durable state manager.
    pub fn state_manager(&self) -> &DurableStateManager {
        &self.state_manager
    }

    /// Access the server config.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Get the number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    // -------------------------------------------------------------------------
    // Request handling
    // -------------------------------------------------------------------------

    /// Process a remote control request and return a response.
    pub fn handle_request(&mut self, request: RemoteRequest) -> RemoteResponse {
        match request {
            RemoteRequest::Ping => RemoteResponse::Pong {
                server_time: epoch_ms(),
            },

            RemoteRequest::Status => {
                let status = self.build_status();
                RemoteResponse::Status { status }
            }

            RemoteRequest::Command { request: cmd_req } => {
                match self.router.route(&cmd_req, &self.registry) {
                    Ok(result) => RemoteResponse::CommandResult { result },
                    Err(e) => RemoteResponse::Error {
                        code: "command_failed".into(),
                        message: e.to_string(),
                    },
                }
            }

            RemoteRequest::ListEntities { kind_filter } => {
                let snapshot = self.registry.snapshot();
                let entities: Vec<EntityInfo> = snapshot
                    .iter()
                    .filter(|e| kind_filter.is_none() || Some(e.identity.kind) == kind_filter)
                    .map(|e| EntityInfo {
                        kind: e.identity.kind,
                        stable_key: e.identity.stable_key(),
                        state: format!("{:?}", e.state),
                    })
                    .collect();
                RemoteResponse::Entities { entities }
            }

            RemoteRequest::Checkpoint { label } => {
                let cp = self.state_manager.checkpoint(
                    &self.registry,
                    &label,
                    CheckpointTrigger::Manual,
                    HashMap::new(),
                );
                RemoteResponse::CheckpointCreated { id: cp.id, label }
            }

            RemoteRequest::Rollback {
                checkpoint_id,
                reason,
            } => match self
                .state_manager
                .rollback(checkpoint_id, &mut self.registry, reason)
            {
                Ok(record) => RemoteResponse::RollbackComplete {
                    restored: record.restored_entity_count,
                    removed: record.removed_entity_count,
                },
                Err(e) => RemoteResponse::Error {
                    code: "rollback_failed".into(),
                    message: e.to_string(),
                },
            },

            RemoteRequest::ListCheckpoints => {
                let cps: Vec<CheckpointInfo> = self
                    .state_manager
                    .list_checkpoints()
                    .into_iter()
                    .map(|s| CheckpointInfo {
                        id: s.id,
                        label: s.label,
                        created_at: s.created_at,
                        entity_count: s.entity_count,
                    })
                    .collect();
                RemoteResponse::Checkpoints { checkpoints: cps }
            }

            RemoteRequest::ListPeers => {
                let peers: Vec<PeerInfo> = self.peers.values().cloned().collect();
                RemoteResponse::Peers { peers }
            }

            RemoteRequest::JoinFederation { peer } => {
                let node_id = peer.node_id.clone();
                self.peers.insert(
                    node_id.clone(),
                    PeerInfo {
                        node: peer,
                        status: PeerStatus::Connected,
                        pane_count: 0,
                        last_heartbeat_at: epoch_ms(),
                        first_seen_at: epoch_ms(),
                        capabilities: vec![],
                    },
                );
                RemoteResponse::FederationJoined { node_id }
            }

            RemoteRequest::LeaveFederation { node_id } => {
                self.peers.remove(&node_id);
                RemoteResponse::FederationLeft { node_id }
            }

            RemoteRequest::Heartbeat { from, pane_count } => {
                if let Some(peer) = self.peers.get_mut(&from.node_id) {
                    peer.last_heartbeat_at = epoch_ms();
                    peer.pane_count = pane_count;
                    peer.status = PeerStatus::Connected;
                }
                RemoteResponse::HeartbeatAck
            }
        }
    }

    // -------------------------------------------------------------------------
    // Federation management
    // -------------------------------------------------------------------------

    /// Check peer health and mark unreachable peers.
    pub fn check_peer_health(&mut self) {
        let now = epoch_ms();
        let timeout = self.config.peer_timeout_ms;

        for peer in self.peers.values_mut() {
            if peer.status == PeerStatus::Connected
                && now.saturating_sub(peer.last_heartbeat_at) > timeout
            {
                peer.status = PeerStatus::Unreachable;
            }
        }
    }

    /// Remove unreachable peers.
    pub fn prune_unreachable_peers(&mut self) -> Vec<String> {
        let unreachable: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, p)| p.status == PeerStatus::Unreachable)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &unreachable {
            self.peers.remove(id);
        }

        unreachable
    }

    /// Get total pane count across all federated nodes (including self).
    pub fn federated_pane_count(&self) -> u64 {
        let local = self
            .registry
            .entity_count_by_kind(LifecycleEntityKind::Pane) as u64;
        let remote: u64 = self
            .peers
            .values()
            .filter(|p| p.status == PeerStatus::Connected)
            .map(|p| p.pane_count as u64)
            .sum();
        local + remote
    }

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn build_status(&self) -> ServerStatus {
        let now = epoch_ms();
        ServerStatus {
            node_id: self.config.node_id.clone(),
            label: self.config.label.clone(),
            uptime_ms: now.saturating_sub(self.started_at),
            pane_count: self
                .registry
                .entity_count_by_kind(LifecycleEntityKind::Pane) as u32,
            session_count: self
                .registry
                .entity_count_by_kind(LifecycleEntityKind::Session)
                as u32,
            window_count: self
                .registry
                .entity_count_by_kind(LifecycleEntityKind::Window) as u32,
            peer_count: self.peers.len() as u32,
            checkpoint_count: self.state_manager.checkpoint_count(),
            started_at: self.started_at,
        }
    }
}

// =============================================================================
// Utility
// =============================================================================

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_topology::{
        LifecycleIdentity, LifecycleState, MuxPaneLifecycleState, WindowLifecycleState,
    };

    fn make_server() -> HeadlessMuxServer {
        HeadlessMuxServer::new(ServerConfig {
            bind_address: "127.0.0.1:9876".into(),
            node_id: "test-node".into(),
            label: Some("Test Server".into()),
            ..ServerConfig::default()
        })
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

    // -------------------------------------------------------------------------
    // Ping/pong
    // -------------------------------------------------------------------------

    #[test]
    fn ping_pong() {
        let mut server = make_server();
        match server.handle_request(RemoteRequest::Ping) {
            RemoteResponse::Pong { server_time } => {
                assert!(server_time > 0);
            }
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Status
    // -------------------------------------------------------------------------

    #[test]
    fn status_empty_server() {
        let mut server = make_server();
        match server.handle_request(RemoteRequest::Status) {
            RemoteResponse::Status { status } => {
                assert_eq!(status.node_id, "test-node");
                assert_eq!(status.pane_count, 0);
                assert_eq!(status.peer_count, 0);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn status_with_panes() {
        let mut server = make_server();
        register_pane(&mut server, 1);
        register_pane(&mut server, 2);

        match server.handle_request(RemoteRequest::Status) {
            RemoteResponse::Status { status } => {
                assert_eq!(status.pane_count, 2);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // List entities
    // -------------------------------------------------------------------------

    #[test]
    fn list_entities_all() {
        let mut server = make_server();
        register_pane(&mut server, 1);
        register_pane(&mut server, 2);

        match server.handle_request(RemoteRequest::ListEntities { kind_filter: None }) {
            RemoteResponse::Entities { entities } => {
                assert_eq!(entities.len(), 2);
            }
            other => panic!("expected Entities, got {other:?}"),
        }
    }

    #[test]
    fn list_entities_filtered() {
        let mut server = make_server();
        register_pane(&mut server, 1);

        // Register a window
        server
            .registry_mut()
            .register_entity(
                LifecycleIdentity::new(LifecycleEntityKind::Window, "default", "local", 100, 1),
                LifecycleState::Window(WindowLifecycleState::Active),
                0,
            )
            .unwrap();

        match server.handle_request(RemoteRequest::ListEntities {
            kind_filter: Some(LifecycleEntityKind::Pane),
        }) {
            RemoteResponse::Entities { entities } => {
                assert_eq!(entities.len(), 1);
                assert_eq!(entities[0].kind, LifecycleEntityKind::Pane);
            }
            other => panic!("expected Entities, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Checkpoint/rollback
    // -------------------------------------------------------------------------

    #[test]
    fn checkpoint_via_remote() {
        let mut server = make_server();
        register_pane(&mut server, 1);

        match server.handle_request(RemoteRequest::Checkpoint {
            label: "test-cp".into(),
        }) {
            RemoteResponse::CheckpointCreated { id, label } => {
                assert!(id > 0);
                assert_eq!(label, "test-cp");
            }
            other => panic!("expected CheckpointCreated, got {other:?}"),
        }
    }

    #[test]
    fn list_checkpoints_via_remote() {
        let mut server = make_server();
        server.handle_request(RemoteRequest::Checkpoint {
            label: "cp1".into(),
        });
        server.handle_request(RemoteRequest::Checkpoint {
            label: "cp2".into(),
        });

        match server.handle_request(RemoteRequest::ListCheckpoints) {
            RemoteResponse::Checkpoints { checkpoints } => {
                assert_eq!(checkpoints.len(), 2);
            }
            other => panic!("expected Checkpoints, got {other:?}"),
        }
    }

    #[test]
    fn rollback_via_remote() {
        let mut server = make_server();
        register_pane(&mut server, 1);

        // Create checkpoint
        let cp_id = match server.handle_request(RemoteRequest::Checkpoint {
            label: "before".into(),
        }) {
            RemoteResponse::CheckpointCreated { id, .. } => id,
            _ => panic!("expected CheckpointCreated"),
        };

        // Add more panes
        register_pane(&mut server, 2);
        register_pane(&mut server, 3);

        // Rollback
        match server.handle_request(RemoteRequest::Rollback {
            checkpoint_id: cp_id,
            reason: "test".into(),
        }) {
            RemoteResponse::RollbackComplete { .. } => {}
            other => panic!("expected RollbackComplete, got {other:?}"),
        }
    }

    #[test]
    fn rollback_invalid_checkpoint() {
        let mut server = make_server();

        match server.handle_request(RemoteRequest::Rollback {
            checkpoint_id: 999,
            reason: "fail".into(),
        }) {
            RemoteResponse::Error { code, .. } => {
                assert_eq!(code, "rollback_failed");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Federation
    // -------------------------------------------------------------------------

    #[test]
    fn join_federation() {
        let mut server = make_server();
        let peer = ServerNodeId::new("192.168.1.2", 9876, "peer-1");

        match server.handle_request(RemoteRequest::JoinFederation { peer }) {
            RemoteResponse::FederationJoined { node_id } => {
                assert_eq!(node_id, "peer-1");
            }
            other => panic!("expected FederationJoined, got {other:?}"),
        }

        assert_eq!(server.peer_count(), 1);
    }

    #[test]
    fn leave_federation() {
        let mut server = make_server();
        let peer = ServerNodeId::new("192.168.1.2", 9876, "peer-1");

        server.handle_request(RemoteRequest::JoinFederation { peer });
        server.handle_request(RemoteRequest::LeaveFederation {
            node_id: "peer-1".into(),
        });

        assert_eq!(server.peer_count(), 0);
    }

    #[test]
    fn list_peers() {
        let mut server = make_server();
        server.handle_request(RemoteRequest::JoinFederation {
            peer: ServerNodeId::new("host1", 9876, "n1"),
        });
        server.handle_request(RemoteRequest::JoinFederation {
            peer: ServerNodeId::new("host2", 9876, "n2"),
        });

        match server.handle_request(RemoteRequest::ListPeers) {
            RemoteResponse::Peers { peers } => {
                assert_eq!(peers.len(), 2);
            }
            other => panic!("expected Peers, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_updates_peer() {
        let mut server = make_server();
        let peer = ServerNodeId::new("host1", 9876, "n1");
        server.handle_request(RemoteRequest::JoinFederation { peer: peer.clone() });

        // Send heartbeat with pane count
        server.handle_request(RemoteRequest::Heartbeat {
            from: peer.clone(),
            pane_count: 42,
        });

        assert_eq!(server.peers.get("n1").unwrap().pane_count, 42);
    }

    #[test]
    fn federated_pane_count() {
        let mut server = make_server();
        register_pane(&mut server, 1);
        register_pane(&mut server, 2);

        let peer = ServerNodeId::new("host1", 9876, "n1");
        server.handle_request(RemoteRequest::JoinFederation { peer: peer.clone() });
        server.handle_request(RemoteRequest::Heartbeat {
            from: peer,
            pane_count: 10,
        });

        assert_eq!(server.federated_pane_count(), 12); // 2 local + 10 remote
    }

    #[test]
    fn prune_unreachable_peers() {
        let mut server = make_server();

        // Add a peer and immediately mark as unreachable
        server.peers.insert(
            "dead-peer".into(),
            PeerInfo {
                node: ServerNodeId::new("host", 9876, "dead-peer"),
                status: PeerStatus::Unreachable,
                pane_count: 0,
                last_heartbeat_at: 0,
                first_seen_at: 0,
                capabilities: vec![],
            },
        );

        let pruned = server.prune_unreachable_peers();
        assert_eq!(pruned, vec!["dead-peer"]);
        assert_eq!(server.peer_count(), 0);
    }

    // -------------------------------------------------------------------------
    // ServerNodeId tests
    // -------------------------------------------------------------------------

    #[test]
    fn server_node_id_address() {
        let node = ServerNodeId::new("192.168.1.1", 9876, "test");
        assert_eq!(node.address(), "192.168.1.1:9876");
    }

    #[test]
    fn server_node_id_serde() {
        let node = ServerNodeId {
            host: "localhost".into(),
            port: 8080,
            node_id: "abc".into(),
            label: Some("dev".into()),
        };
        let json = serde_json::to_string(&node).unwrap();
        let deserialized: ServerNodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(node, deserialized);
    }

    // -------------------------------------------------------------------------
    // ServerConfig tests
    // -------------------------------------------------------------------------

    #[test]
    fn server_config_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address, "0.0.0.0:9876");
        assert_eq!(config.max_connections, 256);
        assert!(config.auto_checkpoint);
        assert_eq!(config.max_panes, 10_000);
    }

    #[test]
    fn server_config_serde() {
        let config = ServerConfig {
            bind_address: "10.0.0.1:9999".into(),
            node_id: "custom".into(),
            label: Some("production".into()),
            max_connections: 1000,
            heartbeat_interval_ms: 10_000,
            peer_timeout_ms: 60_000,
            auto_checkpoint: false,
            max_panes: 50_000,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.bind_address, deserialized.bind_address);
        assert_eq!(config.max_panes, deserialized.max_panes);
    }

    // -------------------------------------------------------------------------
    // RemoteRequest/Response serde
    // -------------------------------------------------------------------------

    #[test]
    fn remote_request_serde_roundtrip() {
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
            RemoteRequest::ListCheckpoints,
            RemoteRequest::ListPeers,
            RemoteRequest::JoinFederation {
                peer: ServerNodeId::new("h", 1, "n"),
            },
            RemoteRequest::LeaveFederation {
                node_id: "n".into(),
            },
        ];

        for req in &requests {
            let json = serde_json::to_string(req).unwrap();
            let deserialized: RemoteRequest = serde_json::from_str(&json).unwrap();
            // Just verify it round-trips without error
            let _ = serde_json::to_string(&deserialized).unwrap();
        }
    }

    // -------------------------------------------------------------------------
    // Peer health check
    // -------------------------------------------------------------------------

    #[test]
    fn check_peer_health_marks_timeout() {
        let mut server = HeadlessMuxServer::new(ServerConfig {
            peer_timeout_ms: 100,
            ..ServerConfig::default()
        });

        server.peers.insert(
            "stale".into(),
            PeerInfo {
                node: ServerNodeId::new("h", 1, "stale"),
                status: PeerStatus::Connected,
                pane_count: 5,
                last_heartbeat_at: 0, // Very old
                first_seen_at: 0,
                capabilities: vec![],
            },
        );

        server.check_peer_health();

        assert_eq!(
            server.peers.get("stale").unwrap().status,
            PeerStatus::Unreachable
        );
    }
}
