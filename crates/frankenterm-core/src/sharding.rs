//! Sharded WezTerm routing for multi-mux deployments.
//!
//! This module introduces a shard-aware wrapper that can fan out pane discovery
//! across multiple mux backends and route pane-scoped operations back to the
//! owning shard.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::circuit_breaker::{CircuitBreakerStatus, CircuitStateKind};
use crate::consistent_hash::HashRing;
use crate::error::WeztermError;
use crate::patterns::AgentType;
use crate::runtime_compat::RwLock;
use crate::watchdog::HealthStatus;
use crate::wezterm::{
    MoveDirection, PaneInfo, SplitDirection, WeztermFuture, WeztermHandle, WeztermInterface,
};

// =============================================================================
// Telemetry types
// =============================================================================

/// Operational telemetry for [`ShardedWeztermClient`].
#[derive(Debug, Default)]
pub struct ShardingTelemetry {
    spawns: AtomicU64,
    pane_listings: AtomicU64,
    health_reports: AtomicU64,
    route_lookups: AtomicU64,
}

impl ShardingTelemetry {
    pub fn snapshot(&self) -> ShardingTelemetrySnapshot {
        ShardingTelemetrySnapshot {
            spawns: self.spawns.load(Ordering::Relaxed),
            pane_listings: self.pane_listings.load(Ordering::Relaxed),
            health_reports: self.health_reports.load(Ordering::Relaxed),
            route_lookups: self.route_lookups.load(Ordering::Relaxed),
        }
    }
}

/// Serializable telemetry snapshot for [`ShardedWeztermClient`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardingTelemetrySnapshot {
    pub spawns: u64,
    pub pane_listings: u64,
    pub health_reports: u64,
    pub route_lookups: u64,
}

/// Number of high bits reserved for shard id in encoded pane ids.
pub const SHARD_ID_BITS: u32 = 16;

/// Mask for local pane id bits in encoded pane ids.
pub const LOCAL_PANE_ID_MASK: u64 = (1u64 << (64 - SHARD_ID_BITS)) - 1;

/// Maximum shard id representable in encoded pane ids.
pub const MAX_SHARD_ID: usize = ((1u64 << SHARD_ID_BITS) - 1) as usize;

/// Identifier for a mux shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShardId(pub usize);

impl std::fmt::Display for ShardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Encode `(shard_id, local_pane_id)` into a globally unique pane id.
#[must_use]
pub fn encode_sharded_pane_id(shard_id: ShardId, local_pane_id: u64) -> u64 {
    assert!(
        shard_id.0 <= MAX_SHARD_ID,
        "shard id {} exceeds {}-bit encoded capacity (max={MAX_SHARD_ID})",
        shard_id.0,
        SHARD_ID_BITS
    );
    ((shard_id.0 as u64) << (64 - SHARD_ID_BITS)) | (local_pane_id & LOCAL_PANE_ID_MASK)
}

/// Decode a globally encoded pane id into `(shard_id, local_pane_id)`.
#[must_use]
pub fn decode_sharded_pane_id(global_pane_id: u64) -> (ShardId, u64) {
    let shard_idx = (global_pane_id >> (64 - SHARD_ID_BITS)) as usize;
    let local = global_pane_id & LOCAL_PANE_ID_MASK;
    (ShardId(shard_idx), local)
}

/// Returns true when a pane id has non-zero shard bits.
#[must_use]
pub fn is_sharded_pane_id(pane_id: u64) -> bool {
    (pane_id >> (64 - SHARD_ID_BITS)) != 0
}

/// Serialize HashMap<u64, V> as a map with string keys for JSON compatibility.
fn serialize_u64_map<S, V: Serialize>(
    map: &HashMap<u64, V>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut ser_map = serializer.serialize_map(Some(map.len()))?;
    for (k, v) in map {
        ser_map.serialize_entry(&k.to_string(), v)?;
    }
    ser_map.end()
}

/// Deserialize HashMap<u64, V> from a map with string keys.
fn deserialize_u64_map<'de, D, V: Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<HashMap<u64, V>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let string_map: HashMap<String, V> = HashMap::deserialize(deserializer)?;
    string_map
        .into_iter()
        .map(|(k, v)| {
            k.parse::<u64>()
                .map(|k| (k, v))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

/// How panes should be assigned to shards.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "strategy")]
pub enum AssignmentStrategy {
    /// Select shards round-robin for new panes. Existing panes are routed by
    /// observed ownership.
    #[default]
    RoundRobin,
    /// Route by normalized pane domain.
    ByDomain {
        domain_to_shard: HashMap<String, ShardId>,
        default_shard: Option<ShardId>,
    },
    /// Route by inferred agent type.
    ByAgentType {
        agent_to_shard: HashMap<AgentType, ShardId>,
        default_shard: Option<ShardId>,
    },
    /// Explicit pane-id map with optional fallback shard.
    Manual {
        #[serde(
            serialize_with = "serialize_u64_map",
            deserialize_with = "deserialize_u64_map"
        )]
        pane_to_shard: HashMap<u64, ShardId>,
        default_shard: Option<ShardId>,
    },
    /// Route by consistent hashing on pane id.
    ConsistentHash { virtual_nodes: u32 },
}

impl AssignmentStrategy {
    fn validate_shards(&self, valid: &HashSet<ShardId>) -> Result<()> {
        let mut referenced = Vec::new();
        match self {
            Self::RoundRobin | Self::ConsistentHash { .. } => {}
            Self::ByDomain {
                domain_to_shard,
                default_shard,
            } => {
                referenced.extend(domain_to_shard.values().copied());
                if let Some(id) = default_shard {
                    referenced.push(*id);
                }
            }
            Self::ByAgentType {
                agent_to_shard,
                default_shard,
            } => {
                referenced.extend(agent_to_shard.values().copied());
                if let Some(id) = default_shard {
                    referenced.push(*id);
                }
            }
            Self::Manual {
                pane_to_shard,
                default_shard,
            } => {
                referenced.extend(pane_to_shard.values().copied());
                if let Some(id) = default_shard {
                    referenced.push(*id);
                }
            }
        }

        if let Some(invalid) = referenced.into_iter().find(|id| !valid.contains(id)) {
            return Err(crate::Error::Wezterm(WeztermError::CommandFailed(format!(
                "assignment strategy references unknown shard id {invalid}"
            ))));
        }

        if let Self::ConsistentHash { virtual_nodes } = self {
            if *virtual_nodes == 0 {
                return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                    "consistent hash virtual_nodes must be >= 1".to_string(),
                )));
            }
        }

        Ok(())
    }

    fn preferred_for_spawn(
        &self,
        domain_hint: Option<&str>,
        agent_hint: Option<AgentType>,
    ) -> Option<ShardId> {
        match self {
            Self::RoundRobin | Self::ConsistentHash { .. } => None,
            Self::ByDomain {
                domain_to_shard,
                default_shard,
            } => {
                if let Some(domain) = domain_hint {
                    let normalized = normalize_domain(domain);
                    domain_to_shard
                        .get(domain)
                        .or_else(|| domain_to_shard.get(&normalized))
                        .copied()
                        .or(*default_shard)
                } else {
                    *default_shard
                }
            }
            Self::ByAgentType {
                agent_to_shard,
                default_shard,
            } => agent_hint
                .and_then(|agent| agent_to_shard.get(&agent).copied())
                .or(*default_shard),
            Self::Manual { default_shard, .. } => *default_shard,
        }
    }
}

