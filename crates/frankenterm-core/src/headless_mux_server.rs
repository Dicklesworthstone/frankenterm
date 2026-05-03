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

use crate::capability_passport::{
    CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
};
use crate::capability_passport_store::{PassportKey, PassportStore};
use crate::capability_preflight::{PreflightChecker, PreflightOutcome};
use crate::command_transport::{CommandRequest, CommandResult, CommandRouter};
use crate::durable_state::{CheckpointId, CheckpointTrigger, DurableStateManager};
use crate::handoff_capsule::{
    CapsuleSection, CapsuleValidationError, HandoffCapsule, SectionApplyResult,
    apply_passport_excerpt_to_store,
};
use crate::phi_accrual_failure_detector::{DEFAULT_SUSPICION_THRESHOLD, PhiAccrualFailureDetector};
use crate::session_topology::{
    LifecycleEngineError, LifecycleEntityKind, LifecycleEntityRecord, LifecycleIdentity,
    LifecycleRegistry, LifecycleState, TopologySnapshot,
};

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
    /// ft-1650n.1 slice 4: capability-gated command dispatch.
    /// Combines a [`PreflightChecker::check`] against `actor` +
    /// `required_classes` with a downstream [`CommandRouter::route`]
    /// call. The router only sees the command if preflight returns
    /// [`PreflightOutcome::Allowed`]; otherwise the response is a
    /// [`RemoteResponse::Error`] with `code = "preflight_denied"` +
    /// the outcome label embedded in the message so operators can
    /// triage without an additional RPC. Existing
    /// [`RemoteRequest::Command`] is preserved unchanged for callers
    /// that do not yet carry capability metadata.
    GuardedCommand {
        actor: PassportKey,
        required_classes: Vec<CapabilityClass>,
        request: Box<CommandRequest>,
    },
    /// ft-1650n.1 slice 5: capability-gated checkpoint creation.
    /// Wraps [`RemoteRequest::Checkpoint`] with a preflight check
    /// against `actor` + `required_classes`. Checkpoint creation is
    /// state-modifying and benefits from explicit capability
    /// authorization. On non-Allowed outcome returns
    /// [`RemoteResponse::Error`] with `code = "preflight_denied"`.
    GuardedCheckpoint {
        actor: PassportKey,
        required_classes: Vec<CapabilityClass>,
        label: String,
    },
    /// ft-1650n.1 slice 5: capability-gated rollback.
    /// Wraps [`RemoteRequest::Rollback`] with a preflight check.
    /// Rollback can DESTROY entities not present in the target
    /// checkpoint, so this is the highest-blast-radius mux operation
    /// — capability gating is most valuable here.
    GuardedRollback {
        actor: PassportKey,
        required_classes: Vec<CapabilityClass>,
        checkpoint_id: CheckpointId,
        reason: String,
    },
    /// ft-1650n.1 slice 6: register a lifecycle entity AND issue a
    /// Declared-only passport in one round-trip. Auto-derives the
    /// `PassportKey` from `(agent_id, identity.local_id)` for pane-
    /// scoped panes; agent-scoped (no pane_id) is supported by
    /// passing `pane_id_override = Some(0)` then ignoring the local_id
    /// in subsequent lookups, but the canonical use is per-pane.
    /// Auto-issues `signed_at_ms = epoch_ms()`.
    RegisterPaneWithPassport {
        identity: LifecycleIdentity,
        state: LifecycleState,
        agent_id: String,
        declared_capabilities: Vec<CapabilityClass>,
    },
    /// ft-1650n.5 slice 4: apply a HandoffCapsule against the
    /// server's own PassportStore. Combines slice-3 of ft-1650n.5
    /// (validate_for_destination + ValidationOutcome) with the
    /// concrete apply_passport_excerpt_to_store helper. The actor's
    /// passport is consulted as the destination passport for the
    /// per-section guard check; only sections that pass land in the
    /// applied summary.
    ApplyHandoffCapsule {
        actor: PassportKey,
        capsule: Box<HandoffCapsule>,
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
    /// ft-1650n.1 slice 6: pane registration acknowledged with the
    /// stable key of the new lifecycle entity. The matching passport
    /// (Declared-only) is now visible in the passport store under
    /// `PassportKey::pane(agent_id, identity.local_id)`.
    PaneRegistered { stable_key: String },
    /// ft-1650n.5 slice 4: capsule application summary. Reports
    /// per-PassportExcerpt apply outcomes plus the indices of every
    /// other section the capsule preflight allowed (caller-side
    /// apply for non-PassportExcerpt sections is operator-driven).
    HandoffApplied { summary: ServerHandoffApplySummary },
    /// Error response.
    Error { code: String, message: String },
}

