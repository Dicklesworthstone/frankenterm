// =============================================================================
// Headless/federated mux server for remote fleet control (ft-3681t.2.6)
//
// Production-grade headless mux server mode with remote control channels,
// enabling multi-host swarm operations and connector mesh adjacency without
// GUI coupling. Provides the protocol layer for federated fleet management.
// =============================================================================

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::capability_passport::CapabilityClass;
use crate::capability_passport_store::{PassportKey, PassportStore};
use crate::capability_preflight::{PreflightChecker, PreflightOutcome};
use crate::command_transport::{CommandRequest, CommandResult, CommandRouter};
use crate::durable_state::{CheckpointId, CheckpointTrigger, DurableStateManager};
use crate::phi_accrual_failure_detector::{DEFAULT_SUSPICION_THRESHOLD, PhiAccrualFailureDetector};
use crate::session_topology::{LifecycleEntityKind, LifecycleRegistry, TopologySnapshot};

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
    /// φ-accrual failure detector tracking the inter-arrival distribution
    /// of heartbeats from this peer (ft-roacq, Hayashibara 2004). Skipped
    /// from serde — the empirical distribution is inherently transient
    /// and a fresh detector after restart is the safe default.
    #[serde(skip, default)]
    pub failure_detector: PhiAccrualFailureDetector,
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
    /// Maximum federation peers tracked in self.peers. Prevents a
    /// hostile or buggy counterparty from OOM'ing the server by
    /// spamming JoinFederation with distinct node_ids. Default is
    /// 512 (2× max_connections by default) — well above any
    /// realistic production federation but low enough to keep the
    /// peer map bounded at ~128 KiB. [ft-ry224]
    #[serde(default = "default_max_peers")]
    pub max_peers: u32,
    /// φ-accrual suspicion threshold for peer failure detection
    /// (ft-roacq, Hayashibara 2004). Default 8.0 matches Akka /
    /// Cassandra production defaults; higher values reduce false-
    /// positive rate at the cost of slower failure detection. The
    /// legacy `peer_timeout_ms` field is retained for serde back-
    /// compat but is no longer consulted by `check_peer_health`.
    #[serde(default = "default_suspicion_threshold")]
    pub suspicion_threshold: f64,
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
fn default_max_peers() -> u32 {
    512
}
fn default_suspicion_threshold() -> f64 {
    DEFAULT_SUSPICION_THRESHOLD
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
            max_peers: default_max_peers(),
            suspicion_threshold: default_suspicion_threshold(),
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
    /// ft-1650n.1 slice 3: capability passport preflight gate.
    /// Caller asks whether dispatching an operation that requires
    /// every capability in `required_classes` against the passport
    /// at `key` is permitted at the server's current view of time.
    /// Returns a [`PreflightOutcome`] inside [`RemoteResponse::PreflightOutcome`].
    PassportPreflight {
        key: PassportKey,
        required_classes: Vec<CapabilityClass>,
    },
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
    /// ft-1650n.1 slice 3: passport preflight outcome.
    PreflightOutcome { outcome: PreflightOutcome },
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
    topology_snapshot: Option<TopologySnapshot>,
    router: CommandRouter,
    state_manager: DurableStateManager,
    peers: HashMap<String, PeerInfo>,
    started_at: u64,
    /// ft-1650n.1 slice 3: optional capability passport store. When
    /// present, [`RemoteRequest::PassportPreflight`] consults it to
    /// authorize capability-gated dispatches. When None, preflight
    /// requests fail closed with `PreflightOutcome::MissingPassport`.
    /// Wrapped in `Arc` so multiple `HeadlessMuxServer` instances or
    /// outside subsystems can share the same store.
    passport_preflight: Option<PreflightChecker>,
}