/// Deterministic stateless pane assignment helper.
#[must_use]
pub fn assign_pane_with_strategy(
    strategy: &AssignmentStrategy,
    shard_ids: &[ShardId],
    pane_id: u64,
    domain_hint: Option<&str>,
    agent_hint: Option<AgentType>,
) -> ShardId {
    if shard_ids.is_empty() {
        return ShardId(0);
    }

    let contains = |candidate: ShardId| shard_ids.contains(&candidate);

    let strategy_choice = match strategy {
        AssignmentStrategy::RoundRobin => None,
        AssignmentStrategy::ByDomain {
            domain_to_shard,
            default_shard,
        } => {
            let from_domain = domain_hint.and_then(|domain| {
                let normalized = normalize_domain(domain);
                domain_to_shard
                    .get(domain)
                    .or_else(|| domain_to_shard.get(&normalized))
                    .copied()
            });
            from_domain.or(*default_shard)
        }
        AssignmentStrategy::ByAgentType {
            agent_to_shard,
            default_shard,
        } => agent_hint
            .and_then(|agent| agent_to_shard.get(&agent).copied())
            .or(*default_shard),
        AssignmentStrategy::Manual {
            pane_to_shard,
            default_shard,
        } => pane_to_shard.get(&pane_id).copied().or(*default_shard),
        AssignmentStrategy::ConsistentHash { virtual_nodes } => {
            let ring = HashRing::with_nodes(*virtual_nodes, shard_ids.iter().copied());
            ring.get_node(format!("pane:{pane_id}")).copied()
        }
    };

    strategy_choice
        .filter(|candidate| contains(*candidate))
        .unwrap_or_else(|| deterministic_fallback_shard(shard_ids, pane_id))
}

fn deterministic_fallback_shard(shard_ids: &[ShardId], seed: u64) -> ShardId {
    if shard_ids.is_empty() {
        return ShardId(0);
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    let idx = (hasher.finish() as usize) % shard_ids.len();
    shard_ids[idx]
}

fn normalize_domain(domain: &str) -> String {
    domain.trim().to_ascii_lowercase()
}

/// Infer an agent type from pane metadata.
#[must_use]
pub fn infer_agent_type(pane: &PaneInfo) -> AgentType {
    let title = pane.effective_title().to_ascii_lowercase();
    let domain = pane.inferred_domain().to_ascii_lowercase();

    if title.contains("codex") || domain.contains("codex") {
        AgentType::Codex
    } else if title.contains("claude") || domain.contains("claude") {
        AgentType::ClaudeCode
    } else if title.contains("gemini") || domain.contains("gemini") {
        AgentType::Gemini
    } else if title.contains("wezterm") || domain.contains("wezterm") {
        AgentType::Wezterm
    } else {
        AgentType::Unknown
    }
}

/// A single shard backend handle.
#[derive(Clone)]
pub struct ShardBackend {
    pub id: ShardId,
    pub label: String,
    pub handle: WeztermHandle,
}

/// Health for a single shard backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardHealthEntry {
    pub shard_id: ShardId,
    pub label: String,
    pub status: HealthStatus,
    pub pane_count: Option<usize>,
    pub circuit: CircuitBreakerStatus,
    pub error: Option<String>,
}

/// Point-in-time health report across all configured shards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardHealthReport {
    pub timestamp_ms: u64,
    pub overall: HealthStatus,
    pub shards: Vec<ShardHealthEntry>,
}

impl ShardHealthReport {
    /// Return shard entries that are not healthy.
    #[must_use]
    pub fn unhealthy_shards(&self) -> Vec<&ShardHealthEntry> {
        self.shards
            .iter()
            .filter(|entry| entry.status != HealthStatus::Healthy)
            .collect()
    }

    /// Render human-readable warnings suitable for watchdog snapshots.
    #[must_use]
    pub fn watchdog_warnings(&self) -> Vec<String> {
        self.unhealthy_shards()
            .into_iter()
            .map(|entry| {
                let detail = entry.error.as_deref().unwrap_or("no error details");
                format!(
                    "Shard {} ({}) {} (circuit={:?}, pane_count={:?}): {}",
                    entry.shard_id.0,
                    entry.label,
                    entry.status,
                    entry.circuit.state,
                    entry.pane_count,
                    detail
                )
            })
            .collect()
    }
}