/// Per-section apply summary for [`RemoteResponse::HandoffApplied`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerHandoffApplySummary {
    /// Indices into `capsule.sections` that preflight allowed.
    pub accepted_indices: Vec<usize>,
    /// Per-section labels for skipped sections + the unmet
    /// CapabilityClasses that caused the skip.
    pub skipped: Vec<HandoffSkipReport>,
    /// Per-PassportExcerpt apply outcomes — one entry per accepted
    /// PassportExcerpt section, in input order.
    pub passport_excerpt_applies: Vec<PassportExcerptApplyReport>,
    /// Count of accepted sections this server's apply path does NOT
    /// yet handle (e.g. ContextSummary, MissionState — operator-side
    /// consumers land in follow-up beads). Surfaced so operators see
    /// what's "accepted but not yet automatically applied".
    pub deferred_to_operator_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffSkipReport {
    pub section_index: usize,
    pub section_label: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassportExcerptApplyReport {
    pub section_index: usize,
    pub agent_id: String,
    pub pane_id: Option<u64>,
    pub disposition: PassportExcerptDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassportExcerptDisposition {
    Applied,
    AlreadyPresent,
    /// Should not happen in practice (the handler only routes
    /// PassportExcerpt sections through the helper); included for
    /// completeness so the response shape is total.
    WrongSectionKind,
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

    /// ft-1650n.1 slice 5: shared preflight gate used by every
    /// `Guarded*` request handler. Returns `Ok(())` iff the
    /// configured [`PreflightChecker`] reports
    /// [`PreflightOutcome::Allowed`] for the actor + required
    /// classes. Returns `Err(outcome)` otherwise — including the
    /// fail-closed `MissingPassport` outcome when no store is
    /// installed (so a misconfigured server cannot silently waive
    /// the capability check). The outcome is plumbed back to the
    /// caller as a `RemoteResponse::Error { code: "preflight_denied",
    /// message: "...outcome={label}..." }` by every handler that
    /// uses this helper.
    fn enforce_preflight(
        &self,
        actor: &PassportKey,
        required_classes: &[CapabilityClass],
    ) -> Result<(), PreflightOutcome> {
        let outcome = match self.passport_preflight.as_ref() {
            Some(checker) => checker.check(actor, required_classes, epoch_ms()),
            None => PreflightOutcome::MissingPassport,
        };
        if outcome.is_allowed() {
            Ok(())
        } else {
            Err(outcome)
        }
    }

    /// Convert a denied preflight outcome into the canonical
    /// `RemoteResponse::Error` shape used by every `Guarded*`
    /// handler. Centralized so the error code + message format stay
    /// consistent across slice 4 (GuardedCommand) and slice 5
    /// (GuardedCheckpoint, GuardedRollback) handlers.
    fn preflight_denied_response(
        actor: &PassportKey,
        outcome: &PreflightOutcome,
    ) -> RemoteResponse {
        RemoteResponse::Error {
            code: "preflight_denied".into(),
            message: format!(
                "passport preflight denied dispatch for actor {:?}: outcome={}",
                actor,
                outcome.label(),
            ),
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

    /// ft-1650n.1 slice 6: register a lifecycle entity AND issue a
    /// Declared-only capability passport for it in one atomic-from-
    /// the-caller's-perspective call. Couples mux session lifecycle
    /// with the passport store so callers cannot accidentally bring
    /// up a pane without a corresponding passport entry.
    ///
    /// The passport ships with `verification = Declared` for every
    /// supplied class — the operator (or an upcoming probe pass)
    /// must promote them to Verified before they satisfy
    /// [`PreflightChecker::check`]. This is the fail-closed
    /// onboarding contract: a fresh pane has a passport but it
    /// authorizes nothing until something verifies the claims.
    ///
    /// When no passport store is installed, this method behaves
    /// identically to `registry_mut().register_entity(...)` — the
    /// passport step is skipped and no error is raised.
    /// When a store is installed, the passport is inserted via
    /// `PassportStore::insert` (no validation gate; callers wanting
    /// validation should use [`PassportValidator`] first).
    ///
    /// Returns the underlying [`LifecycleEntityRecord`] so callers
    /// who just want the registry side can continue chaining.
    pub fn register_pane_with_passport(
        &mut self,
        identity: LifecycleIdentity,
        state: LifecycleState,
        timestamp_ms: u64,
        passport_key: PassportKey,
        declared_capabilities: Vec<CapabilityClass>,
    ) -> Result<LifecycleEntityRecord, LifecycleEngineError> {
        let record = self
            .registry
            .register_entity(identity, state, timestamp_ms)?;

        if let Some(checker) = self.passport_preflight.as_ref() {
            let entries = declared_capabilities
                .into_iter()
                .map(|class| CapabilityEntry {
                    class,
                    verification: CapabilityVerification::Declared,
                    last_observed_at_ms: Some(timestamp_ms),
                    proof: RedactedProof::empty(),
                })
                .collect();
            let passport = CapabilityPassport {
                agent_id: passport_key.agent_id.clone(),
                pane_id: passport_key.pane_id,
                capabilities: entries,
                generation: 1,
                signed_at_ms: timestamp_ms,
            };
            // Passport store is shared via Arc so this insert is
            // visible to every PreflightChecker observing the same
            // store, including subsequent GuardedCommand /
            // GuardedCheckpoint / GuardedRollback handlers.
            checker.store().insert(passport);
        }
        Ok(record)
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

            RemoteRequest::GuardedCommand {
                actor,
                required_classes,
                request: cmd_req,
            } => {
                // ft-1650n.1 slice 4: capability-gated command
                // dispatch. Slice 5 refactored the preflight check
                // into the shared `enforce_preflight` helper so the
                // gate semantics stay consistent across every
                // `Guarded*` handler.
                if let Err(outcome) = self.enforce_preflight(&actor, &required_classes) {
                    return Self::preflight_denied_response(&actor, &outcome);
                }
                match self.router.route(&cmd_req, &self.registry) {
                    Ok(result) => RemoteResponse::CommandResult { result },
                    Err(e) => RemoteResponse::Error {
                        code: "command_failed".into(),
                        message: e.to_string(),
                    },
                }
            }

            RemoteRequest::GuardedCheckpoint {
                actor,
                required_classes,
                label,
            } => {
                // ft-1650n.1 slice 5: capability-gated checkpoint
                // creation. Same fail-closed semantics as
                // GuardedCommand — preflight runs first, only on
                // Allowed do we touch the durable state manager.
                if let Err(outcome) = self.enforce_preflight(&actor, &required_classes) {
                    return Self::preflight_denied_response(&actor, &outcome);
                }
                let cp = self.state_manager.checkpoint_with_topology(
                    &self.registry,
                    self.topology_snapshot.clone(),
                    &label,
                    CheckpointTrigger::Manual,
                    HashMap::new(),
                );
                RemoteResponse::CheckpointCreated { id: cp.id, label }
            }

            RemoteRequest::RegisterPaneWithPassport {
                identity,
                state,
                agent_id,
                declared_capabilities,
            } => {
                // ft-1650n.1 slice 6: combine pane registration with
                // Declared-only passport issuance. PassportKey derives
                // from (agent_id, identity.local_id) so the resulting
                // entry is pane-scoped — matches what GuardedCommand
                // / GuardedCheckpoint / GuardedRollback look up.
                let key = PassportKey::pane(agent_id, identity.local_id);
                let stable_key = identity.stable_key();
                let now = epoch_ms();
                match self.register_pane_with_passport(
                    identity,
                    state,
                    now,
                    key,
                    declared_capabilities,
                ) {
                    Ok(_record) => RemoteResponse::PaneRegistered { stable_key },
                    Err(e) => RemoteResponse::Error {
                        code: "register_failed".into(),
                        message: format!("{e:?}"),
                    },
                }
            }

            RemoteRequest::ApplyHandoffCapsule { actor, capsule } => {
                // ft-1650n.5 slice 4: validate the capsule against
                // the actor's passport, then apply each accepted
                // PassportExcerpt section into the server's own
                // PassportStore. Other accepted section kinds are
                // surfaced in deferred_to_operator_count for the
                // operator's CLI/UX surface to consume.
                let actor_passport = self
                    .passport_preflight
                    .as_ref()
                    .and_then(|c| c.store().get(&actor));
                match capsule.validate_for_destination(actor_passport.as_ref()) {
                    Ok(validation) => {
                        let mut excerpt_applies = Vec::new();
                        let mut deferred = 0usize;
                        for &idx in &validation.accepted {
                            let section = &capsule.sections[idx];
                            match section {
                                CapsuleSection::PassportExcerpt {
                                    passport: excerpt_passport,
                                } => {
                                    let store_ref = self
                                        .passport_preflight
                                        .as_ref()
                                        .map(|c| c.store());
                                    let result = if let Some(store) = store_ref {
                                        apply_passport_excerpt_to_store(section, store)
                                    } else {
                                        // No passport store installed at
                                        // all — server cannot accept any
                                        // PassportExcerpt. Treat as wrong-
                                        // kind for reporting; operator sees
                                        // it under deferred bucket too.
                                        SectionApplyResult::WrongSectionKind
                                    };
                                    let disposition = match result {
                                        SectionApplyResult::Applied => {
                                            PassportExcerptDisposition::Applied
                                        }
                                        SectionApplyResult::AlreadyPresent => {
                                            PassportExcerptDisposition::AlreadyPresent
                                        }
                                        SectionApplyResult::WrongSectionKind => {
                                            PassportExcerptDisposition::WrongSectionKind
                                        }
                                    };
                                    excerpt_applies.push(PassportExcerptApplyReport {
                                        section_index: idx,
                                        agent_id: excerpt_passport.agent_id.clone(),
                                        pane_id: excerpt_passport.pane_id,
                                        disposition,
                                    });
                                }
                                _ => {
                                    deferred += 1;
                                }
                            }
                        }
                        let skipped = validation
                            .skipped
                            .iter()
                            .map(|s| HandoffSkipReport {
                                section_index: s.section_index,
                                section_label: s.section_label.clone(),
                                reason: s.reason.clone(),
                            })
                            .collect();
                        RemoteResponse::HandoffApplied {
                            summary: ServerHandoffApplySummary {
                                accepted_indices: validation.accepted,
                                skipped,
                                passport_excerpt_applies: excerpt_applies,
                                deferred_to_operator_count: deferred,
                            },
                        }
                    }
                    Err(CapsuleValidationError::IntegrityMismatch { .. }) => {
                        RemoteResponse::Error {
                            code: "capsule_integrity_mismatch".into(),
                            message: "handoff capsule integrity hash did not match recomputed digest".into(),
                        }
                    }
                    Err(CapsuleValidationError::UnsupportedVersion { got, expected }) => {
                        RemoteResponse::Error {
                            code: "capsule_unsupported_version".into(),
                            message: format!(
                                "handoff capsule version {got} unsupported (server speaks {expected})"
                            ),
                        }
                    }
                }
            }

            RemoteRequest::GuardedRollback {
                actor,
                required_classes,
                checkpoint_id,
                reason,
            } => {
                // ft-1650n.1 slice 5: capability-gated rollback —
                // the highest-blast-radius mux operation (can DESTROY
                // entities not present in the target checkpoint), so
                // capability gating is most valuable here.
                if let Err(outcome) = self.enforce_preflight(&actor, &required_classes) {
                    return Self::preflight_denied_response(&actor, &outcome);
                }
                match self.state_manager.rollback_with_topology(
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
                }
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

    fn passport_with_bash_verified(
        agent: &str,
        pane_id: u64,
        observed_at_ms: u64,
    ) -> CapabilityPassport {
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

        let mut server =
            HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store.clone());

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

        let mut server = HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store);

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

    // ────────────────────────────────────────────────────────────────────────
    // ft-1650n.1 slice 4: GuardedCommand wires preflight before route
    // ────────────────────────────────────────────────────────────────────────

    use crate::command_transport::{CommandContext, CommandKind, CommandScope};

    fn slice4_send_input_request() -> CommandRequest {
        // Targets pane id=999 which the slice4 tests deliberately do
        // NOT register in their LifecycleRegistry. The router will
        // therefore error with a NOT preflight-related code on the
        // ALLOW path — that's the exact signal we want: preflight
        // passed (otherwise we'd see preflight_denied), the router
        // ran (otherwise we'd see PreflightOutcome), and the failure
        // mode is the registry-side missing-pane error rather than
        // an authorization error.
        CommandRequest {
            command_id: "slice4-cmd".to_string(),
            scope: CommandScope::pane(LifecycleIdentity::new(
                LifecycleEntityKind::Pane,
                "ws1",
                "local",
                999,
                1,
            )),
            command: CommandKind::SendInput {
                text: "noop".to_string(),
                paste_mode: false,
                append_newline: false,
            },
            context: CommandContext {
                timestamp_ms: 0,
                component: "slice4-test".into(),
                correlation_id: "slice4-corr".into(),
                caller_identity: "slice4-actor".into(),
                reason: None,
                policy_trace: None,
            },
            dry_run: false,
        }
    }

    /// Without a passport store the GuardedCommand path MUST fail
    /// closed with `preflight_denied` — the dispatch never reaches
    /// the router. Pre-existing `RemoteRequest::Command` is unchanged
    /// for callers that don't carry capability metadata.
    #[test]
    fn guarded_command_fails_closed_when_no_passport_store_ft_1650n_1() {
        let mut server = HeadlessMuxServer::new(ServerConfig::default());
        let resp = server.handle_request(RemoteRequest::GuardedCommand {
            actor: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
            request: Box::new(slice4_send_input_request()),
        });
        match resp {
            RemoteResponse::Error { code, message } => {
                assert_eq!(code, "preflight_denied");
                assert!(
                    message.contains("missing_passport"),
                    "expected outcome label in error message, got: {message}"
                );
            }
            other => panic!("expected Error preflight_denied, got {other:?}"),
        }
    }

    /// When the actor's passport carries the required capability only
    /// at Declared (not Verified), the GuardedCommand handler returns
    /// `preflight_denied` with `missing_capabilities` in the message —
    /// the router never runs.
    #[test]
    fn guarded_command_denied_when_required_capability_unverified_ft_1650n_1() {
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
        let mut server = HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store);
        let resp = server.handle_request(RemoteRequest::GuardedCommand {
            actor: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
            request: Box::new(slice4_send_input_request()),
        });
        match resp {
            RemoteResponse::Error { code, message } => {
                assert_eq!(code, "preflight_denied");
                assert!(
                    message.contains("missing_capabilities"),
                    "expected missing_capabilities label, got: {message}"
                );
            }
            other => panic!("expected Error preflight_denied, got {other:?}"),
        }
    }

    /// When preflight allows, the router IS invoked and produces a
    /// non-preflight response. Slice4_send_input_request targets a
    /// deliberately-unregistered pane so the router errors with a
    /// command-level code — proves preflight passed AND the router
    /// ran (rather than being short-circuited).
    #[test]
    fn guarded_command_routes_through_when_preflight_allows_ft_1650n_1() {
        let observed_at_ms = epoch_ms() + 60_000;
        let store = Arc::new(PassportStore::new());
        store.insert(passport_with_bash_verified("cc1", 1, observed_at_ms));
        let mut server = HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store);

        let resp = server.handle_request(RemoteRequest::GuardedCommand {
            actor: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
            request: Box::new(slice4_send_input_request()),
        });
        match resp {
            RemoteResponse::Error { code, .. } => {
                // Anything OTHER than preflight_denied proves the
                // router was reached after preflight allowed.
                assert_ne!(
                    code, "preflight_denied",
                    "preflight should have allowed, but got preflight_denied"
                );
            }
            RemoteResponse::CommandResult { .. } => {
                // Acceptable too — depending on router behavior on
                // an unregistered target, the route may produce a
                // CommandResult with delivered_count=0 instead of an
                // Err. Either way, preflight passed.
            }
            other => panic!(
                "unexpected response shape (preflight should have allowed and routed): {other:?}"
            ),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // ft-1650n.1 slice 5: GuardedCheckpoint + GuardedRollback
    // ────────────────────────────────────────────────────────────────────────

    /// Without a passport store the GuardedCheckpoint path MUST fail
    /// closed with `preflight_denied` — durable state is never
    /// touched.
    #[test]
    fn guarded_checkpoint_fails_closed_when_no_passport_store_ft_1650n_1() {
        let mut server = HeadlessMuxServer::new(ServerConfig::default());
        let cp_count_before = server.state_manager().list_checkpoints().len();
        let resp = server.handle_request(RemoteRequest::GuardedCheckpoint {
            actor: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
            label: "denied-cp".into(),
        });
        match resp {
            RemoteResponse::Error { code, message } => {
                assert_eq!(code, "preflight_denied");
                assert!(message.contains("missing_passport"));
            }
            other => panic!("expected preflight_denied, got {other:?}"),
        }
        // Durable state untouched: no checkpoint created.
        assert_eq!(
            server.state_manager().list_checkpoints().len(),
            cp_count_before,
            "checkpoint must NOT be created on preflight denial"
        );
    }

    /// With a Verified-and-fresh passport, GuardedCheckpoint creates
    /// the checkpoint via state_manager.checkpoint_with_topology and
    /// returns CheckpointCreated with the new id.
    #[test]
    fn guarded_checkpoint_dispatches_when_preflight_allows_ft_1650n_1() {
        let observed_at_ms = epoch_ms() + 60_000;
        let store = Arc::new(PassportStore::new());
        store.insert(passport_with_bash_verified("cc1", 1, observed_at_ms));
        let mut server = HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store);

        let cp_count_before = server.state_manager().list_checkpoints().len();
        let resp = server.handle_request(RemoteRequest::GuardedCheckpoint {
            actor: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
            label: "allowed-cp".into(),
        });
        match resp {
            RemoteResponse::CheckpointCreated { label, .. } => {
                assert_eq!(label, "allowed-cp");
            }
            other => panic!("expected CheckpointCreated, got {other:?}"),
        }
        assert_eq!(
            server.state_manager().list_checkpoints().len(),
            cp_count_before + 1,
            "checkpoint should have been added on preflight allow"
        );
    }

    /// Without a passport store the GuardedRollback path MUST fail
    /// closed with `preflight_denied`. Rollback can DESTROY entities
    /// — most-valuable gating point.
    #[test]
    fn guarded_rollback_fails_closed_when_no_passport_store_ft_1650n_1() {
        let mut server = HeadlessMuxServer::new(ServerConfig::default());
        let resp = server.handle_request(RemoteRequest::GuardedRollback {
            actor: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
            checkpoint_id: 0,
            reason: "denied-rollback".into(),
        });
        match resp {
            RemoteResponse::Error { code, message } => {
                assert_eq!(code, "preflight_denied");
                assert!(message.contains("missing_passport"));
            }
            other => panic!("expected preflight_denied, got {other:?}"),
        }
    }

    /// With a Verified-and-fresh passport, GuardedRollback reaches
    /// state_manager.rollback_with_topology. The synthetic checkpoint_id
    /// here doesn't exist so the rollback itself errors with
    /// "rollback_failed" — that's the exact signal that preflight
    /// PASSED (otherwise we'd see "preflight_denied").
    #[test]
    fn guarded_rollback_routes_through_when_preflight_allows_ft_1650n_1() {
        let observed_at_ms = epoch_ms() + 60_000;
        let store = Arc::new(PassportStore::new());
        store.insert(passport_with_bash_verified("cc1", 1, observed_at_ms));
        let mut server = HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store);

        let resp = server.handle_request(RemoteRequest::GuardedRollback {
            actor: PassportKey::pane("cc1", 1),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
            checkpoint_id: 99_999,
            reason: "allowed-rollback".into(),
        });
        match resp {
            RemoteResponse::Error { code, .. } => {
                assert_ne!(
                    code, "preflight_denied",
                    "preflight should have allowed; saw preflight_denied which means short-circuit"
                );
                // Acceptable: rollback_failed (synthetic id not found).
            }
            RemoteResponse::RollbackComplete { .. } => {
                // Also acceptable depending on state_manager behavior.
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // ft-1650n.1 slice 6: register_pane_with_passport (lifecycle integration)
    // ────────────────────────────────────────────────────────────────────────

    /// With a passport store installed, register_pane_with_passport
    /// registers the pane in the lifecycle registry AND issues a
    /// matching Declared-only passport in the store atomically (from
    /// the caller's perspective). The passport's capabilities are
    /// each at CapabilityVerification::Declared — they do NOT
    /// satisfy preflight until promoted to Verified, which is the
    /// fail-closed onboarding contract.
    #[test]
    fn register_pane_with_passport_inserts_in_both_registry_and_store_ft_1650n_1() {
        let store = Arc::new(PassportStore::new());
        let mut server =
            HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store.clone());

        let identity =
            LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws-slice6", "local", 42, 1);
        let key = PassportKey::pane("agent-slice6", 42);

        let record = server
            .register_pane_with_passport(
                identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                12_345,
                key.clone(),
                vec![Cap::ToolAvailability("bash".into())],
            )
            .expect("register should succeed");
        assert_eq!(record.identity.local_id, 42);

        // Registry side: entity is present.
        assert!(server.registry().get(&identity).is_some());

        // Passport store side: passport is present at the requested
        // key with verification=Declared and one capability.
        let passport = store.get(&key).expect("passport must be issued");
        assert_eq!(passport.agent_id, "agent-slice6");
        assert_eq!(passport.pane_id, Some(42));
        assert_eq!(passport.capabilities.len(), 1);
        assert_eq!(
            passport.capabilities[0].verification,
            CapabilityVerification::Declared
        );
        assert_eq!(passport.generation, 1);
        assert_eq!(passport.signed_at_ms, 12_345);
    }

    /// Without a passport store installed, register_pane_with_passport
    /// degrades to plain registry registration — no passport is
    /// produced and no error is raised. This preserves back-compat
    /// for existing call paths that do not yet care about passports.
    #[test]
    fn register_pane_with_passport_no_op_passport_when_no_store_ft_1650n_1() {
        let mut server = HeadlessMuxServer::new(ServerConfig::default());
        let identity =
            LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws-slice6", "local", 7, 1);
        let _ = server
            .register_pane_with_passport(
                identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                42,
                PassportKey::pane("agent-x", 7),
                vec![Cap::ToolAvailability("bash".into())],
            )
            .expect("register should succeed even with no passport store");

        // Registry side: pane is registered.
        assert!(server.registry().get(&identity).is_some());

        // Passport side: server's preflight checker is None — there's
        // nowhere for the passport to land, by design.
        assert!(server.passport_preflight().is_none());
    }

    /// End-to-end via remote endpoint: RegisterPaneWithPassport
    /// returns PaneRegistered with the stable_key + the matching
    /// Declared-only passport is now visible via PassportPreflight
    /// (which returns MissingCapabilities because Declared !=
    /// Verified). Pre-fix the only way to get a passport into the
    /// store was an out-of-band insert; slice 6 makes the
    /// remote-side endpoint the canonical path.
    #[test]
    fn remote_register_pane_with_passport_endpoint_lands_passport_ft_1650n_1() {
        let store = Arc::new(PassportStore::new());
        let mut server = HeadlessMuxServer::new(ServerConfig::default()).with_passport_store(store);

        let identity = LifecycleIdentity::new(
            LifecycleEntityKind::Pane,
            "ws-slice6-remote",
            "local",
            13,
            1,
        );

        let resp = server.handle_request(RemoteRequest::RegisterPaneWithPassport {
            identity: identity.clone(),
            state: LifecycleState::Pane(MuxPaneLifecycleState::Running),
            agent_id: "remote-agent".into(),
            declared_capabilities: vec![Cap::ToolAvailability("bash".into())],
        });
        match resp {
            RemoteResponse::PaneRegistered { stable_key } => {
                assert_eq!(stable_key, identity.stable_key());
            }
            other => panic!("expected PaneRegistered, got {other:?}"),
        }

        // Sanity: the Declared-only passport is queryable via the
        // PassportPreflight endpoint, and the outcome is
        // MissingCapabilities (Declared !== Verified — fail-closed).
        let preflight_resp = server.handle_request(RemoteRequest::PassportPreflight {
            key: PassportKey::pane("remote-agent", 13),
            required_classes: vec![Cap::ToolAvailability("bash".into())],
        });
        match preflight_resp {
            RemoteResponse::PreflightOutcome { outcome } => {
                let PreflightOutcome::MissingCapabilities { unmet, present_at } = outcome else {
                    panic!(
                        "expected MissingCapabilities for Declared-only passport (Declared != Verified)"
                    );
                };
                assert_eq!(unmet.len(), 1);
                assert_eq!(present_at[0].1, Some(CapabilityVerification::Declared));
            }
            other => panic!("expected PreflightOutcome, got {other:?}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // ft-1650n.5 slice 4: ApplyHandoffCapsule remote endpoint
    // ────────────────────────────────────────────────────────────────────────

    use crate::handoff_capsule::{CapsuleEndpoint, CapsuleSection, HandoffCapsule};

    fn handoff_actor_passport_with(verified: &[&str]) -> CapabilityPassport {
        let capabilities = verified
            .iter()
            .map(|s| CapabilityEntry {
                class: Cap::SafetyConstraint((*s).to_string()),
                verification: CapabilityVerification::Verified,
                last_observed_at_ms: Some(1_000_000),
                proof: RedactedProof::from_value(s.as_bytes()),
            })
            .collect();
        CapabilityPassport {
            agent_id: "actor-agent".into(),
            pane_id: Some(11),
            capabilities,
            generation: 1,
            signed_at_ms: 1_000_000,
        }
    }

    fn passport_excerpt_to_inherit(generation: u64) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: "inherited-agent".into(),
            pane_id: Some(99),
            capabilities: vec![CapabilityEntry {
                class: Cap::ToolAvailability("bash".into()),
                verification: CapabilityVerification::Verified,
                last_observed_at_ms: Some(900_000),
                proof: RedactedProof::from_value(b"bash-handshake"),
            }],
            generation,
            signed_at_ms: 900_000,
        }
    }

    fn build_handoff_capsule(
        sections: Vec<CapsuleSection>,
        actor_pane_id: u64,
    ) -> HandoffCapsule {
        HandoffCapsule::build(
            CapsuleEndpoint {
                agent_id: "source-agent".into(),
                pane_id: Some(1),
                label: None,
            },
            CapsuleEndpoint {
                agent_id: "actor-agent".into(),
                pane_id: Some(actor_pane_id),
                label: None,
            },
            sections,
            1_500_000,
        )
    }

    /// When the actor's passport satisfies accept_passport_excerpt and
    /// the capsule carries a PassportExcerpt, the server's passport
    /// store ends up holding the inherited passport.
    #[test]
    fn apply_handoff_capsule_inserts_passport_excerpt_into_store_ft_1650n_5() {
        let store = Arc::new(PassportStore::new());
        store.insert(handoff_actor_passport_with(&["accept_passport_excerpt"]));
        let mut server = HeadlessMuxServer::new(ServerConfig::default())
            .with_passport_store(store.clone());

        let inherited = passport_excerpt_to_inherit(1);
        let capsule = build_handoff_capsule(
            vec![CapsuleSection::PassportExcerpt {
                passport: inherited.clone(),
            }],
            11,
        );

        let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
            actor: PassportKey::pane("actor-agent", 11),
            capsule: Box::new(capsule),
        });
        let RemoteResponse::HandoffApplied { summary } = resp else {
            panic!("expected HandoffApplied, got {resp:?}");
        };
        assert_eq!(summary.accepted_indices, vec![0]);
        assert!(summary.skipped.is_empty());
        assert_eq!(summary.passport_excerpt_applies.len(), 1);
        assert_eq!(
            summary.passport_excerpt_applies[0].disposition,
            PassportExcerptDisposition::Applied
        );
        assert_eq!(summary.deferred_to_operator_count, 0);

        // Inherited passport now lives in the store.
        let inherited_key = PassportKey::pane("inherited-agent", 99);
        assert_eq!(store.get(&inherited_key), Some(inherited));
    }

    /// Re-applying the same capsule twice is idempotent — the second
    /// apply observes AlreadyPresent and does not clobber any later
    /// progress (e.g. probe promotions) made on the destination
    /// passport between the two applies.
    #[test]
    fn apply_handoff_capsule_is_idempotent_on_repeated_apply_ft_1650n_5() {
        let store = Arc::new(PassportStore::new());
        store.insert(handoff_actor_passport_with(&["accept_passport_excerpt"]));
        let mut server = HeadlessMuxServer::new(ServerConfig::default())
            .with_passport_store(store.clone());

        let capsule = build_handoff_capsule(
            vec![CapsuleSection::PassportExcerpt {
                passport: passport_excerpt_to_inherit(1),
            }],
            11,
        );

        let _ = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
            actor: PassportKey::pane("actor-agent", 11),
            capsule: Box::new(capsule.clone()),
        });
        let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
            actor: PassportKey::pane("actor-agent", 11),
            capsule: Box::new(capsule),
        });
        let RemoteResponse::HandoffApplied { summary } = resp else {
            panic!("expected HandoffApplied, got {resp:?}");
        };
        assert_eq!(
            summary.passport_excerpt_applies[0].disposition,
            PassportExcerptDisposition::AlreadyPresent
        );
    }

    /// When a section's required CapabilityClass is not Verified in
    /// the actor's passport, that section is skipped — the server's
    /// store is NOT modified for that section.
    #[test]
    fn apply_handoff_capsule_skips_sections_actor_cannot_satisfy_ft_1650n_5() {
        // Actor has NO accept_passport_excerpt capability.
        let store = Arc::new(PassportStore::new());
        store.insert(handoff_actor_passport_with(&["unrelated_constraint"]));
        let mut server = HeadlessMuxServer::new(ServerConfig::default())
            .with_passport_store(store.clone());

        let inherited = passport_excerpt_to_inherit(1);
        let capsule = build_handoff_capsule(
            vec![CapsuleSection::PassportExcerpt {
                passport: inherited.clone(),
            }],
            11,
        );

        let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
            actor: PassportKey::pane("actor-agent", 11),
            capsule: Box::new(capsule),
        });
        let RemoteResponse::HandoffApplied { summary } = resp else {
            panic!("expected HandoffApplied, got {resp:?}");
        };
        assert!(summary.accepted_indices.is_empty());
        assert_eq!(summary.skipped.len(), 1);
        assert_eq!(summary.skipped[0].section_label, "passport_excerpt");
        assert!(summary.passport_excerpt_applies.is_empty());

        // Store does NOT contain the inherited passport.
        assert!(store.get(&PassportKey::pane("inherited-agent", 99)).is_none());
    }

    /// Tampered capsule produces capsule_integrity_mismatch error
    /// without touching the store.
    #[test]
    fn apply_handoff_capsule_integrity_mismatch_short_circuits_ft_1650n_5() {
        let store = Arc::new(PassportStore::new());
        store.insert(handoff_actor_passport_with(&["accept_passport_excerpt"]));
        let mut server = HeadlessMuxServer::new(ServerConfig::default())
            .with_passport_store(store.clone());

        let mut capsule = build_handoff_capsule(
            vec![CapsuleSection::PassportExcerpt {
                passport: passport_excerpt_to_inherit(1),
            }],
            11,
        );
        // Tamper: replace the passport with a different one; integrity
        // hash now refers to the original.
        capsule.sections[0] = CapsuleSection::PassportExcerpt {
            passport: passport_excerpt_to_inherit(99),
        };

        let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
            actor: PassportKey::pane("actor-agent", 11),
            capsule: Box::new(capsule),
        });
        match resp {
            RemoteResponse::Error { code, .. } => {
                assert_eq!(code, "capsule_integrity_mismatch");
            }
            other => panic!("expected integrity_mismatch error, got {other:?}"),
        }
        // Store untouched.
        assert!(store.get(&PassportKey::pane("inherited-agent", 99)).is_none());
    }

    /// Sections of kind other than PassportExcerpt land in the
    /// deferred bucket — operator-side apply paths handle them.
    #[test]
    fn apply_handoff_capsule_defers_non_excerpt_sections_to_operator_ft_1650n_5() {
        let store = Arc::new(PassportStore::new());
        // Actor has every safety constraint Verified so all guarded
        // sections accept; the capsule mixes a PassportExcerpt with a
        // ContextSummary (unguarded) and a CausalSummary (guarded by
        // inherit_causal_summary).
        store.insert(handoff_actor_passport_with(&[
            "accept_passport_excerpt",
            "inherit_causal_summary",
        ]));
        let mut server = HeadlessMuxServer::new(ServerConfig::default())
            .with_passport_store(store);

        let capsule = build_handoff_capsule(
            vec![
                CapsuleSection::ContextSummary {
                    text: "ctx".into(),
                },
                CapsuleSection::PassportExcerpt {
                    passport: passport_excerpt_to_inherit(1),
                },
                CapsuleSection::CausalSummary {
                    claim_ids: vec!["c1".into()],
                },
            ],
            11,
        );

        let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
            actor: PassportKey::pane("actor-agent", 11),
            capsule: Box::new(capsule),
        });
        let RemoteResponse::HandoffApplied { summary } = resp else {
            panic!("expected HandoffApplied");
        };
        // All 3 accepted.
        assert_eq!(summary.accepted_indices, vec![0, 1, 2]);
        // Only the PassportExcerpt was actually applied; the other 2
        // (ContextSummary + CausalSummary) land in deferred bucket.
        assert_eq!(summary.passport_excerpt_applies.len(), 1);
        assert_eq!(summary.deferred_to_operator_count, 2);
    }
}