impl HeadlessMuxServer {
    /// Create a new headless mux server.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            registry: LifecycleRegistry::new(),
            topology_snapshot: None,
            router: CommandRouter::new(),
            state_manager: DurableStateManager::new(),
            peers: HashMap::new(),
            started_at: epoch_ms(),
            passport_preflight: None,
        }
    }

    /// ft-1650n.1 slice 3: install a capability passport store + the
    /// default freshness window for preflight requests. Builder-style
    /// so existing `HeadlessMuxServer::new(config)` callers stay
    /// back-compat (preflight requests fail closed when no store is
    /// configured).
    #[must_use]
    pub fn with_passport_store(mut self, store: Arc<PassportStore>) -> Self {
        self.passport_preflight = Some(PreflightChecker::new(store));
        self
    }

    /// ft-1650n.1 slice 3: install a passport store with an explicit
    /// freshness window override.
    #[must_use]
    pub fn with_passport_store_and_freshness(
        mut self,
        store: Arc<PassportStore>,
        max_age_ms: u64,
    ) -> Self {
        self.passport_preflight = Some(PreflightChecker::new(store).with_max_age_ms(max_age_ms));
        self
    }

    /// Access the installed [`PreflightChecker`], if any.
    #[must_use]
    pub fn passport_preflight(&self) -> Option<&PreflightChecker> {
        self.passport_preflight.as_ref()
    }

    /// Access the lifecycle registry.
    pub fn registry(&self) -> &LifecycleRegistry {
        &self.registry
    }

    /// Mutable access to the lifecycle registry.
    pub fn registry_mut(&mut self) -> &mut LifecycleRegistry {
        &mut self.registry
    }

    /// Access the live topology snapshot tracked alongside the registry.
    pub fn topology_snapshot(&self) -> Option<&TopologySnapshot> {
        self.topology_snapshot.as_ref()
    }

    /// Replace the live topology snapshot tracked by the headless server.
    pub fn set_topology_snapshot(&mut self, snapshot: TopologySnapshot) {
        self.topology_snapshot = Some(snapshot);
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
                let cp = self.state_manager.checkpoint_with_topology(
                    &self.registry,
                    self.topology_snapshot.clone(),
                    &label,
                    CheckpointTrigger::Manual,
                    HashMap::new(),
                );
                RemoteResponse::CheckpointCreated { id: cp.id, label }
            }

            RemoteRequest::Rollback {
                checkpoint_id,
                reason,
            } => match self.state_manager.rollback_with_topology(
                checkpoint_id,
                &mut self.registry,
                &mut self.topology_snapshot,
                reason,
            ) {
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
                let now = epoch_ms();

                // [ft-ry224] Bound the peer registry. A hostile or
                // buggy counterparty spamming JoinFederation with
                // distinct node_ids would otherwise grow self.peers
                // until OOM. prune_unreachable_peers only fires
                // after peer_timeout_ms + an external sweep, so a
                // burst is not naturally bounded. Reject new peers
                // past max_peers with a specific error code so the
                // sender can back off or escalate. Re-joins of an
                // already-known node_id are allowed even at cap —
                // capacity is reserved for DISTINCT node_ids.
                let is_rejoin = self.peers.contains_key(&node_id);
                if !is_rejoin && self.peers.len() >= self.config.max_peers as usize {
                    return RemoteResponse::Error {
                        code: "peer_registry_full".into(),
                        message: format!(
                            "federation peer registry is at capacity \
                             ({}/{} peers); refusing to admit {}",
                            self.peers.len(),
                            self.config.max_peers,
                            node_id,
                        ),
                    };
                }

                // [ft-ry224] Preserve first_seen_at on re-join. The
                // previous implementation overwrote the whole
                // PeerInfo on every JoinFederation, including
                // first_seen_at. A peer that re-joined (e.g. the
                // ft-lekgj recovery path: Heartbeat → peer_not_
                // federated → JoinFederation) lost its original
                // federation timestamp. Keep the insert idempotent
                // on the "has this peer ever joined?" metric while
                // refreshing everything else.
                let first_seen_at = self
                    .peers
                    .get(&node_id)
                    .map_or(now, |existing| existing.first_seen_at);

                let mut failure_detector = PhiAccrualFailureDetector::new();
                failure_detector.record_heartbeat(now.saturating_mul(1_000));
                self.peers.insert(
                    node_id.clone(),
                    PeerInfo {
                        node: peer,
                        status: PeerStatus::Connected,
                        pane_count: 0,
                        last_heartbeat_at: now,
                        first_seen_at,
                        capabilities: vec![],
                        failure_detector,
                    },
                );
                RemoteResponse::FederationJoined { node_id }
            }

            RemoteRequest::LeaveFederation { node_id } => {
                self.peers.remove(&node_id);
                RemoteResponse::FederationLeft { node_id }
            }

            RemoteRequest::Heartbeat { from, pane_count } => {
                // [ft-lekgj] Fail closed on unknown peer. Previously the
                // handler silently returned HeartbeatAck for any node_id,
                // producing a split-brain: the peer believed the
                // federation was healthy while the server's `peers` map
                // didn't contain it. Two natural paths reach this state:
                //
                //   1. Post-prune race — peer goes `Unreachable` via
                //      check_peer_health, is removed by
                //      prune_unreachable_peers, then reconnects after a
                //      partition heal. Silent ACK means it never
                //      rejoins.
                //   2. Heartbeat-before-Join — message-reordering on a
                //      lossy link delivers the first Heartbeat before
                //      JoinFederation.
                //
                // A distinct `peer_not_federated` error lets the sender
                // re-send JoinFederation and self-heal; HeartbeatAck is
                // reserved for the case where the server actually
                // updated its state for `from`.
                if let Some(peer) = self.peers.get_mut(&from.node_id) {
                    let now = epoch_ms();
                    peer.last_heartbeat_at = now;
                    peer.failure_detector
                        .record_heartbeat(now.saturating_mul(1_000));
                    peer.pane_count = pane_count;
                    peer.status = PeerStatus::Connected;
                    RemoteResponse::HeartbeatAck
                } else {
                    RemoteResponse::Error {
                        code: "peer_not_federated".into(),
                        message: format!(
                            "node {} has not joined this server; re-send JoinFederation",
                            from.node_id
                        ),
                    }
                }
            }

            RemoteRequest::PassportPreflight {
                key,
                required_classes,
            } => {
                // ft-1650n.1 slice 3: capability passport preflight gate.
                // When no passport store is installed the request fails
                // closed with `MissingPassport` so callers cannot read
                // an Allowed outcome from a server that is not actually
                // tracking capabilities.
                let outcome = match self.passport_preflight.as_ref() {
                    Some(checker) => checker.check(&key, &required_classes, epoch_ms()),
                    None => PreflightOutcome::MissingPassport,
                };
                RemoteResponse::PreflightOutcome { outcome }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Federation management
    // -------------------------------------------------------------------------

    /// Check peer health and mark unreachable peers.
    ///
    /// ft-roacq: replaces the legacy fixed-threshold timeout with the
    /// φ-accrual failure detector (Hayashibara 2004). Each peer carries
    /// its own [`PhiAccrualFailureDetector`] tracking the empirical
    /// inter-arrival distribution of its heartbeats; suspicion crosses
    /// the operator-tuned threshold (`config.suspicion_threshold`,
    /// default 8.0) only when the elapsed time is statistically anomalous
    /// relative to that distribution. Peers with high heartbeat variance
    /// no longer false-positive on a single late arrival.
    pub fn check_peer_health(&mut self) {
        let now_micros = epoch_ms().saturating_mul(1_000);
        let threshold = self.config.suspicion_threshold;

        for peer in self.peers.values_mut() {
            if peer.status == PeerStatus::Connected
                && peer.failure_detector.is_unreachable(now_micros, threshold)
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

    // [ft-ry224] Unbounded peer-map growth DoS: a counterparty
    // spamming JoinFederation with distinct node_ids used to grow
    // self.peers without bound. Now max_peers caps the registry at
    // 512 (default) — the 513th distinct join is rejected with
    // peer_registry_full. Known peers (re-joins) are allowed even
    // at cap because they don't consume a new slot.
    #[test]
    fn join_federation_rejects_beyond_max_peers_ft_ry224() {
        // Shrink max_peers so the test runs fast. Keep all other
        // config at the default.
        let mut server = HeadlessMuxServer::new(ServerConfig {
            bind_address: "127.0.0.1:9876".into(),
            node_id: "test-node".into(),
            label: Some("Test Server".into()),
            max_peers: 3,
            ..ServerConfig::default()
        });

        for i in 0..3 {
            let peer = ServerNodeId::new("host", 9876, &format!("peer-{i}"));
            match server.handle_request(RemoteRequest::JoinFederation { peer }) {
                RemoteResponse::FederationJoined { .. } => {}
                other => panic!("peer-{i} expected FederationJoined, got {other:?}"),
            }
        }
        assert_eq!(server.peer_count(), 3);

        // The fourth DISTINCT peer exceeds max_peers — must be
        // rejected with peer_registry_full, and the registry size
        // stays at the cap.
        let peer4 = ServerNodeId::new("host", 9876, "peer-3");
        match server.handle_request(RemoteRequest::JoinFederation { peer: peer4 }) {
            RemoteResponse::Error { code, message } => {
                assert_eq!(code, "peer_registry_full");
                assert!(
                    message.contains("3/3") && message.contains("peer-3"),
                    "ft-ry224: rejection must cite cap + node_id, got: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(
            server.peer_count(),
            3,
            "ft-ry224: registry must stay at cap"
        );

        // But an EXISTING peer re-joining at cap is fine — re-join
        // doesn't consume a new slot.
        let peer_rejoin = ServerNodeId::new("host", 9876, "peer-0");
        match server.handle_request(RemoteRequest::JoinFederation { peer: peer_rejoin }) {
            RemoteResponse::FederationJoined { node_id } => {
                assert_eq!(node_id, "peer-0");
            }
            other => panic!("expected FederationJoined for re-join, got {other:?}"),
        }
        assert_eq!(server.peer_count(), 3);
    }

    // [ft-ry224] first_seen_at must survive a re-join. The ft-lekgj
    // recovery path (Heartbeat from unknown peer → peer_not_federated
    // → re-JoinFederation) would previously clobber the original
    // federation timestamp on every re-join, making 'how long has
    // this peer been federated' observability lie.
    #[test]
    fn join_federation_preserves_first_seen_at_on_rejoin_ft_ry224() {
        let mut server = make_server();
        let peer = ServerNodeId::new("host", 9876, "persistent-peer");

        server.handle_request(RemoteRequest::JoinFederation { peer: peer.clone() });
        let first_seen = server
            .peers
            .get("persistent-peer")
            .expect("peer must be in registry")
            .first_seen_at;

        // Sleep long enough that epoch_ms() definitely advances.
        // 5ms is well inside SystemTime's resolution on all supported
        // platforms; we only need it to be non-zero.
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Re-join the same node_id.
        server.handle_request(RemoteRequest::JoinFederation { peer });
        let after_rejoin = server
            .peers
            .get("persistent-peer")
            .expect("peer must still be in registry")
            .first_seen_at;

        assert_eq!(
            after_rejoin, first_seen,
            "ft-ry224: first_seen_at must survive a re-join (original {first_seen}, after re-join {after_rejoin})"
        );
        // last_heartbeat_at should have advanced (not required by
        // the contract, but a nice sanity check that the re-join
        // actually touched something).
        assert!(
            server
                .peers
                .get("persistent-peer")
                .unwrap()
                .last_heartbeat_at
                >= first_seen,
            "ft-ry224: last_heartbeat_at must be refreshed on re-join"
        );
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
        let resp = server.handle_request(RemoteRequest::Heartbeat {
            from: peer.clone(),
            pane_count: 42,
        });

        // [ft-lekgj] Regression: known peer still yields HeartbeatAck
        // and state is updated.
        match resp {
            RemoteResponse::HeartbeatAck => {}
            other => panic!("expected HeartbeatAck for known peer, got {other:?}"),
        }
        assert_eq!(server.peers.get("n1").unwrap().pane_count, 42);
    }

    /// [ft-lekgj] A Heartbeat from a node the server never saw via
    /// JoinFederation must NOT silently succeed. The peer needs to
    /// learn that the server has no record of it so it can re-send
    /// JoinFederation and self-heal; a silent HeartbeatAck would leave
    /// the peer believing federation was healthy while the server's
    /// registry excluded it.
    #[test]
    fn ft_lekgj_heartbeat_for_unknown_peer_returns_error() {
        let mut server = make_server();
        let ghost = ServerNodeId::new("phantom", 1234, "never-joined");

        let resp = server.handle_request(RemoteRequest::Heartbeat {
            from: ghost,
            pane_count: 7,
        });

        match resp {
            RemoteResponse::Error { code, message } => {
                assert_eq!(
                    code, "peer_not_federated",
                    "distinct code so clients can pattern-match: got {code}, message={message}",
                );
                assert!(
                    message.contains("never-joined"),
                    "error message should cite the node_id for debuggability: {message}"
                );
            }
            other => panic!("expected Error {{ code: peer_not_federated, .. }}, got {other:?}"),
        }
        assert!(
            server.peers.is_empty(),
            "unknown peer heartbeat must not synthesise a peer entry"
        );
    }

    /// [ft-lekgj] Post-prune race: peer joined, went unreachable,
    /// was removed by prune_unreachable_peers, then sent a fresh
    /// heartbeat (the partition healed). The server must surface
    /// `peer_not_federated` so the peer knows to rejoin.
    #[test]
    fn ft_lekgj_heartbeat_after_prune_returns_error() {
        let mut server = make_server();
        let peer = ServerNodeId::new("host1", 9876, "n1");
        server.handle_request(RemoteRequest::JoinFederation { peer: peer.clone() });

        // Force the peer into Unreachable and then prune it.
        if let Some(p) = server.peers.get_mut("n1") {
            p.status = PeerStatus::Unreachable;
        }
        let pruned = server.prune_unreachable_peers();
        assert_eq!(pruned, vec!["n1".to_string()]);
        assert!(server.peers.is_empty());

        // Peer reconnects and heartbeats — must get the peer_not_federated signal.
        let resp = server.handle_request(RemoteRequest::Heartbeat {
            from: peer,
            pane_count: 1,
        });
        match resp {
            RemoteResponse::Error { code, .. } => {
                assert_eq!(code, "peer_not_federated");
            }
            other => panic!("expected Error after prune, got {other:?}"),
        }
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
                failure_detector: PhiAccrualFailureDetector::default(),
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
            max_peers: 256,
            suspicion_threshold: 12.0,
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
            suspicion_threshold: 8.0,
            ..ServerConfig::default()
        });

        // Seed the detector with one heartbeat at the same very-old
        // timestamp so suspicion-at-now grows unbounded under the
        // warmup distribution (1 s mean, 100 ms stddev floor) — the
        // adaptive replacement of the old fixed-100ms timeout.
        let mut failure_detector = PhiAccrualFailureDetector::new();
        failure_detector.record_heartbeat(0);
        server.peers.insert(
            "stale".into(),
            PeerInfo {
                node: ServerNodeId::new("h", 1, "stale"),
                status: PeerStatus::Connected,
                pane_count: 5,
                last_heartbeat_at: 0, // Very old
                first_seen_at: 0,
                capabilities: vec![],
                failure_detector,
            },
        );

        server.check_peer_health();

        assert_eq!(
            server.peers.get("stale").unwrap().status,
            PeerStatus::Unreachable
        );
    }

    /// ft-roacq: regression — a peer with HIGH heartbeat variance must
    /// NOT false-positive on a single late arrival that the legacy
    /// fixed-threshold timeout would have flagged. Drives the detector
    /// with bursty intervals (alternating 0.5 s / 2 s) for 200 samples,
    /// then probes at a 5 s gap from the last heartbeat. Under the old
    /// `peer_timeout_ms: 1000` semantics this would flip Unreachable;
    /// under φ-accrual at threshold 8.0 the probe stays Connected
    /// because 5 s is unsurprising given the observed variance.
    #[test]
    fn check_peer_health_phi_accrual_tolerates_variance_ft_roacq() {
        let mut server = HeadlessMuxServer::new(ServerConfig {
            // Legacy timeout would trip; φ-accrual must NOT.
            peer_timeout_ms: 1_000,
            suspicion_threshold: 8.0,
            ..ServerConfig::default()
        });

        let mut failure_detector = PhiAccrualFailureDetector::new();
        let mut t_micros: u64 = 1;
        for k in 0..200 {
            failure_detector.record_heartbeat(t_micros);
            t_micros += if k % 2 == 0 { 500_000 } else { 2_000_000 };
        }
        let last_hb_micros = t_micros - 2_000_000;

        server.peers.insert(
            "jittery".into(),
            PeerInfo {
                node: ServerNodeId::new("h", 1, "jittery"),
                status: PeerStatus::Connected,
                pane_count: 1,
                last_heartbeat_at: last_hb_micros / 1_000,
                first_seen_at: 0,
                capabilities: vec![],
                failure_detector,
            },
        );

        // Manually drive the detector forward by 5 s without going
        // through epoch_ms (which would use system time). The peer's
        // suspicion at +5 s should be well below 8.0 given the high-
        // variance distribution.
        let probe_micros = last_hb_micros + 5_000_000;
        let suspicion = server
            .peers
            .get("jittery")
            .unwrap()
            .failure_detector
            .suspicion_at(probe_micros);
        assert!(
            suspicion < 8.0,
            "high-variance peer should tolerate 5 s gap (suspicion={}, threshold=8.0)",
            suspicion
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // ft-1650n.1 slice 3: passport preflight remote endpoint
    // ────────────────────────────────────────────────────────────────────────

    use crate::capability_passport::{
        CapabilityClass as Cap, CapabilityEntry, CapabilityPassport, CapabilityVerification,
        RedactedProof,
    };
    use crate::capability_passport_store::PassportStore;

    fn passport_with_bash_verified(agent: &str, pane_id: u64, observed_at_ms: u64) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: agent.into(),
            pane_id: Some(pane_id),
            capabilities: vec![CapabilityEntry {
                class: Cap::ToolAvailability("bash".into()),
                verification: CapabilityVerification::Verified,
                last_observed_at_ms: Some(observed_at_ms),
                proof: RedactedProof::empty(),
            }],
            generation: 1,
            signed_at_ms: observed_at_ms,
        }
    }

    /// Pre-fix the server had no preflight endpoint at all. This test
    /// pins the new contract: when the server has NO passport store
    /// installed, every preflight request fails closed with
    /// MissingPassport — callers cannot read a permissive outcome
    /// from a server that does not actually track capabilities.
    #[test]
    fn passport_preflight_without_store_fails_closed_ft_1650n_1() {
        let mut server = HeadlessMuxServer::new(ServerConfig::default());
        let resp = server.handle_request(RemoteRequest::PassportPreflight {
            key: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
        });
        match resp {
            RemoteResponse::PreflightOutcome { outcome } => {
                assert_eq!(outcome, PreflightOutcome::MissingPassport);
            }
            other => panic!("expected PreflightOutcome, got {other:?}"),
        }
    }

    /// With a passport store installed and a Verified-and-fresh
    /// capability registered, a preflight request for that capability
    /// is Allowed.
    #[test]
    fn passport_preflight_with_fresh_verified_capability_returns_allowed_ft_1650n_1() {
        // observed_at far in the future so freshness window passes
        // regardless of the server's current epoch_ms() at test time.
        let observed_at_ms = epoch_ms() + 60_000;
        let store = Arc::new(PassportStore::new());
        store.insert(passport_with_bash_verified("cc1", 1, observed_at_ms));

        let mut server = HeadlessMuxServer::new(ServerConfig::default())
            .with_passport_store(store.clone());

        // Sanity: the installed store is observable.
        assert!(server.passport_preflight().is_some());

        let resp = server.handle_request(RemoteRequest::PassportPreflight {
            key: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
        });
        match resp {
            RemoteResponse::PreflightOutcome { outcome } => {
                assert!(
                    outcome.is_allowed(),
                    "expected Allowed for Verified-and-fresh capability, got {outcome:?}"
                );
            }
            other => panic!("expected PreflightOutcome, got {other:?}"),
        }
    }

    /// With a passport store installed but the requested capability
    /// only Declared (not Verified), the preflight returns
    /// MissingCapabilities listing the unmet class — and the
    /// `present_at` field reflects the actual verification level so
    /// the operator can distinguish "never declared" from
    /// "declared but not verified" without inspecting the passport.
    #[test]
    fn passport_preflight_with_declared_only_returns_missing_capabilities_ft_1650n_1() {
        let store = Arc::new(PassportStore::new());
        store.insert(CapabilityPassport {
            agent_id: "cc1".into(),
            pane_id: Some(1),
            capabilities: vec![CapabilityEntry {
                class: Cap::ToolAvailability("bash".into()),
                verification: CapabilityVerification::Declared,
                last_observed_at_ms: None,
                proof: RedactedProof::empty(),
            }],
            generation: 1,
            signed_at_ms: epoch_ms(),
        });

        let mut server =
            HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store);

        let resp = server.handle_request(RemoteRequest::PassportPreflight {
            key: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
        });
        let RemoteResponse::PreflightOutcome { outcome } = resp else {
            panic!("expected PreflightOutcome");
        };
        let PreflightOutcome::MissingCapabilities { unmet, present_at } = outcome else {
            panic!("expected MissingCapabilities for declared-only capability, got {outcome:?}");
        };
        assert_eq!(unmet, vec![Cap::ToolAvailability("bash".into())]);
        assert_eq!(
            present_at,
            vec![(
                Cap::ToolAvailability("bash".into()),
                Some(CapabilityVerification::Declared)
            )]
        );
    }
}