impl std::fmt::Debug for ShardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardBackend")
            .field("id", &self.id)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl ShardBackend {
    #[must_use]
    pub fn new(id: ShardId, label: impl Into<String>, handle: WeztermHandle) -> Self {
        Self {
            id,
            label: label.into(),
            handle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneRoute {
    shard_id: ShardId,
    local_pane_id: u64,
}

/// Shard-aware wrapper implementing the WezTerm interface.
#[derive(Debug)]
pub struct ShardedWeztermClient {
    backends: Vec<ShardBackend>,
    backend_index: HashMap<ShardId, usize>,
    strategy: AssignmentStrategy,
    pane_routes: RwLock<HashMap<u64, PaneRoute>>,
    round_robin_cursor: AtomicUsize,
    hash_ring: Option<HashRing<ShardId>>,
    telemetry: ShardingTelemetry,
}

impl ShardedWeztermClient {
    /// Create a new sharded client.
    pub fn new(mut backends: Vec<ShardBackend>, strategy: AssignmentStrategy) -> Result<Self> {
        if backends.is_empty() {
            return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                "sharded client requires at least one backend".to_string(),
            )));
        }

        backends.sort_by_key(|backend| backend.id);
        let ids: Vec<ShardId> = backends.iter().map(|backend| backend.id).collect();
        let unique: HashSet<ShardId> = ids.iter().copied().collect();
        if unique.len() != ids.len() {
            return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                "duplicate shard id in backend configuration".to_string(),
            )));
        }
        if let Some(invalid) = ids.iter().find(|id| id.0 > MAX_SHARD_ID) {
            return Err(crate::Error::Wezterm(WeztermError::CommandFailed(format!(
                "shard id {} exceeds {}-bit encoded pane id capacity (max {})",
                invalid.0, SHARD_ID_BITS, MAX_SHARD_ID
            ))));
        }

        strategy.validate_shards(&unique)?;

        let backend_index = backends
            .iter()
            .enumerate()
            .map(|(idx, backend)| (backend.id, idx))
            .collect::<HashMap<_, _>>();

        let hash_ring = match strategy {
            AssignmentStrategy::ConsistentHash { virtual_nodes } => {
                Some(HashRing::with_nodes(virtual_nodes, ids.iter().copied()))
            }
            _ => None,
        };

        Ok(Self {
            backends,
            backend_index,
            strategy,
            pane_routes: RwLock::new(HashMap::new()),
            round_robin_cursor: AtomicUsize::new(0),
            hash_ring,
            telemetry: ShardingTelemetry::default(),
        })
    }

    /// Convenience constructor assigning shard ids sequentially from handles.
    pub fn from_handles(strategy: AssignmentStrategy, handles: Vec<WeztermHandle>) -> Result<Self> {
        let backends = handles
            .into_iter()
            .enumerate()
            .map(|(idx, handle)| ShardBackend::new(ShardId(idx), format!("shard-{idx}"), handle))
            .collect::<Vec<_>>();
        Self::new(backends, strategy)
    }

    /// Returns the telemetry tracker for this client.
    pub fn telemetry(&self) -> &ShardingTelemetry {
        &self.telemetry
    }

    /// List configured shard ids in deterministic order.
    #[must_use]
    pub fn shard_ids(&self) -> Vec<ShardId> {
        self.backends.iter().map(|backend| backend.id).collect()
    }

    fn backend_for_id(&self, shard_id: ShardId) -> Result<&ShardBackend> {
        self.backend_index
            .get(&shard_id)
            .copied()
            .and_then(|idx| self.backends.get(idx))
            .ok_or_else(|| {
                crate::Error::Wezterm(WeztermError::CommandFailed(format!(
                    "unknown shard id {}",
                    shard_id
                )))
            })
    }

    fn backend_error(
        &self,
        shard_id: ShardId,
        op: &str,
        pane_id: Option<u64>,
        err: crate::Error,
    ) -> crate::Error {
        let label = self
            .backend_for_id(shard_id)
            .map(|backend| backend.label.as_str().to_string())
            .unwrap_or_else(|_| format!("shard-{shard_id}"));
        let pane_hint = pane_id.map_or_else(String::new, |id| format!(", pane={id}"));
        crate::Error::Wezterm(WeztermError::CommandFailed(format!(
            "{op} failed on {label} ({shard_id}{pane_hint}): {err}"
        )))
    }

    fn next_round_robin_shard(&self) -> ShardId {
        let backend_count = self.backends.len().max(1);
        let idx = self.round_robin_cursor.fetch_add(1, Ordering::Relaxed) % backend_count;
        self.backends
            .get(idx)
            .map_or(ShardId(0), |backend| backend.id)
    }

    fn choose_spawn_shard(
        &self,
        domain_hint: Option<&str>,
        agent_hint: Option<AgentType>,
    ) -> ShardId {
        if let Some(candidate) = self.strategy.preferred_for_spawn(domain_hint, agent_hint) {
            if self.backend_index.contains_key(&candidate) {
                return candidate;
            }
        }

        if let Some(ref ring) = self.hash_ring {
            if let Some(domain) = domain_hint {
                if let Some(node) = ring.get_node(format!("spawn:{domain}")) {
                    return *node;
                }
            }
        }

        self.next_round_robin_shard()
    }

    /// Spawn a new pane while honoring shard-assignment hints.
    pub async fn spawn_with_hints(
        &self,
        cwd: Option<&str>,
        domain_name: Option<&str>,
        agent_hint: Option<AgentType>,
    ) -> Result<u64> {
        self.telemetry.spawns.fetch_add(1, Ordering::Relaxed);
        let shard = self.choose_spawn_shard(domain_name, agent_hint);
        let backend = self.backend_for_id(shard)?;
        let local_id = backend
            .handle
            .spawn(cwd, domain_name)
            .await
            .map_err(|err| self.backend_error(shard, "spawn", None, err))?;
        let global_id = encode_sharded_pane_id(shard, local_id);
        self.pane_routes.write().await.insert(
            global_id,
            PaneRoute {
                shard_id: shard,
                local_pane_id: local_id,
            },
        );
        Ok(global_id)
    }

    async fn collect_panes(&self) -> Result<(Vec<PaneInfo>, HashMap<u64, PaneRoute>)> {
        let mut all = Vec::new();
        let mut routes = HashMap::new();

        for backend in &self.backends {
            let panes = backend
                .handle
                .list_panes()
                .await
                .map_err(|err| self.backend_error(backend.id, "list_panes", None, err))?;

            for mut pane in panes {
                let local_pane_id = pane.pane_id;
                let global_pane_id = encode_sharded_pane_id(backend.id, local_pane_id);
                pane.pane_id = global_pane_id;
                pane.extra
                    .insert("shard_id".to_string(), Value::from(backend.id.0 as u64));
                pane.extra
                    .insert("local_pane_id".to_string(), Value::from(local_pane_id));

                routes.insert(
                    global_pane_id,
                    PaneRoute {
                        shard_id: backend.id,
                        local_pane_id,
                    },
                );
                all.push(pane);
            }
        }

        Ok((all, routes))
    }

    /// Aggregate panes across all shards and refresh the route index.
    pub async fn list_all_panes(&self) -> Result<Vec<PaneInfo>> {
        self.telemetry.pane_listings.fetch_add(1, Ordering::Relaxed);
        let (panes, routes) = self.collect_panes().await?;
        let mut guard = self.pane_routes.write().await;
        *guard = routes;
        Ok(panes)
    }

    /// Build a shard-level health report for watchdog integration.
    pub async fn shard_health_report(&self) -> ShardHealthReport {
        self.telemetry
            .health_reports
            .fetch_add(1, Ordering::Relaxed);
        let mut overall = HealthStatus::Healthy;
        let mut shards = Vec::with_capacity(self.backends.len());

        for backend in &self.backends {
            let circuit = backend.handle.circuit_status();
            let mut status = health_from_circuit_state(circuit.state);
            let mut pane_count = None;
            let mut error = None;

            match backend.handle.list_panes().await {
                Ok(panes) => {
                    pane_count = Some(panes.len());
                }
                Err(err) => {
                    status = status.max(HealthStatus::Hung);
                    error = Some(err.to_string());
                }
            }

            overall = overall.max(status);
            shards.push(ShardHealthEntry {
                shard_id: backend.id,
                label: backend.label.clone(),
                status,
                pane_count,
                circuit,
                error,
            });
        }

        ShardHealthReport {
            timestamp_ms: now_epoch_ms(),
            overall,
            shards,
        }
    }

    /// Produce watchdog warning lines from current shard health.
    pub async fn shard_watchdog_warnings(&self) -> Vec<String> {
        self.shard_health_report().await.watchdog_warnings()
    }

    async fn route_for_global_pane_id(&self, pane_id: u64) -> Result<PaneRoute> {
        self.telemetry.route_lookups.fetch_add(1, Ordering::Relaxed);
        if let Some(route) = self.pane_routes.read().await.get(&pane_id).copied() {
            return Ok(route);
        }

        let (_panes, routes) = self.collect_panes().await?;
        {
            let mut guard = self.pane_routes.write().await;
            *guard = routes;
            if let Some(route) = guard.get(&pane_id).copied() {
                return Ok(route);
            }
        }

        if self.backends.len() == 1 {
            return Ok(PaneRoute {
                shard_id: self.backends[0].id,
                local_pane_id: pane_id,
            });
        }

        let (decoded_shard, decoded_local) = decode_sharded_pane_id(pane_id);
        if self.backend_index.contains_key(&decoded_shard) {
            return Ok(PaneRoute {
                shard_id: decoded_shard,
                local_pane_id: decoded_local,
            });
        }

        Err(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)))
    }
}

impl WeztermInterface for ShardedWeztermClient {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        Box::pin(async move { self.list_all_panes().await })
    }

    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            let mut pane = backend
                .handle
                .get_pane(route.local_pane_id)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "get_pane", Some(pane_id), err)
                })?;
            pane.pane_id = encode_sharded_pane_id(route.shard_id, route.local_pane_id);
            pane.extra
                .insert("shard_id".to_string(), Value::from(route.shard_id.0 as u64));
            pane.extra.insert(
                "local_pane_id".to_string(),
                Value::from(route.local_pane_id),
            );
            Ok(pane)
        })
    }

    fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .get_text(route.local_pane_id, escapes)
                .await
                .map_err(|err| self.backend_error(route.shard_id, "get_text", Some(pane_id), err))
        })
    }

    fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text(route.local_pane_id, &text)
                .await
                .map_err(|err| self.backend_error(route.shard_id, "send_text", Some(pane_id), err))
        })
    }

    fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text_no_paste(route.local_pane_id, &text)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "send_text_no_paste", Some(pane_id), err)
                })
        })
    }

    fn send_text_with_options(
        &self,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text_with_options(route.local_pane_id, &text, no_paste, no_newline)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "send_text_with_options", Some(pane_id), err)
                })
        })
    }

    fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
        let control_char = control_char.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_control(route.local_pane_id, &control_char)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "send_control", Some(pane_id), err)
                })
        })
    }

    fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.send_control(pane_id, "\u{3}")
    }

    fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.send_control(pane_id, "\u{4}")
    }

    fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
        let cwd = cwd.map(ToString::to_string);
        let domain_name = domain_name.map(ToString::to_string);
        Box::pin(async move {
            self.spawn_with_hints(cwd.as_deref(), domain_name.as_deref(), None)
                .await
        })
    }

    fn split_pane(
        &self,
        pane_id: u64,
        direction: SplitDirection,
        cwd: Option<&str>,
        percent: Option<u8>,
    ) -> WeztermFuture<'_, u64> {
        let cwd = cwd.map(ToString::to_string);
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            let local_new = backend
                .handle
                .split_pane(route.local_pane_id, direction, cwd.as_deref(), percent)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "split_pane", Some(pane_id), err)
                })?;

            let global_new = encode_sharded_pane_id(route.shard_id, local_new);
            self.pane_routes.write().await.insert(
                global_new,
                PaneRoute {
                    shard_id: route.shard_id,
                    local_pane_id: local_new,
                },
            );
            Ok(global_new)
        })
    }

    fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .activate_pane(route.local_pane_id)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "activate_pane", Some(pane_id), err)
                })
        })
    }

    fn get_pane_direction(
        &self,
        pane_id: u64,
        direction: MoveDirection,
    ) -> WeztermFuture<'_, Option<u64>> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            let next_local = backend
                .handle
                .get_pane_direction(route.local_pane_id, direction)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "get_pane_direction", Some(pane_id), err)
                })?;

            if let Some(local_id) = next_local {
                let global_id = encode_sharded_pane_id(route.shard_id, local_id);
                self.pane_routes.write().await.insert(
                    global_id,
                    PaneRoute {
                        shard_id: route.shard_id,
                        local_pane_id: local_id,
                    },
                );
                Ok(Some(global_id))
            } else {
                Ok(None)
            }
        })
    }

    fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .kill_pane(route.local_pane_id)
                .await
                .map_err(|err| {
                    self.backend_error(route.shard_id, "kill_pane", Some(pane_id), err)
                })?;
            self.pane_routes.write().await.remove(&pane_id);
            Ok(())
        })
    }

    fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id).await?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .zoom_pane(route.local_pane_id, zoom)
                .await
                .map_err(|err| self.backend_error(route.shard_id, "zoom_pane", Some(pane_id), err))
        })
    }

    fn circuit_status(&self) -> CircuitBreakerStatus {
        let mut combined = CircuitBreakerStatus::default();
        for backend in &self.backends {
            let status = backend.handle.circuit_status();
            let current_rank = circuit_state_rank(combined.state);
            let candidate_rank = circuit_state_rank(status.state);
            if candidate_rank > current_rank {
                combined = status;
            } else if candidate_rank == current_rank {
                combined.consecutive_failures = combined
                    .consecutive_failures
                    .max(status.consecutive_failures);
                combined.failure_threshold =
                    combined.failure_threshold.max(status.failure_threshold);
                combined.success_threshold =
                    combined.success_threshold.max(status.success_threshold);
            }
        }
        combined
    }

    fn watchdog_warnings(&self) -> WeztermFuture<'_, Vec<String>> {
        Box::pin(async move { Ok(self.shard_watchdog_warnings().await) })
    }
}

