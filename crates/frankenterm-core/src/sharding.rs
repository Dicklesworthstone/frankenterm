//! Sharded WezTerm routing for multi-mux deployments.
//!
//! This module introduces a shard-aware wrapper that can fan out pane discovery
//! across multiple mux backends and route pane-scoped operations back to the
//! owning shard.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::Result;
use crate::circuit_breaker::{CircuitBreakerStatus, CircuitStateKind};
use crate::consistent_hash::HashRing;
use crate::error::WeztermError;
use crate::patterns::AgentType;
use crate::watchdog::HealthStatus;
use crate::wezterm::{
    MoveDirection, PaneInfo, SplitDirection, WeztermFuture, WeztermHandle, WeztermInterface,
};

/// Number of high bits reserved for shard id in encoded pane ids.
pub const SHARD_ID_BITS: u32 = 16;

/// Mask for local pane id bits in encoded pane ids.
pub const LOCAL_PANE_ID_MASK: u64 = (1u64 << (64 - SHARD_ID_BITS)) - 1;

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

/// How panes should be assigned to shards.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "strategy")]
pub enum AssignmentStrategy {
    /// Select shards round-robin for new panes. Existing panes are routed by
    /// observed ownership.
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
        pane_to_shard: HashMap<u64, ShardId>,
        default_shard: Option<ShardId>,
    },
    /// Route by consistent hashing on pane id.
    ConsistentHash { virtual_nodes: u32 },
}

impl Default for AssignmentStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
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
pub struct ShardedWeztermClient {
    backends: Vec<ShardBackend>,
    backend_index: HashMap<ShardId, usize>,
    strategy: AssignmentStrategy,
    pane_routes: RwLock<HashMap<u64, PaneRoute>>,
    round_robin_cursor: AtomicUsize,
    hash_ring: Option<HashRing<ShardId>>,
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
        let idx = self.round_robin_cursor.fetch_add(1, Ordering::Relaxed) % self.backends.len();
        self.backends[idx].id
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
        let (panes, routes) = self.collect_panes().await?;
        let mut guard = self.pane_routes.write().await;
        *guard = routes;
        Ok(panes)
    }

    /// Build a shard-level health report for watchdog integration.
    pub async fn shard_health_report(&self) -> ShardHealthReport {
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
            let shard = self.choose_spawn_shard(domain_name.as_deref(), None);
            let backend = self.backend_for_id(shard)?;
            let local_id = backend
                .handle
                .spawn(cwd.as_deref(), domain_name.as_deref())
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

    #[tokio::test]
    async fn list_panes_aggregates_and_routes_text() {
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
    }

    #[tokio::test]
    async fn spawn_round_robin_across_shards() {
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
    }

    #[tokio::test]
    async fn shard_health_report_marks_failed_shard_hung() {
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
    }
}