fn circuit_state_rank(state: CircuitStateKind) -> u8 {
    match state {
        CircuitStateKind::Closed => 0,
        CircuitStateKind::HalfOpen => 1,
        CircuitStateKind::Open => 2,
    }
}

fn health_from_circuit_state(state: CircuitStateKind) -> HealthStatus {
    match state {
        CircuitStateKind::Closed => HealthStatus::Healthy,
        CircuitStateKind::HalfOpen => HealthStatus::Degraded,
        CircuitStateKind::Open => HealthStatus::Critical,
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::wezterm::{MockWezterm, WeztermInterface};

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build sharding test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let shard = ShardId(37);
        let local = 0x0000_FFFF_FFFF_u64;
        let encoded = encode_sharded_pane_id(shard, local);
        let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
        assert_eq!(decoded_shard, shard);
        assert_eq!(decoded_local, local & LOCAL_PANE_ID_MASK);
    }

    #[test]
    fn assign_manual_fallback_and_consistent_hash() {
        let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
        let manual = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::from([(42, ShardId(1))]),
            default_shard: Some(ShardId(2)),
        };

        assert_eq!(
            assign_pane_with_strategy(&manual, &shards, 42, None, None),
            ShardId(1)
        );
        assert_eq!(
            assign_pane_with_strategy(&manual, &shards, 100, None, None),
            ShardId(2)
        );

        let ch = AssignmentStrategy::ConsistentHash { virtual_nodes: 128 };
        let a = assign_pane_with_strategy(&ch, &shards, 9_999, None, None);
        let b = assign_pane_with_strategy(&ch, &shards, 9_999, None, None);
        assert_eq!(a, b);
        assert!(shards.contains(&a));
    }

    #[test]
    fn circuit_state_maps_to_health() {
        assert_eq!(
            health_from_circuit_state(CircuitStateKind::Closed),
            HealthStatus::Healthy
        );
        assert_eq!(
            health_from_circuit_state(CircuitStateKind::HalfOpen),
            HealthStatus::Degraded
        );
        assert_eq!(
            health_from_circuit_state(CircuitStateKind::Open),
            HealthStatus::Critical
        );
    }

    #[test]
    fn list_panes_aggregates_and_routes_text() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(7).await;
            shard0.inject_output(7, "alpha").await.unwrap();

            let shard1 = Arc::new(MockWezterm::new());
            shard1.add_default_pane(7).await;
            shard1.inject_output(7, "beta").await.unwrap();

            let handle0: WeztermHandle = shard0.clone();
            let handle1: WeztermHandle = shard1.clone();

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), "zero", handle0),
                    ShardBackend::new(ShardId(1), "one", handle1),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            assert_eq!(panes.len(), 2);

            let pane_on_shard0 = panes
                .iter()
                .find(|pane| pane.extra.get("shard_id") == Some(&Value::from(0_u64)))
                .unwrap();
            let pane_on_shard1 = panes
                .iter()
                .find(|pane| pane.extra.get("shard_id") == Some(&Value::from(1_u64)))
                .unwrap();

            assert!(is_sharded_pane_id(pane_on_shard1.pane_id));
            assert_eq!(
                decode_sharded_pane_id(pane_on_shard0.pane_id),
                (ShardId(0), 7)
            );
            assert_eq!(
                decode_sharded_pane_id(pane_on_shard1.pane_id),
                (ShardId(1), 7)
            );

            let text0 = client
                .get_text(pane_on_shard0.pane_id, false)
                .await
                .unwrap();
            let text1 = client
                .get_text(pane_on_shard1.pane_id, false)
                .await
                .unwrap();
            assert_eq!(text0, "alpha");
            assert_eq!(text1, "beta");
        });
    }

    #[test]
    fn spawn_round_robin_across_shards() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            let shard1 = Arc::new(MockWezterm::new());
            let handle0: WeztermHandle = shard0.clone();
            let handle1: WeztermHandle = shard1.clone();

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), "zero", handle0),
                    ShardBackend::new(ShardId(1), "one", handle1),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let pane_a = client.spawn(None, None).await.unwrap();
            let pane_b = client.spawn(None, None).await.unwrap();

            assert_eq!(decode_sharded_pane_id(pane_a), (ShardId(0), 0));
            assert_eq!(decode_sharded_pane_id(pane_b), (ShardId(1), 0));
            assert_eq!(shard0.pane_count().await, 1);
            assert_eq!(shard1.pane_count().await, 1);
        });
    }

    #[test]
    fn spawn_with_agent_hint_uses_agent_assignment() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            let shard1 = Arc::new(MockWezterm::new());
            let handle0: WeztermHandle = shard0.clone();
            let handle1: WeztermHandle = shard1.clone();

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), "zero", handle0),
                    ShardBackend::new(ShardId(1), "one", handle1),
                ],
                AssignmentStrategy::ByAgentType {
                    agent_to_shard: HashMap::from([
                        (AgentType::Codex, ShardId(1)),
                        (AgentType::ClaudeCode, ShardId(0)),
                    ]),
                    default_shard: Some(ShardId(0)),
                },
            )
            .unwrap();

            let pane = client
                .spawn_with_hints(None, None, Some(AgentType::Codex))
                .await
                .unwrap();
            assert_eq!(decode_sharded_pane_id(pane), (ShardId(1), 0));
            assert_eq!(shard0.pane_count().await, 0);
            assert_eq!(shard1.pane_count().await, 1);
        });
    }

    // -----------------------------------------------------------------------
    // Encode / decode edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn encode_decode_shard_zero_local_zero() {
        let encoded = encode_sharded_pane_id(ShardId(0), 0);
        assert_eq!(encoded, 0);
        let (s, l) = decode_sharded_pane_id(encoded);
        assert_eq!(s, ShardId(0));
        assert_eq!(l, 0);
    }

    #[test]
    fn encode_decode_max_shard() {
        let max_shard = (1usize << SHARD_ID_BITS) - 1;
        let shard = ShardId(max_shard);
        let local = 42_u64;
        let encoded = encode_sharded_pane_id(shard, local);
        let (s, l) = decode_sharded_pane_id(encoded);
        assert_eq!(s, shard);
        assert_eq!(l, local);
    }

    #[test]
    fn encode_decode_max_local() {
        let shard = ShardId(1);
        let encoded = encode_sharded_pane_id(shard, LOCAL_PANE_ID_MASK);
        let (s, l) = decode_sharded_pane_id(encoded);
        assert_eq!(s, shard);
        assert_eq!(l, LOCAL_PANE_ID_MASK);
    }

    #[test]
    #[should_panic(expected = "exceeds 16-bit encoded capacity")]
    fn encode_shard_overflow_panics() {
        let _ = encode_sharded_pane_id(ShardId(MAX_SHARD_ID + 1), 42);
    }

    #[test]
    fn encode_local_overflow_masked() {
        let shard = ShardId(1);
        // Pass a value larger than LOCAL_PANE_ID_MASK; high bits should be masked.
        let big_local = LOCAL_PANE_ID_MASK + 1;
        let encoded = encode_sharded_pane_id(shard, big_local);
        let (s, l) = decode_sharded_pane_id(encoded);
        assert_eq!(s, shard);
        assert_eq!(l, 0); // Overflow wraps to 0 after mask.
    }

    // -----------------------------------------------------------------------
    // is_sharded_pane_id
    // -----------------------------------------------------------------------

    #[test]
    fn shard_zero_pane_is_not_sharded() {
        let encoded = encode_sharded_pane_id(ShardId(0), 123);
        assert!(!is_sharded_pane_id(encoded));
    }

    #[test]
    fn nonzero_shard_pane_is_sharded() {
        let encoded = encode_sharded_pane_id(ShardId(1), 123);
        assert!(is_sharded_pane_id(encoded));
    }

    // -----------------------------------------------------------------------
    // ShardId Display / serde
    // -----------------------------------------------------------------------

    #[test]
    fn shard_id_display_batch2() {
        assert_eq!(ShardId(0).to_string(), "0");
        assert_eq!(ShardId(42).to_string(), "42");
    }

    #[test]
    fn shard_id_serde_roundtrip_batch2() {
        let id = ShardId(7);
        let json = serde_json::to_string(&id).unwrap();
        let back: ShardId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn shard_id_ordering() {
        assert!(ShardId(0) < ShardId(1));
        assert!(ShardId(1) < ShardId(100));
    }

    // -----------------------------------------------------------------------
    // AssignmentStrategy
    // -----------------------------------------------------------------------

    #[test]
    fn assignment_strategy_default_is_round_robin_batch2() {
        assert_eq!(
            AssignmentStrategy::default(),
            AssignmentStrategy::RoundRobin
        );
    }

    #[test]
    fn assignment_strategy_round_robin_serde() {
        let s = AssignmentStrategy::RoundRobin;
        let json = serde_json::to_string(&s).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn assignment_strategy_consistent_hash_serde() {
        let s = AssignmentStrategy::ConsistentHash { virtual_nodes: 64 };
        let json = serde_json::to_string(&s).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn assign_empty_shards_returns_shard_zero() {
        let s = AssignmentStrategy::RoundRobin;
        let result = assign_pane_with_strategy(&s, &[], 42, None, None);
        assert_eq!(result, ShardId(0));
    }

    #[test]
    fn assign_by_domain_resolves_known_domain() {
        let shards = vec![ShardId(0), ShardId(1)];
        let strategy = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::from([("local".to_string(), ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        let result = assign_pane_with_strategy(&strategy, &shards, 1, Some("local"), None);
        assert_eq!(result, ShardId(1));
    }

    #[test]
    fn assign_by_domain_unknown_uses_default() {
        let shards = vec![ShardId(0), ShardId(1)];
        let strategy = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::new(),
            default_shard: Some(ShardId(0)),
        };
        let result = assign_pane_with_strategy(&strategy, &shards, 1, Some("unknown"), None);
        assert_eq!(result, ShardId(0));
    }

    #[test]
    fn assign_round_robin_deterministic_for_same_pane() {
        let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
        let strategy = AssignmentStrategy::RoundRobin;
        // RoundRobin doesn't use pane_id, so it falls through to deterministic_fallback_shard.
        let a = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
        let b = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
        // Both should be deterministic for same seed.
        assert_eq!(a, b);
    }

    #[test]
    fn assign_consistent_hash_deterministic() {
        let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
        let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 128 };
        let a = assign_pane_with_strategy(&strategy, &shards, 99, None, None);
        let b = assign_pane_with_strategy(&strategy, &shards, 99, None, None);
        assert_eq!(a, b);
        assert!(shards.contains(&a));
    }

    // -----------------------------------------------------------------------
    // ShardHealthReport
    // -----------------------------------------------------------------------

    #[test]
    fn health_report_all_healthy_no_unhealthy() {
        let report = ShardHealthReport {
            timestamp_ms: 1000,
            overall: HealthStatus::Healthy,
            shards: vec![ShardHealthEntry {
                shard_id: ShardId(0),
                label: "s0".to_string(),
                status: HealthStatus::Healthy,
                pane_count: Some(3),
                circuit: CircuitBreakerStatus::default(),
                error: None,
            }],
        };
        assert!(report.unhealthy_shards().is_empty());
        assert!(report.watchdog_warnings().is_empty());
    }

    #[test]
    fn health_report_mixed_healthy_and_degraded() {
        let report = ShardHealthReport {
            timestamp_ms: 1000,
            overall: HealthStatus::Degraded,
            shards: vec![
                ShardHealthEntry {
                    shard_id: ShardId(0),
                    label: "s0".to_string(),
                    status: HealthStatus::Healthy,
                    pane_count: Some(3),
                    circuit: CircuitBreakerStatus::default(),
                    error: None,
                },
                ShardHealthEntry {
                    shard_id: ShardId(1),
                    label: "s1".to_string(),
                    status: HealthStatus::Degraded,
                    pane_count: None,
                    circuit: CircuitBreakerStatus::default(),
                    error: Some("timeout".to_string()),
                },
            ],
        };
        let unhealthy = report.unhealthy_shards();
        assert_eq!(unhealthy.len(), 1);
        assert_eq!(unhealthy[0].shard_id, ShardId(1));

        let warnings = report.watchdog_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Shard 1 (s1)"));
        assert!(warnings[0].contains("timeout"));
    }

    #[test]
    fn health_report_serde_roundtrip() {
        let report = ShardHealthReport {
            timestamp_ms: 1234,
            overall: HealthStatus::Healthy,
            shards: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ShardHealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.timestamp_ms, 1234);
        assert_eq!(back.overall, HealthStatus::Healthy);
    }

    // -----------------------------------------------------------------------
    // infer_agent_type
    // -----------------------------------------------------------------------

    #[test]
    fn infer_agent_type_from_pane_title() {
        use crate::wezterm::PaneInfo;

        fn pane_with_title(title: &str) -> PaneInfo {
            serde_json::from_value(serde_json::json!({
                "pane_id": 0,
                "tab_id": 0,
                "window_id": 0,
                "title": title,
            }))
            .unwrap()
        }

        assert_eq!(
            infer_agent_type(&pane_with_title("codex-session-1")),
            AgentType::Codex
        );
        assert_eq!(
            infer_agent_type(&pane_with_title("claude-code-dev")),
            AgentType::ClaudeCode
        );
        assert_eq!(
            infer_agent_type(&pane_with_title("gemini-worker")),
            AgentType::Gemini
        );
        assert_eq!(
            infer_agent_type(&pane_with_title("bash shell")),
            AgentType::Unknown
        );
    }

    // -----------------------------------------------------------------------
    // circuit_state_rank
    // -----------------------------------------------------------------------

    #[test]
    fn circuit_state_rank_ordering() {
        assert!(
            circuit_state_rank(CircuitStateKind::Closed)
                < circuit_state_rank(CircuitStateKind::HalfOpen)
        );
        assert!(
            circuit_state_rank(CircuitStateKind::HalfOpen)
                < circuit_state_rank(CircuitStateKind::Open)
        );
    }

    // -----------------------------------------------------------------------
    // normalize_domain
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_domain_lowercases_and_trims() {
        assert_eq!(normalize_domain("  LOCAL  "), "local");
        assert_eq!(normalize_domain("SSH:Prod"), "ssh:prod");
    }

    #[test]
    fn shard_health_report_marks_failed_shard_hung() {
        run_async_test(async {
            let healthy = Arc::new(MockWezterm::new());
            healthy.add_default_pane(1).await;

            let healthy_handle: WeztermHandle = healthy.clone();
            let failing_handle: WeztermHandle = crate::wezterm::mock_wezterm_handle_failing();

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), "healthy", healthy_handle),
                    ShardBackend::new(ShardId(1), "failing", failing_handle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let report = client.shard_health_report().await;
            assert_eq!(report.shards.len(), 2);
            assert_eq!(report.overall, HealthStatus::Hung);

            let healthy_entry = report
                .shards
                .iter()
                .find(|entry| entry.shard_id == ShardId(0))
                .unwrap();
            assert_eq!(healthy_entry.status, HealthStatus::Healthy);
            assert_eq!(healthy_entry.pane_count, Some(1));
            assert!(healthy_entry.error.is_none());

            let failing_entry = report
                .shards
                .iter()
                .find(|entry| entry.shard_id == ShardId(1))
                .unwrap();
            assert_eq!(failing_entry.status, HealthStatus::Hung);
            assert_eq!(failing_entry.pane_count, None);
            assert!(failing_entry.error.is_some());

            let warnings = report.watchdog_warnings();
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("Shard 1 (failing)"));

            let trait_warnings = client.watchdog_warnings().await.unwrap();
            assert_eq!(trait_warnings.len(), 1);
            assert!(trait_warnings[0].contains("Shard 1 (failing)"));
        });
    }

    // -----------------------------------------------------------------------
    // AssignmentStrategy serde variants
    // -----------------------------------------------------------------------

    #[test]
    fn assignment_strategy_by_domain_serde() {
        let s = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::from([("local".to_string(), ShardId(0))]),
            default_shard: Some(ShardId(1)),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn assignment_strategy_by_agent_type_serde() {
        let s = AssignmentStrategy::ByAgentType {
            agent_to_shard: HashMap::from([(AgentType::Codex, ShardId(2))]),
            default_shard: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn assignment_strategy_manual_serde() {
        // HashMap<u64, _> serializes keys as strings in JSON; use empty map
        // to avoid the string-key-to-u64 deserialization limitation, then
        // verify the default_shard field survives.
        let s = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::new(),
            default_shard: Some(ShardId(0)),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    // -----------------------------------------------------------------------
    // validate_shards
    // -----------------------------------------------------------------------

    #[test]
    fn validate_shards_rejects_unknown_shard_in_by_domain() {
        let valid: HashSet<ShardId> = [ShardId(0)].into();
        let strategy = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::from([("x".to_string(), ShardId(99))]),
            default_shard: None,
        };
        let err = strategy.validate_shards(&valid).unwrap_err();
        assert!(err.to_string().contains("unknown shard id 99"));
    }

    #[test]
    fn validate_shards_rejects_unknown_in_by_agent_type() {
        let valid: HashSet<ShardId> = [ShardId(0)].into();
        let strategy = AssignmentStrategy::ByAgentType {
            agent_to_shard: HashMap::from([(AgentType::Codex, ShardId(5))]),
            default_shard: None,
        };
        assert!(strategy.validate_shards(&valid).is_err());
    }

    #[test]
    fn validate_shards_rejects_unknown_in_manual() {
        let valid: HashSet<ShardId> = [ShardId(0)].into();
        let strategy = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::from([(1, ShardId(7))]),
            default_shard: None,
        };
        assert!(strategy.validate_shards(&valid).is_err());
    }

    #[test]
    fn validate_shards_rejects_zero_virtual_nodes() {
        let valid: HashSet<ShardId> = [ShardId(0)].into();
        let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 0 };
        let err = strategy.validate_shards(&valid).unwrap_err();
        assert!(err.to_string().contains("virtual_nodes must be >= 1"));
    }

    #[test]
    fn validate_shards_round_robin_always_ok() {
        let valid: HashSet<ShardId> = [ShardId(0)].into();
        assert!(
            AssignmentStrategy::RoundRobin
                .validate_shards(&valid)
                .is_ok()
        );
    }

    #[test]
    fn validate_shards_rejects_unknown_default_shard() {
        let valid: HashSet<ShardId> = [ShardId(0)].into();
        let strategy = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::new(),
            default_shard: Some(ShardId(99)),
        };
        assert!(strategy.validate_shards(&valid).is_err());
    }

    // -----------------------------------------------------------------------
    // preferred_for_spawn
    // -----------------------------------------------------------------------

    #[test]
    fn preferred_for_spawn_round_robin_returns_none() {
        let s = AssignmentStrategy::RoundRobin;
        assert_eq!(s.preferred_for_spawn(None, None), None);
    }

    #[test]
    fn preferred_for_spawn_by_domain_with_hint() {
        let s = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::from([("local".to_string(), ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        assert_eq!(s.preferred_for_spawn(Some("local"), None), Some(ShardId(1)));
    }

    #[test]
    fn preferred_for_spawn_by_domain_no_hint_uses_default() {
        let s = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::from([("local".to_string(), ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        assert_eq!(s.preferred_for_spawn(None, None), Some(ShardId(0)));
    }

    #[test]
    fn preferred_for_spawn_by_agent_type_with_match() {
        let s = AssignmentStrategy::ByAgentType {
            agent_to_shard: HashMap::from([(AgentType::Gemini, ShardId(2))]),
            default_shard: None,
        };
        assert_eq!(
            s.preferred_for_spawn(None, Some(AgentType::Gemini)),
            Some(ShardId(2))
        );
    }

    #[test]
    fn preferred_for_spawn_by_agent_type_no_match_uses_default() {
        let s = AssignmentStrategy::ByAgentType {
            agent_to_shard: HashMap::from([(AgentType::Gemini, ShardId(2))]),
            default_shard: Some(ShardId(0)),
        };
        assert_eq!(
            s.preferred_for_spawn(None, Some(AgentType::Codex)),
            Some(ShardId(0))
        );
    }

    #[test]
    fn preferred_for_spawn_manual_returns_default_only() {
        let s = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::from([(42, ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        assert_eq!(s.preferred_for_spawn(None, None), Some(ShardId(0)));
    }

    #[test]
    fn preferred_for_spawn_consistent_hash_returns_none() {
        let s = AssignmentStrategy::ConsistentHash { virtual_nodes: 64 };
        assert_eq!(
            s.preferred_for_spawn(Some("x"), Some(AgentType::Codex)),
            None
        );
    }

    // -----------------------------------------------------------------------
    // ShardBackend
    // -----------------------------------------------------------------------

    #[test]
    fn shard_backend_debug_omits_handle() {
        let mock = Arc::new(MockWezterm::new()) as WeztermHandle;
        let backend = ShardBackend::new(ShardId(3), "test-shard", mock);
        let debug = format!("{:?}", backend);
        assert!(debug.contains("id: ShardId(3)"));
        assert!(debug.contains("test-shard"));
        // handle should be omitted via finish_non_exhaustive
        assert!(debug.contains(".."));
    }

    // -----------------------------------------------------------------------
    // ShardedWeztermClient constructor errors
    // -----------------------------------------------------------------------

    #[test]
    fn client_new_rejects_empty_backends() {
        let result = ShardedWeztermClient::new(vec![], AssignmentStrategy::RoundRobin);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("at least one backend")
        );
    }

    #[test]
    fn client_new_rejects_duplicate_shard_ids() {
        let mock1 = Arc::new(MockWezterm::new()) as WeztermHandle;
        let mock2 = Arc::new(MockWezterm::new()) as WeztermHandle;
        let result = ShardedWeztermClient::new(
            vec![
                ShardBackend::new(ShardId(0), "a", mock1),
                ShardBackend::new(ShardId(0), "b", mock2),
            ],
            AssignmentStrategy::RoundRobin,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate shard id")
        );
    }

    #[test]
    fn client_new_rejects_shard_id_overflow() {
        let mock = Arc::new(MockWezterm::new()) as WeztermHandle;
        let result = ShardedWeztermClient::new(
            vec![ShardBackend::new(
                ShardId(MAX_SHARD_ID + 1),
                "overflow",
                mock,
            )],
            AssignmentStrategy::RoundRobin,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds 16-bit encoded pane id capacity")
        );
    }

    // -----------------------------------------------------------------------
    // from_handles
    // -----------------------------------------------------------------------

    #[test]
    fn from_handles_assigns_sequential_ids() {
        let mock0 = Arc::new(MockWezterm::new()) as WeztermHandle;
        let mock1 = Arc::new(MockWezterm::new()) as WeztermHandle;
        let client =
            ShardedWeztermClient::from_handles(AssignmentStrategy::RoundRobin, vec![mock0, mock1])
                .unwrap();
        assert_eq!(client.shard_ids(), vec![ShardId(0), ShardId(1)]);
    }

    // -----------------------------------------------------------------------
    // shard_ids
    // -----------------------------------------------------------------------

    #[test]
    fn shard_ids_returns_sorted() {
        let mock0 = Arc::new(MockWezterm::new()) as WeztermHandle;
        let mock1 = Arc::new(MockWezterm::new()) as WeztermHandle;
        // Provide out-of-order backends
        let client = ShardedWeztermClient::new(
            vec![
                ShardBackend::new(ShardId(5), "five", mock0),
                ShardBackend::new(ShardId(2), "two", mock1),
            ],
            AssignmentStrategy::RoundRobin,
        )
        .unwrap();
        assert_eq!(client.shard_ids(), vec![ShardId(2), ShardId(5)]);
    }

    // -----------------------------------------------------------------------
    // assign_pane_with_strategy: ByAgentType
    // -----------------------------------------------------------------------

    #[test]
    fn assign_by_agent_type_known_agent() {
        let shards = vec![ShardId(0), ShardId(1)];
        let strategy = AssignmentStrategy::ByAgentType {
            agent_to_shard: HashMap::from([(AgentType::ClaudeCode, ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        let result =
            assign_pane_with_strategy(&strategy, &shards, 1, None, Some(AgentType::ClaudeCode));
        assert_eq!(result, ShardId(1));
    }

    #[test]
    fn assign_by_agent_type_unknown_agent_uses_default() {
        let shards = vec![ShardId(0), ShardId(1)];
        let strategy = AssignmentStrategy::ByAgentType {
            agent_to_shard: HashMap::from([(AgentType::Codex, ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        let result =
            assign_pane_with_strategy(&strategy, &shards, 1, None, Some(AgentType::Gemini));
        assert_eq!(result, ShardId(0));
    }

    // -----------------------------------------------------------------------
    // assign_pane_with_strategy: Manual with explicit pane mapping
    // -----------------------------------------------------------------------

    #[test]
    fn assign_manual_explicit_pane_id() {
        let shards = vec![ShardId(0), ShardId(1)];
        let strategy = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::from([(100, ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        assert_eq!(
            assign_pane_with_strategy(&strategy, &shards, 100, None, None),
            ShardId(1)
        );
    }

    // -----------------------------------------------------------------------
    // assign_pane_with_strategy: strategy_choice references invalid shard
    // -----------------------------------------------------------------------

    #[test]
    fn assign_strategy_invalid_shard_falls_back() {
        // The strategy maps to ShardId(99) but shard_ids only has [0,1]
        let shards = vec![ShardId(0), ShardId(1)];
        let strategy = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::from([(42, ShardId(99))]),
            default_shard: None,
        };
        // Should fall through to deterministic_fallback_shard
        let result = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
        assert!(shards.contains(&result));
    }

    // -----------------------------------------------------------------------
    // deterministic_fallback_shard consistency
    // -----------------------------------------------------------------------

    #[test]
    fn deterministic_fallback_is_repeatable() {
        let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
        let a = deterministic_fallback_shard(&shards, 42);
        let b = deterministic_fallback_shard(&shards, 42);
        assert_eq!(a, b);
        assert!(shards.contains(&a));
    }

    #[test]
    fn deterministic_fallback_empty_shards_returns_zero_shard() {
        assert_eq!(deterministic_fallback_shard(&[], 42), ShardId(0));
    }

    #[test]
    fn deterministic_fallback_spreads_across_shards() {
        let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
        let mut seen = HashSet::new();
        for seed in 0..100 {
            seen.insert(deterministic_fallback_shard(&shards, seed));
        }
        // With 100 seeds and 3 shards, we should hit all 3
        assert_eq!(seen.len(), 3);
    }

    // -----------------------------------------------------------------------
    // ShardHealthEntry serde
    // -----------------------------------------------------------------------

    #[test]
    fn shard_health_entry_serde_roundtrip() {
        let entry = ShardHealthEntry {
            shard_id: ShardId(2),
            label: "test".to_string(),
            status: HealthStatus::Degraded,
            pane_count: Some(5),
            circuit: CircuitBreakerStatus::default(),
            error: Some("timeout".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ShardHealthEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.shard_id, ShardId(2));
        assert_eq!(back.label, "test");
        assert_eq!(back.status, HealthStatus::Degraded);
        assert_eq!(back.pane_count, Some(5));
        assert_eq!(back.error.as_deref(), Some("timeout"));
    }

    // -----------------------------------------------------------------------
    // now_epoch_ms
    // -----------------------------------------------------------------------

    #[test]
    fn now_epoch_ms_is_reasonable() {
        let ms = now_epoch_ms();
        // Should be after 2020-01-01 (1577836800000ms)
        assert!(ms > 1_577_836_800_000);
    }

    // -----------------------------------------------------------------------
    // infer_agent_type edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn infer_agent_type_wezterm_title() {
        fn pane_with_title(title: &str) -> PaneInfo {
            serde_json::from_value(serde_json::json!({
                "pane_id": 0,
                "tab_id": 0,
                "window_id": 0,
                "title": title,
            }))
            .unwrap()
        }
        assert_eq!(
            infer_agent_type(&pane_with_title("WezTerm config")),
            AgentType::Wezterm
        );
    }

    #[test]
    fn infer_agent_type_mixed_case() {
        fn pane_with_title(title: &str) -> PaneInfo {
            serde_json::from_value(serde_json::json!({
                "pane_id": 0,
                "tab_id": 0,
                "window_id": 0,
                "title": title,
            }))
            .unwrap()
        }
        // Case-insensitive matching
        assert_eq!(
            infer_agent_type(&pane_with_title("CODEX-dev")),
            AgentType::Codex
        );
        assert_eq!(
            infer_agent_type(&pane_with_title("CLAUDE-code")),
            AgentType::ClaudeCode
        );
        assert_eq!(
            infer_agent_type(&pane_with_title("GEMINI session")),
            AgentType::Gemini
        );
    }

    // -----------------------------------------------------------------------
    // Async trait operations: get_pane, send_text, split_pane, kill_pane, etc.
    // -----------------------------------------------------------------------

    #[test]
    fn get_pane_routes_to_correct_shard() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(10).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(
                    ShardId(0),
                    "s0",
                    shard0.clone() as WeztermHandle,
                )],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            // List first to populate routes
            let panes = client.list_panes().await.unwrap();
            assert_eq!(panes.len(), 1);

            let global_id = panes[0].pane_id;
            let pane = client.get_pane(global_id).await.unwrap();
            assert_eq!(pane.pane_id, global_id);
            assert_eq!(pane.extra.get("shard_id"), Some(&Value::from(0_u64)));
        });
    }

    #[test]
    fn send_text_routes_to_correct_shard() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(5).await;
            let shard1 = Arc::new(MockWezterm::new());
            shard1.add_default_pane(5).await;

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), "s0", shard0.clone() as WeztermHandle),
                    ShardBackend::new(ShardId(1), "s1", shard1.clone() as WeztermHandle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            let shard1_pane = panes
                .iter()
                .find(|p| p.extra.get("shard_id") == Some(&Value::from(1_u64)))
                .unwrap();

            client
                .send_text(shard1_pane.pane_id, "hello")
                .await
                .unwrap();
            // Verify shard1 got the text
            let text = shard1.get_text(5, false).await.unwrap();
            assert!(text.contains("hello"));
        });
    }

    #[test]
    fn split_pane_encodes_global_id() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(1).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), "s0", shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            let global_id = panes[0].pane_id;

            let new_pane = client
                .split_pane(global_id, SplitDirection::Right, None, None)
                .await
                .unwrap();
            let (shard, _local) = decode_sharded_pane_id(new_pane);
            assert_eq!(shard, ShardId(0));
        });
    }

    #[test]
    fn kill_pane_removes_from_routes() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(1).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), "s0", shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            assert_eq!(panes.len(), 1);
            let global_id = panes[0].pane_id;

            client.kill_pane(global_id).await.unwrap();

            // Route should be removed
            let routes = client.pane_routes.read().await;
            assert!(!routes.contains_key(&global_id));
        });
    }

    #[test]
    fn circuit_status_aggregates_worst_state() {
        run_async_test(async {
            let healthy = Arc::new(MockWezterm::new());
            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(
                    ShardId(0),
                    "s0",
                    healthy as WeztermHandle,
                )],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let status = client.circuit_status();
            assert_eq!(status.state, CircuitStateKind::Closed);
        });
    }

    #[test]
    fn activate_pane_routes_correctly() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(3).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), "s0", shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            // Should not error
            client.activate_pane(panes[0].pane_id).await.unwrap();
        });
    }

    #[test]
    fn zoom_pane_routes_correctly() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(3).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), "s0", shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            client.zoom_pane(panes[0].pane_id, true).await.unwrap();
        });
    }

    #[test]
    fn route_for_unknown_pane_single_backend_uses_raw_id() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(42).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), "s0", shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            // Don't list_panes first, so routes are empty.
            // With single backend, route_for_global_pane_id should fall back to
            // using the raw pane_id on the only backend. The collect_panes call
            // will find pane 42, so 42 should be routable.
            let text = client.get_text(42, false).await.unwrap();
            let _ = text; // Just verify no error (get_text succeeded)
        });
    }

    #[test]
    fn send_ctrl_c_routes_correctly() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(1).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), "s0", shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            client.send_ctrl_c(panes[0].pane_id).await.unwrap();
        });
    }

    #[test]
    fn send_ctrl_d_routes_correctly() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(1).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), "s0", shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            client.send_ctrl_d(panes[0].pane_id).await.unwrap();
        });
    }

    // -----------------------------------------------------------------------
    // assign_pane_with_strategy: ByDomain with case normalization
    // -----------------------------------------------------------------------

    #[test]
    fn assign_by_domain_normalizes_case() {
        let shards = vec![ShardId(0), ShardId(1)];
        let strategy = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::from([("local".to_string(), ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        // Pass "LOCAL" which should normalize to "local"
        let result = assign_pane_with_strategy(&strategy, &shards, 1, Some("LOCAL"), None);
        assert_eq!(result, ShardId(1));
    }

    // -----------------------------------------------------------------------
    // watchdog_warnings formatting
    // -----------------------------------------------------------------------

    #[test]
    fn watchdog_warnings_includes_no_error_detail() {
        let report = ShardHealthReport {
            timestamp_ms: 1000,
            overall: HealthStatus::Critical,
            shards: vec![ShardHealthEntry {
                shard_id: ShardId(0),
                label: "s0".to_string(),
                status: HealthStatus::Critical,
                pane_count: None,
                circuit: CircuitBreakerStatus::default(),
                error: None,
            }],
        };
        let warnings = report.watchdog_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no error details"));
    }

    // -----------------------------------------------------------------------
    // SHARD_ID_BITS / LOCAL_PANE_ID_MASK constants
    // -----------------------------------------------------------------------

    #[test]
    fn shard_id_bits_and_mask_are_consistent() {
        assert_eq!(SHARD_ID_BITS, 16);
        assert_eq!(LOCAL_PANE_ID_MASK, (1u64 << 48) - 1);
    }

    // -- Batch: DarkBadger wa-1u90p.7.1 ----------------------------------------

    #[test]
    fn shard_id_display_v2() {
        assert_eq!(ShardId(0).to_string(), "0");
        assert_eq!(ShardId(42).to_string(), "42");
        assert_eq!(ShardId(65535).to_string(), "65535");
    }

    #[test]
    fn shard_id_debug_clone_copy_eq() {
        let a = ShardId(5);
        let b = a; // Copy
        assert_eq!(a, b);
        let c = a;
        assert_eq!(a, c);
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("ShardId"));
    }

    #[test]
    fn shard_id_hash_in_set() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        assert!(set.insert(ShardId(0)));
        assert!(set.insert(ShardId(1)));
        assert!(set.insert(ShardId(2)));
        assert_eq!(set.len(), 3);
        assert!(!set.insert(ShardId(1)));
    }

    #[test]
    fn shard_id_ord() {
        assert!(ShardId(0) < ShardId(1));
        assert!(ShardId(1) < ShardId(100));
        let mut ids = vec![ShardId(3), ShardId(1), ShardId(2)];
        ids.sort();
        assert_eq!(ids, vec![ShardId(1), ShardId(2), ShardId(3)]);
    }

    #[test]
    fn shard_id_serde_roundtrip_v2() {
        let id = ShardId(42);
        let json = serde_json::to_string(&id).unwrap();
        let parsed: ShardId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn encode_decode_shard_zero() {
        let encoded = encode_sharded_pane_id(ShardId(0), 123);
        let (shard, local) = decode_sharded_pane_id(encoded);
        assert_eq!(shard, ShardId(0));
        assert_eq!(local, 123);
    }

    #[test]
    fn is_sharded_pane_id_shard_zero() {
        let encoded = encode_sharded_pane_id(ShardId(0), 42);
        assert!(!is_sharded_pane_id(encoded));
    }

    #[test]
    fn is_sharded_pane_id_shard_nonzero() {
        let encoded = encode_sharded_pane_id(ShardId(1), 42);
        assert!(is_sharded_pane_id(encoded));
    }

    #[test]
    fn assignment_strategy_default_is_round_robin_v2() {
        assert_eq!(
            AssignmentStrategy::default(),
            AssignmentStrategy::RoundRobin
        );
    }

    #[test]
    fn assignment_strategy_serde_round_robin() {
        let s = AssignmentStrategy::RoundRobin;
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn assignment_strategy_serde_consistent_hash() {
        let s = AssignmentStrategy::ConsistentHash { virtual_nodes: 64 };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn assignment_strategy_serde_manual() {
        let s = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::from([(1, ShardId(0)), (2, ShardId(1))]),
            default_shard: Some(ShardId(0)),
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn assignment_strategy_debug_clone() {
        let s = AssignmentStrategy::RoundRobin;
        let cloned = s.clone();
        assert_eq!(s, cloned);
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("RoundRobin"));
    }

    #[test]
    fn assign_pane_empty_shards_returns_zero() {
        let result =
            assign_pane_with_strategy(&AssignmentStrategy::RoundRobin, &[], 42, None, None);
        assert_eq!(result, ShardId(0));
    }

    #[test]
    fn encode_max_local_pane_id() {
        let max_local = LOCAL_PANE_ID_MASK;
        let encoded = encode_sharded_pane_id(ShardId(1), max_local);
        let (shard, local) = decode_sharded_pane_id(encoded);
        assert_eq!(shard, ShardId(1));
        assert_eq!(local, max_local);
    }

    #[test]
    fn encode_local_id_overflow_is_masked() {
        // local_pane_id larger than LOCAL_PANE_ID_MASK gets masked
        let big_local = LOCAL_PANE_ID_MASK + 1;
        let encoded = encode_sharded_pane_id(ShardId(0), big_local);
        let (_, local) = decode_sharded_pane_id(encoded);
        assert_eq!(local, 0); // overflow bit masked off
    }
}
