//! Sharded WezTerm routing for multi-mux deployments.
//!
//! This module introduces a shard-aware wrapper that can fan out pane discovery
//! across multiple mux backends and route pane-scoped operations back to the
//! owning shard.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use frankenterm_sigpipe::{RecoverablePanicSite, catch_recoverable, catch_recoverable_future};

use crate::Result;
use crate::circuit_breaker::{CircuitBreakerStatus, CircuitStateKind};
use crate::concurrent_map::PaneMap;
use crate::consistent_hash::HashRing;
use crate::error::{StorageError, WeztermError};
use crate::patterns::AgentType;
use crate::watchdog::HealthStatus;
use crate::wezterm::{
    MoveDirection, MuxSemanticSnapshot, PaneInfo, PaneTieredScrollbackSummary, SpawnTarget,
    SplitDirection, WeztermFuture, WeztermHandle, WeztermInterface,
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
    route_snapshot_conflicts: AtomicU64,
}

impl ShardingTelemetry {
    pub fn snapshot(&self) -> ShardingTelemetrySnapshot {
        ShardingTelemetrySnapshot {
            spawns: self.spawns.load(Ordering::Relaxed),
            pane_listings: self.pane_listings.load(Ordering::Relaxed),
            health_reports: self.health_reports.load(Ordering::Relaxed),
            route_lookups: self.route_lookups.load(Ordering::Relaxed),
            route_snapshot_conflicts: self
                .route_snapshot_conflicts
                .load(Ordering::Relaxed),
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
    /// Discovery snapshots skipped after a newer point mutation or generation exhaustion.
    pub route_snapshot_conflicts: u64,
}

/// Number of shard-id bits in the persistence-safe encoded pane-id domain.
///
/// SQLite stores pane ids in signed `INTEGER` columns throughout the storage
/// layer. The sign bit is therefore deliberately left unused: 15 shard bits
/// plus 48 local-pane bits produce a non-negative 63-bit identifier.
pub const SHARD_ID_BITS: u32 = 15;

/// Number of low bits reserved for the backend-local pane id.
pub const LOCAL_PANE_ID_BITS: u32 = 48;

/// Mask for local pane id bits in encoded pane ids.
pub const LOCAL_PANE_ID_MASK: u64 = (1u64 << LOCAL_PANE_ID_BITS) - 1;

/// Maximum shard id representable in encoded pane ids.
pub const MAX_SHARD_ID: usize = ((1u64 << SHARD_ID_BITS) - 1) as usize;

/// Maximum number of uniquely identified shards accepted by one client.
///
/// This also bounds the cardinality of a full health report, including a
/// cancelled report that retains explicit not-started entries.
pub const MAX_CONFIGURED_SHARDS: usize = MAX_SHARD_ID + 1;

/// Largest global pane id that every signed-64 persistence boundary accepts.
pub const MAX_GLOBAL_PANE_ID: u64 = (1u64 << 63) - 1;

/// Hard ceiling for rollback after a backend creates an unencodable pane.
///
/// The backend's normal command timeout is currently 30 seconds. Matching it
/// here prevents a compensating task from outliving its useful recovery window
/// even if a custom backend ignores Cx budgets.
const PANE_CREATION_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded, content-free errors for the two audited rollback panic phases.
/// Never include the original panic payload here: backend implementations may
/// panic with credentials, pane contents, paths, or other caller-controlled
/// text.
const PANE_CREATION_ROLLBACK_OPERATION_PANIC: &str =
    "WA-SHARDING-ROLLBACK-PANIC: pane-creation rollback operation panicked";
const PANE_CREATION_ROLLBACK_JOIN_PANIC: &str =
    "WA-SHARDING-ROLLBACK-PANIC: pane-creation rollback join panicked";

/// Content-free result classes for rollback paths that cannot preserve the
/// original backend diagnostic safely. Backend and scheduler errors can carry
/// pane text, paths, credentials, or arbitrarily large caller-controlled
/// strings, so public errors retain only these finite classes.
const PANE_CREATION_ROLLBACK_TIMEOUT_CLASS: &str =
    "WA-SHARDING-ROLLBACK-TIMEOUT: pane-creation rollback timed out";
const PANE_CREATION_ROLLBACK_ADMISSION_CLASS: &str =
    "WA-SHARDING-ROLLBACK-ADMISSION: rollback admission and inline cleanup failed";

const SHARD_BACKEND_CALLBACK_PANIC_DETAIL: &str =
    "WA-SHARDING-BACKEND-PANIC: backend health callback panicked";

/// Identifier for a mux shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShardId(pub usize);

impl std::fmt::Display for ShardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Fallibly encode `(shard_id, local_pane_id)` into a globally unique,
/// persistence-safe pane id.
///
/// The 63-bit layout is `[unused sign bit][15-bit shard][48-bit local]`.
/// Existing ids for shards `0..=32767` retain exactly the same bit pattern as
/// the former 16+48 layout. Inputs outside either field are rejected instead
/// of being truncated or allowed to cross SQLite's signed-integer boundary.
pub fn try_encode_sharded_pane_id(shard_id: ShardId, local_pane_id: u64) -> Result<u64> {
    if shard_id.0 > MAX_SHARD_ID {
        return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
            format!(
                "shard id {} exceeds {}-bit persistence-safe encoded capacity (max={MAX_SHARD_ID})",
                shard_id.0, SHARD_ID_BITS
            ),
        )));
    }
    if local_pane_id > LOCAL_PANE_ID_MASK {
        return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
            format!(
                "local pane id {local_pane_id} exceeds {LOCAL_PANE_ID_BITS}-bit encoded capacity (max={LOCAL_PANE_ID_MASK})"
            ),
        )));
    }

    let encoded = ((shard_id.0 as u64) << LOCAL_PANE_ID_BITS) | local_pane_id;
    debug_assert!(encoded <= MAX_GLOBAL_PANE_ID);
    Ok(encoded)
}

/// Encode `(shard_id, local_pane_id)` into a globally unique pane id.
///
/// # Panics
///
/// Panics when either input is outside the persistence-safe encoded domain.
/// Production mux paths use [`try_encode_sharded_pane_id`] so an invalid
/// backend response becomes a normal error rather than a process panic.
#[must_use]
pub fn encode_sharded_pane_id(shard_id: ShardId, local_pane_id: u64) -> u64 {
    try_encode_sharded_pane_id(shard_id, local_pane_id)
        .unwrap_or_else(|err| panic!("{err}"))
}

/// Fallibly decode a persistence-safe global pane id.
pub fn try_decode_sharded_pane_id(global_pane_id: u64) -> Result<(ShardId, u64)> {
    if global_pane_id > MAX_GLOBAL_PANE_ID {
        return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
            format!(
                "global pane id {global_pane_id} exceeds persistence-safe signed-64 range (max={MAX_GLOBAL_PANE_ID})"
            ),
        )));
    }

    let shard_idx = (global_pane_id >> LOCAL_PANE_ID_BITS) as usize;
    let local = global_pane_id & LOCAL_PANE_ID_MASK;
    Ok((ShardId(shard_idx), local))
}

/// Decode a globally encoded pane id into `(shard_id, local_pane_id)`.
///
/// # Panics
///
/// Panics when `global_pane_id` is outside the persistence-safe encoded
/// domain. Production routing paths use [`try_decode_sharded_pane_id`] so
/// untrusted or stale ids become normal errors rather than process panics.
#[must_use]
pub fn decode_sharded_pane_id(global_pane_id: u64) -> (ShardId, u64) {
    try_decode_sharded_pane_id(global_pane_id).unwrap_or_else(|err| panic!("{err}"))
}

/// Returns true when a pane id has non-zero shard bits.
#[must_use]
pub fn is_sharded_pane_id(pane_id: u64) -> bool {
    (pane_id >> LOCAL_PANE_ID_BITS) != 0
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
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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

impl std::fmt::Debug for AssignmentStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoundRobin => f.write_str("RoundRobin"),
            Self::ByDomain {
                domain_to_shard,
                default_shard,
            } => f
                .debug_struct("ByDomain")
                .field("mapping_count", &domain_to_shard.len())
                .field("default_shard", default_shard)
                .finish(),
            Self::ByAgentType {
                agent_to_shard,
                default_shard,
            } => f
                .debug_struct("ByAgentType")
                .field("mapping_count", &agent_to_shard.len())
                .field("default_shard", default_shard)
                .finish(),
            Self::Manual {
                pane_to_shard,
                default_shard,
            } => f
                .debug_struct("Manual")
                .field("mapping_count", &pane_to_shard.len())
                .field("default_shard", default_shard)
                .finish(),
            Self::ConsistentHash { virtual_nodes } => f
                .debug_struct("ConsistentHash")
                .field("virtual_nodes", virtual_nodes)
                .finish(),
        }
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
    pub handle: WeztermHandle,
}

/// Finite classification of a backend failure observed by sharded routing.
///
/// This is deliberately less detailed than [`crate::Error`]. The latter can
/// contain untrusted backend text, while this enum is safe to project into
/// operator errors, telemetry, health reports, and debug output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardBackendErrorClass {
    /// Backend executable, process, or transport endpoint unavailable.
    Unavailable,
    /// Requested pane was absent.
    PaneNotFound,
    /// Backend rejected or failed a command without a stronger class.
    CommandFailed,
    /// Backend response could not be parsed or validated.
    InvalidResponse,
    /// Backend response exceeded an enforced size limit.
    OutputTooLarge,
    /// Operation exceeded its time budget.
    TimedOut,
    /// Circuit breaker rejected the operation.
    CircuitOpen,
    /// A non-idempotent backend mutation may already have taken effect.
    IndeterminateMutation,
    /// Backend operation reported cancellation.
    #[serde(rename = "backend_cancelled")]
    Cancelled,
    /// Backend operation crossed a contained panic boundary.
    #[serde(rename = "backend_panicked")]
    Panicked,
    /// Local I/O failed.
    Io,
    /// Failure did not match a more specific safe class.
    Other,
}

impl ShardBackendErrorClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::PaneNotFound => "pane_not_found",
            Self::CommandFailed => "command_failed",
            Self::InvalidResponse => "invalid_response",
            Self::OutputTooLarge => "output_too_large",
            Self::TimedOut => "timed_out",
            Self::CircuitOpen => "circuit_open",
            Self::IndeterminateMutation => "indeterminate_mutation",
            Self::Cancelled => "backend_cancelled",
            Self::Panicked => "backend_panicked",
            Self::Io => "io",
            Self::Other => "other",
        }
    }
}

impl std::fmt::Display for ShardBackendErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Finite outcome for one shard probe in a health scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "error_class")]
pub enum ShardHealthProbeOutcome {
    /// Probe finished successfully.
    Complete,
    /// Probe finished with a finite backend error class.
    Failed(ShardBackendErrorClass),
    /// Probe was interrupted by scan cancellation.
    Cancelled,
    /// Scan cancellation was observed before this probe began.
    NotStarted,
}

impl ShardHealthProbeOutcome {
    const fn warning_token(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Failed(class) => class.as_str(),
            Self::Cancelled => "scan_cancelled",
            Self::NotStarted => "not_started",
        }
    }
}

/// Completion state for an entire shard health scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardHealthReportOutcome {
    /// Every configured shard was inspected.
    Complete,
    /// The scan was interrupted; per-shard outcomes identify the boundary.
    Cancelled,
}

/// Health for a single shard backend.
#[derive(Clone)]
pub struct ShardHealthEntry {
    pub shard_id: ShardId,
    pub status: HealthStatus,
    pub pane_count: Option<usize>,
    pub circuit: CircuitBreakerStatus,
    /// Finite probe result. Raw backend diagnostics never enter health-report
    /// state or its operator-facing projections.
    pub probe_outcome: ShardHealthProbeOutcome,
}

impl Serialize for ShardHealthEntry {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        validate_shard_health_entry(self).map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("ShardHealthEntry", 5)?;
        state.serialize_field("shard_id", &self.shard_id)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("pane_count", &self.pane_count)?;
        state.serialize_field("circuit", &self.circuit)?;
        state.serialize_field("probe_outcome", &self.probe_outcome)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ShardHealthEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEntry {
            shard_id: ShardId,
            status: HealthStatus,
            pane_count: Option<usize>,
            circuit: CircuitBreakerStatus,
            probe_outcome: ShardHealthProbeOutcome,
        }

        let wire = WireEntry::deserialize(deserializer)?;
        let entry = Self {
            shard_id: wire.shard_id,
            status: wire.status,
            pane_count: wire.pane_count,
            circuit: wire.circuit,
            probe_outcome: wire.probe_outcome,
        };
        validate_shard_health_entry(&entry).map_err(serde::de::Error::custom)?;
        Ok(entry)
    }
}

impl std::fmt::Debug for ShardHealthEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardHealthEntry")
            .field("shard_id", &self.shard_id)
            .field("status", &self.status)
            .field("pane_count", &self.pane_count)
            .field("circuit", &self.circuit)
            .field("probe_outcome", &self.probe_outcome)
            .finish()
    }
}

/// Point-in-time health report across all configured shards.
#[derive(Clone)]
pub struct ShardHealthReport {
    pub timestamp_ms: u64,
    pub overall: HealthStatus,
    pub shards: Vec<ShardHealthEntry>,
}

impl std::fmt::Debug for ShardHealthReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const DEBUG_ENTRY_LIMIT: usize = 16;

        let admitted = self.shards.len().min(DEBUG_ENTRY_LIMIT);
        f.debug_struct("ShardHealthReport")
            .field("timestamp_ms", &self.timestamp_ms)
            .field("overall", &self.overall)
            .field("outcome", &self.outcome())
            .field("shard_count", &self.shards.len())
            .field("shards", &&self.shards[..admitted])
            .field("omitted_shards", &self.shards.len().saturating_sub(admitted))
            .finish()
    }
}

fn deserialize_bounded_shard_health_entries<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ShardHealthEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedShardEntriesVisitor;

    impl<'de> serde::de::Visitor<'de> for BoundedShardEntriesVisitor {
        type Value = Vec<ShardHealthEntry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "at most {MAX_CONFIGURED_SHARDS} shard health entries"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let size_hint = sequence.size_hint();
            if let Some(length) = size_hint.filter(|length| *length > MAX_CONFIGURED_SHARDS) {
                return Err(serde::de::Error::invalid_length(
                    length,
                    &self,
                ));
            }

            let capacity = size_hint.unwrap_or_default().min(MAX_CONFIGURED_SHARDS);
            let mut entries = Vec::with_capacity(capacity);
            while let Some(entry) = sequence.next_element()? {
                if entries.len() == MAX_CONFIGURED_SHARDS {
                    return Err(serde::de::Error::invalid_length(
                        MAX_CONFIGURED_SHARDS + 1,
                        &self,
                    ));
                }
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_seq(BoundedShardEntriesVisitor)
}

impl Serialize for ShardHealthReport {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let outcome = self.outcome();
        validate_shard_health_report(self, outcome).map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("ShardHealthReport", 4)?;
        state.serialize_field("timestamp_ms", &self.timestamp_ms)?;
        state.serialize_field("overall", &self.overall)?;
        state.serialize_field("outcome", &outcome)?;
        state.serialize_field("shards", &self.shards)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ShardHealthReport {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireReport {
            timestamp_ms: u64,
            overall: HealthStatus,
            outcome: ShardHealthReportOutcome,
            #[serde(deserialize_with = "deserialize_bounded_shard_health_entries")]
            shards: Vec<ShardHealthEntry>,
        }

        let wire = WireReport::deserialize(deserializer)?;
        let report = Self {
            timestamp_ms: wire.timestamp_ms,
            overall: wire.overall,
            shards: wire.shards,
        };
        validate_shard_health_report(&report, wire.outcome)
            .map_err(serde::de::Error::custom)?;
        Ok(report)
    }
}

impl ShardHealthReport {
    /// Maximum number of per-shard warnings admitted to one live watchdog
    /// snapshot. The optional final entry reports how many additional
    /// unhealthy shards were omitted.
    const WATCHDOG_WARNING_LIMIT: usize = 64;

    /// Report whether all configured shards were inspected.
    ///
    /// A cancelled scan always retains one stable entry per configured shard;
    /// entries that were interrupted or never started carry explicit typed
    /// outcomes instead of silently disappearing from an apparently healthy
    /// report.
    #[must_use]
    pub fn outcome(&self) -> ShardHealthReportOutcome {
        if self.shards.iter().any(|entry| {
            matches!(
                entry.probe_outcome,
                ShardHealthProbeOutcome::Cancelled | ShardHealthProbeOutcome::NotStarted
            )
        }) {
            ShardHealthReportOutcome::Cancelled
        } else {
            ShardHealthReportOutcome::Complete
        }
    }

    /// Return shard entries that are unhealthy or whose health was not
    /// observed because the scan was interrupted.
    #[must_use]
    pub fn unhealthy_shards(&self) -> Vec<&ShardHealthEntry> {
        self.shards
            .iter()
            .filter(|entry| entry.status != HealthStatus::Healthy)
            .collect()
    }

    /// Render human-readable warnings suitable for watchdog snapshots.
    ///
    /// Raw backend diagnostics are deliberately excluded because they can
    /// contain caller-controlled pane text, paths, credentials, or arbitrarily
    /// large strings. The watchdog surface needs stable operational
    /// classification, not a second copy of backend diagnostics. Every line is
    /// therefore content-free and bounded by fixed-width enum/numeric fields,
    /// while the number of lines is capped independently of configured shard
    /// count.
    #[must_use]
    pub fn watchdog_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::with_capacity(
            self.shards
                .len()
                .min(Self::WATCHDOG_WARNING_LIMIT.saturating_add(1)),
        );
        let mut omitted = 0usize;
        for entry in self
            .shards
            .iter()
            .filter(|entry| entry.status != HealthStatus::Healthy)
        {
            if warnings.len() >= Self::WATCHDOG_WARNING_LIMIT {
                omitted = omitted.saturating_add(1);
                continue;
            }
            let probe_outcome = entry.probe_outcome;
            warnings.push(format!(
                "Shard {} {} (status={}, circuit={}, probe={})",
                entry.shard_id.0,
                if matches!(
                    probe_outcome,
                    ShardHealthProbeOutcome::Cancelled | ShardHealthProbeOutcome::NotStarted
                ) {
                    "health unknown"
                } else {
                    "unhealthy"
                },
                entry.status,
                if probe_outcome == ShardHealthProbeOutcome::NotStarted {
                    "not_observed"
                } else {
                    circuit_state_token(entry.circuit.state)
                },
                probe_outcome.warning_token(),
            ));
        }
        if omitted > 0 {
            warnings.push(format!(
                "Shard watchdog omitted {omitted} additional unhealthy shard(s) after bounded limit {}",
                Self::WATCHDOG_WARNING_LIMIT
            ));
        }
        warnings
    }
}

impl std::fmt::Debug for ShardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardBackend")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl ShardBackend {
    #[must_use]
    pub fn new(id: ShardId, handle: WeztermHandle) -> Self {
        Self { id, handle }
    }
}

const fn circuit_state_token(state: CircuitStateKind) -> &'static str {
    match state {
        CircuitStateKind::Closed => "closed",
        CircuitStateKind::Open => "open",
        CircuitStateKind::HalfOpen => "half_open",
    }
}

fn classify_backend_error(error: &crate::Error) -> ShardBackendErrorClass {
    match error {
        crate::Error::Wezterm(error) => match error {
            WeztermError::CliNotFound
            | WeztermError::NotRunning
            | WeztermError::SocketNotFound(_) => ShardBackendErrorClass::Unavailable,
            WeztermError::PaneNotFound(_) => ShardBackendErrorClass::PaneNotFound,
            WeztermError::CommandFailed(_) => ShardBackendErrorClass::CommandFailed,
            WeztermError::ParseError(_) => ShardBackendErrorClass::InvalidResponse,
            WeztermError::OutputTooLarge { .. } => ShardBackendErrorClass::OutputTooLarge,
            WeztermError::Timeout(_) => ShardBackendErrorClass::TimedOut,
            WeztermError::CircuitOpen { .. } => ShardBackendErrorClass::CircuitOpen,
            WeztermError::IndeterminateMutation { .. } => {
                ShardBackendErrorClass::IndeterminateMutation
            }
        },
        crate::Error::Storage(
            StorageError::IndeterminateMutation { .. }
            | StorageError::WriterSettlementIndeterminate { .. },
        ) => ShardBackendErrorClass::IndeterminateMutation,
        crate::Error::RuntimeOperation {
            source: crate::error::RuntimeOperationSource::Cancelled(_),
            ..
        }
        | crate::Error::Cancelled(_) => ShardBackendErrorClass::Cancelled,
        crate::Error::Panicked(_) => ShardBackendErrorClass::Panicked,
        crate::Error::PaneOperation {
            source: crate::error::PaneOperationSource::PaneNotFound,
            ..
        } => ShardBackendErrorClass::PaneNotFound,
        crate::Error::Io(_) => ShardBackendErrorClass::Io,
        _ => ShardBackendErrorClass::Other,
    }
}

fn backend_callback_panic_error() -> crate::Error {
    crate::Error::Panicked(SHARD_BACKEND_CALLBACK_PANIC_DETAIL.to_owned())
}

fn health_circuit_status(backend: &ShardBackend) -> Result<CircuitBreakerStatus> {
    catch_recoverable(
        RecoverablePanicSite::ClientCallback,
        std::panic::AssertUnwindSafe(|| backend.handle.circuit_status()),
    )
    .map_err(|_panic| backend_callback_panic_error())
}

async fn cx_health_panes(backend: &ShardBackend, cx: &crate::cx::Cx) -> Result<Vec<PaneInfo>> {
    let future = match catch_recoverable(
        RecoverablePanicSite::ClientCallback,
        std::panic::AssertUnwindSafe(|| backend.handle.list_panes_with_cx(cx)),
    ) {
        Ok(future) => future,
        Err(_panic) => return Err(backend_callback_panic_error()),
    };
    match catch_recoverable_future(RecoverablePanicSite::ClientCallback, future).await {
        Ok(result) => result,
        Err(_panic) => Err(backend_callback_panic_error()),
    }
}

fn pending_shard_health_entry(backend: &ShardBackend) -> ShardHealthEntry {
    // A pending entry must be constructible without invoking any backend. In
    // particular, a pre-cancelled scan must not call even synchronous backend
    // callbacks while creating its stable topology projection.
    let circuit = CircuitBreakerStatus::default();
    ShardHealthEntry {
        shard_id: backend.id,
        status: health_from_circuit_state(circuit.state).max(HealthStatus::Degraded),
        pane_count: None,
        circuit,
        probe_outcome: ShardHealthProbeOutcome::NotStarted,
    }
}

fn completed_shard_health_entry(
    backend: &ShardBackend,
    circuit: CircuitBreakerStatus,
    panes: std::result::Result<Vec<PaneInfo>, crate::Error>,
) -> ShardHealthEntry {
    let circuit_health = health_from_circuit_state(circuit.state);
    match panes {
        Ok(panes) => ShardHealthEntry {
            shard_id: backend.id,
            status: circuit_health,
            pane_count: Some(panes.len()),
            circuit,
            probe_outcome: ShardHealthProbeOutcome::Complete,
        },
        Err(error) => ShardHealthEntry {
            shard_id: backend.id,
            status: circuit_health.max(HealthStatus::Hung),
            pane_count: None,
            circuit,
            probe_outcome: ShardHealthProbeOutcome::Failed(classify_backend_error(&error)),
        },
    }
}

fn cancelled_shard_health_entry(
    backend: &ShardBackend,
    circuit: CircuitBreakerStatus,
) -> ShardHealthEntry {
    ShardHealthEntry {
        shard_id: backend.id,
        status: health_from_circuit_state(circuit.state).max(HealthStatus::Degraded),
        pane_count: None,
        circuit,
        probe_outcome: ShardHealthProbeOutcome::Cancelled,
    }
}

fn overall_shard_health(shards: &[ShardHealthEntry]) -> HealthStatus {
    shards
        .iter()
        .fold(HealthStatus::Healthy, |overall, entry| {
            overall.max(entry.status)
        })
}

fn validate_shard_health_entry(entry: &ShardHealthEntry) -> std::result::Result<(), &'static str> {
    if entry.shard_id.0 > MAX_SHARD_ID {
        return Err("shard health entry contains an out-of-range shard id");
    }
    if entry.probe_outcome != ShardHealthProbeOutcome::Complete
        && entry.status == HealthStatus::Healthy
    {
        return Err("an incomplete or failed shard health probe cannot be healthy");
    }
    if matches!(
        entry.probe_outcome,
        ShardHealthProbeOutcome::Failed(_) | ShardHealthProbeOutcome::NotStarted
    ) && entry.pane_count.is_some()
    {
        return Err("an unobserved or failed shard health probe cannot report a pane count");
    }
    if entry.probe_outcome == ShardHealthProbeOutcome::Complete && entry.pane_count.is_none() {
        return Err("a completed shard health probe must report a pane count");
    }
    Ok(())
}

fn validate_shard_health_report(
    report: &ShardHealthReport,
    declared_outcome: ShardHealthReportOutcome,
) -> std::result::Result<(), &'static str> {
    if report.shards.len() > MAX_CONFIGURED_SHARDS {
        return Err("shard health report exceeds bounded shard capacity");
    }

    let mut previous_shard_id = None;
    let mut computed_outcome = ShardHealthReportOutcome::Complete;
    let mut computed_overall = HealthStatus::Healthy;
    for entry in &report.shards {
        if entry.shard_id.0 > MAX_SHARD_ID {
            return Err("shard health report contains an out-of-range shard id");
        }
        if previous_shard_id.is_some_and(|previous| previous >= entry.shard_id) {
            return Err("shard health report entries are not strictly ordered and unique");
        }
        validate_shard_health_entry(entry)?;
        previous_shard_id = Some(entry.shard_id);
        computed_overall = computed_overall.max(entry.status);
        if matches!(
            entry.probe_outcome,
            ShardHealthProbeOutcome::Cancelled | ShardHealthProbeOutcome::NotStarted
        ) {
            computed_outcome = ShardHealthReportOutcome::Cancelled;
        }
    }
    if computed_outcome != declared_outcome {
        return Err("shard health report outcome disagrees with its entries");
    }
    if computed_overall != report.overall {
        return Err("shard health report overall status disagrees with its entries");
    }
    Ok(())
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
    pane_routes: PaneMap<PaneRoute>,
    pane_route_commit: std::sync::Mutex<()>,
    pane_route_generation: AtomicU64,
    round_robin_cursor: AtomicUsize,
    hash_ring: Option<HashRing<ShardId>>,
    telemetry: ShardingTelemetry,
}

impl std::fmt::Debug for ShardedWeztermClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardedWeztermClient")
            .field("backend_count", &self.backends.len())
            .field("strategy", &self.strategy)
            .field(
                "pane_route_generation",
                &self.pane_route_generation.load(Ordering::Relaxed),
            )
            .field("hash_ring_present", &self.hash_ring.is_some())
            .field("telemetry", &self.telemetry.snapshot())
            .finish_non_exhaustive()
    }
}

impl ShardedWeztermClient {
    /// Create a new sharded client.
    pub fn new(mut backends: Vec<ShardBackend>, strategy: AssignmentStrategy) -> Result<Self> {
        if backends.is_empty() {
            return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                "sharded client requires at least one backend".to_string(),
            )));
        }
        if backends.len() > MAX_CONFIGURED_SHARDS {
            return Err(crate::Error::Wezterm(WeztermError::CommandFailed(format!(
                "sharded client backend count {} exceeds bounded capacity {MAX_CONFIGURED_SHARDS}",
                backends.len()
            ))));
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
                "shard id {} exceeds {}-bit persistence-safe encoded pane id capacity (max {})",
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
            pane_routes: PaneMap::new(),
            pane_route_commit: std::sync::Mutex::new(()),
            pane_route_generation: AtomicU64::new(0),
            round_robin_cursor: AtomicUsize::new(0),
            hash_ring,
            telemetry: ShardingTelemetry::default(),
        })
    }

    /// Convenience constructor assigning shard ids sequentially from handles.
    pub fn from_handles(strategy: AssignmentStrategy, handles: Vec<WeztermHandle>) -> Result<Self> {
        if handles.len() > MAX_CONFIGURED_SHARDS {
            return Err(crate::Error::Wezterm(WeztermError::CommandFailed(format!(
                "sharded client backend count {} exceeds bounded capacity {MAX_CONFIGURED_SHARDS}",
                handles.len()
            ))));
        }
        let backends = handles
            .into_iter()
            .enumerate()
            .map(|(idx, handle)| ShardBackend::new(ShardId(idx), handle))
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

    fn lock_pane_route_commit(&self) -> std::sync::MutexGuard<'_, ()> {
        self.pane_route_commit.lock().unwrap_or_else(|poison| {
            tracing::error!(
                "sharded pane-route commit mutex was poisoned; recovering the protected generation"
            );
            self.pane_route_commit.clear_poison();
            poison.into_inner()
        })
    }

    fn pane_route_generation(&self) -> u64 {
        self.pane_route_generation.load(Ordering::Acquire)
    }

    fn advance_pane_route_generation_locked(&self) {
        let current = self.pane_route_generation.load(Ordering::Relaxed);
        if let Some(next) = current.checked_add(1) {
            self.pane_route_generation.store(next, Ordering::Release);
        }
        // `u64::MAX` is a permanent exhausted sentinel. Point mutations still
        // update the cache under the commit mutex, while full snapshots fail
        // closed below; wrapping to zero would let an ancient snapshot match.
    }

    fn insert_pane_route(&self, pane_id: u64, route: PaneRoute) {
        let _commit = self.lock_pane_route_commit();
        self.pane_routes.insert(pane_id, route);
        // This helper is the post-success linearization point for pane
        // creation/navigation. Even an identical cached value represents a
        // newer backend generation that an older discovery snapshot must not
        // erase.
        self.advance_pane_route_generation_locked();
    }

    fn remove_pane_route(&self, pane_id: u64) {
        let _commit = self.lock_pane_route_commit();
        self.pane_routes.remove(pane_id);
        // A successful backend kill is newer truth even when this cache did
        // not contain the route (cold ids decode without cache insertion).
        self.advance_pane_route_generation_locked();
    }

    /// Publish a full discovery snapshot only if no point update committed
    /// after discovery began and the generation remains representable.
    /// Without this generation fence, a slow listing can erase a newly spawned
    /// pane route or resurrect a route removed by a concurrent successful kill.
    fn publish_pane_route_snapshot(
        &self,
        expected_generation: u64,
        routes: HashMap<u64, PaneRoute>,
    ) -> bool {
        let _commit = self.lock_pane_route_commit();
        let current_generation = self.pane_route_generation.load(Ordering::Acquire);
        if current_generation == u64::MAX || current_generation != expected_generation {
            self.telemetry
                .route_snapshot_conflicts
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }

        self.pane_routes.replace_all(routes);
        self.advance_pane_route_generation_locked();
        true
    }

    fn backend_error(
        shard_id: ShardId,
        op: &'static str,
        pane_id: Option<u64>,
        err: crate::Error,
    ) -> crate::Error {
        let error_class = classify_backend_error(&err);
        let pane_hint = pane_id.map_or_else(String::new, |id| format!(", pane={id}"));
        let finite_detail = || {
            format!("{op} failed on shard {shard_id}{pane_hint} (class={error_class})")
        };

        // Preserve every existing typed class that controls retry/cancellation
        // semantics while replacing untrusted backend strings with a finite
        // shard-local projection. In particular, laundering an indeterminate
        // mutation into CommandFailed makes generic retry code duplicate
        // pane creation, input, kill, activation, or zoom effects.
        match err {
            crate::Error::Wezterm(error) => crate::Error::Wezterm(match error {
                WeztermError::CliNotFound => WeztermError::CliNotFound,
                WeztermError::NotRunning => WeztermError::NotRunning,
                WeztermError::PaneNotFound(local_pane_id) => {
                    WeztermError::PaneNotFound(pane_id.unwrap_or(local_pane_id))
                }
                WeztermError::SocketNotFound(_) => WeztermError::SocketNotFound(format!(
                    "shard-{shard_id}-backend-endpoint"
                )),
                WeztermError::CommandFailed(_) => WeztermError::CommandFailed(finite_detail()),
                WeztermError::IndeterminateMutation { .. } => {
                    WeztermError::IndeterminateMutation { operation: op }
                }
                WeztermError::ParseError(_) => WeztermError::ParseError(finite_detail()),
                WeztermError::OutputTooLarge { len, cap, .. } => {
                    WeztermError::OutputTooLarge {
                        command: format!("{op} on shard {shard_id}"),
                        len,
                        cap,
                    }
                }
                WeztermError::Timeout(seconds) => WeztermError::Timeout(seconds),
                WeztermError::CircuitOpen { retry_after_ms } => {
                    WeztermError::CircuitOpen { retry_after_ms }
                }
            }),
            crate::Error::Cancelled(_) => crate::Error::Cancelled(format!(
                "{op} cancelled on shard {shard_id}"
            )),
            crate::Error::RuntimeOperation { source, .. } => match source {
                crate::error::RuntimeOperationSource::Backend(_) => {
                    crate::Error::Wezterm(WeztermError::CommandFailed(finite_detail()))
                }
                crate::error::RuntimeOperationSource::Cancelled(_) => {
                    crate::Error::RuntimeOperation {
                        operation: op,
                        source: crate::error::RuntimeOperationSource::Cancelled(
                            "sharded backend cancellation".to_owned(),
                        ),
                    }
                }
                finite_source => crate::Error::RuntimeOperation {
                    operation: op,
                    source: finite_source,
                },
            },
            crate::Error::Storage(error) => crate::Error::Storage(match error {
                StorageError::Database(_) => StorageError::Database(finite_detail()),
                StorageError::WriterBackendEpochPoisoned => {
                    StorageError::WriterBackendEpochPoisoned
                }
                StorageError::BackendEpochPoisoned => StorageError::BackendEpochPoisoned,
                StorageError::MigrationEpochPoisoned => StorageError::MigrationEpochPoisoned,
                StorageError::IndeterminateMutation { .. } => {
                    StorageError::IndeterminateMutation { operation: op }
                }
                StorageError::WriterSettlementIndeterminate { .. } => {
                    StorageError::WriterSettlementIndeterminate {
                        phase: "shard_backend_writer_settlement",
                    }
                }
                StorageError::WriterClosed => StorageError::WriterClosed,
                StorageError::SubmitIdempotency(error) => {
                    StorageError::SubmitIdempotency(error)
                }
                StorageError::ReservationConflict {
                    pane_id,
                    existing_id,
                } => StorageError::ReservationConflict {
                    pane_id,
                    existing_id,
                },
                StorageError::InvalidEventDeliveryLeaseBatch(error) => {
                    StorageError::InvalidEventDeliveryLeaseBatch(error)
                }
                StorageError::LeaseTokenConflict { event_id } => {
                    StorageError::LeaseTokenConflict { event_id }
                }
                StorageError::LeaseOwnershipConflict { updated, expected } => {
                    StorageError::LeaseOwnershipConflict { updated, expected }
                }
                StorageError::SequenceDiscontinuity { expected, actual } => {
                    StorageError::SequenceDiscontinuity { expected, actual }
                }
                StorageError::MigrationFailed(_) => {
                    StorageError::MigrationFailed(finite_detail())
                }
                StorageError::SchemaTooNew { current, supported } => {
                    StorageError::SchemaTooNew { current, supported }
                }
                StorageError::WaTooOld { .. } => StorageError::WaTooOld {
                    current: "redacted".to_owned(),
                    min_compatible: "redacted".to_owned(),
                },
                StorageError::FtsQueryError(_) => {
                    StorageError::FtsQueryError(finite_detail())
                }
                StorageError::Corruption { .. } => StorageError::Corruption {
                    details: finite_detail(),
                },
                StorageError::NotFound(_) => StorageError::NotFound(finite_detail()),
            }),
            crate::Error::PaneOperation {
                pane_id: backend_pane_id,
                source: crate::error::PaneOperationSource::PaneNotFound,
                ..
            } => crate::Error::PaneOperation {
                pane_id: pane_id.unwrap_or(backend_pane_id),
                operation: op,
                source: crate::error::PaneOperationSource::PaneNotFound,
            },
            crate::Error::Io(error) => crate::Error::Io(std::io::Error::new(
                error.kind(),
                finite_detail(),
            )),
            crate::Error::Panicked(_) => crate::Error::Panicked(format!(
                "{op} backend panicked on shard {shard_id}"
            )),
            crate::Error::CaptureAuthority(error) => crate::Error::CaptureAuthority(error),
            _ => crate::Error::Wezterm(WeztermError::CommandFailed(finite_detail())),
        }
    }

    fn codec_error_with_cleanup_failure(
        creation_op: &'static str,
        cleanup_op: &'static str,
        backend: &ShardBackend,
        local_pane_id: u64,
        cleanup_error: &crate::Error,
    ) -> crate::Error {
        // Keep the codec failure first and in the same error variant so
        // rollback trouble adds evidence without replacing the root cause.
        // An ordinary cleanup error is just as untrusted as a panic payload:
        // either can contain credentials, pane text, paths, or an arbitrarily
        // large string. Retain only static operation classes and numeric
        // routing identity in the compensator suffix.
        let primary_detail = format!(
            "local pane id {local_pane_id} exceeds {LOCAL_PANE_ID_BITS}-bit encoded capacity (max={LOCAL_PANE_ID_MASK})"
        );
        let cleanup_class = match cleanup_error {
            crate::Error::Wezterm(WeztermError::CommandFailed(detail))
                if detail == PANE_CREATION_ROLLBACK_OPERATION_PANIC =>
            {
                PANE_CREATION_ROLLBACK_OPERATION_PANIC
            }
            crate::Error::Wezterm(WeztermError::CommandFailed(detail))
                if detail == PANE_CREATION_ROLLBACK_JOIN_PANIC =>
            {
                PANE_CREATION_ROLLBACK_JOIN_PANIC
            }
            crate::Error::Wezterm(WeztermError::CommandFailed(detail))
                if detail == PANE_CREATION_ROLLBACK_TIMEOUT_CLASS =>
            {
                PANE_CREATION_ROLLBACK_TIMEOUT_CLASS
            }
            crate::Error::Wezterm(WeztermError::CommandFailed(detail))
                if detail == PANE_CREATION_ROLLBACK_ADMISSION_CLASS =>
            {
                PANE_CREATION_ROLLBACK_ADMISSION_CLASS
            }
            _ => "cleanup_failed",
        };
        crate::Error::Wezterm(WeztermError::CommandFailed(format!(
            "{primary_detail}; best-effort rollback after {creation_op} failed: {cleanup_op} for local pane {local_pane_id} on shard {} ({cleanup_class})",
            backend.id
        )))
    }

    async fn encode_created_pane_or_rollback(
        backend: &ShardBackend,
        shard_id: ShardId,
        local_pane_id: u64,
        creation_op: &'static str,
    ) -> Result<u64> {
        let primary = match try_encode_sharded_pane_id(shard_id, local_pane_id) {
            Ok(global_pane_id) => return Ok(global_pane_id),
            Err(primary) => primary,
        };

        match Self::rollback_unencodable_pane_with_fresh_cx(backend, local_pane_id).await {
            Ok(()) => Err(primary),
            Err(cleanup_error) => {
                Err(Self::codec_error_with_cleanup_failure(
                    creation_op,
                    "kill_pane_with_fresh_cleanup_cx",
                    backend,
                    local_pane_id,
                    &cleanup_error,
                ))
            }
        }
    }

    async fn run_bounded_pane_creation_rollback(
        cleanup_cx: &crate::cx::Cx,
        handle: &WeztermHandle,
        local_pane_id: u64,
    ) -> Result<()> {
        match crate::runtime_async::timeout_with_cx(
            cleanup_cx,
            PANE_CREATION_ROLLBACK_TIMEOUT,
            handle.kill_pane_with_cx(cleanup_cx, local_pane_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_timeout_error) => Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                PANE_CREATION_ROLLBACK_TIMEOUT_CLASS.to_owned(),
            ))),
        }
    }

    fn pane_creation_rollback_panic_error(message: &'static str) -> crate::Error {
        crate::Error::Wezterm(WeztermError::CommandFailed(message.to_string()))
    }

    async fn run_pane_creation_rollback_catching_panic(
        cleanup_cx: &crate::cx::Cx,
        handle: &WeztermHandle,
        local_pane_id: u64,
    ) -> Result<()> {
        let rollback = catch_recoverable_future(
            RecoverablePanicSite::ShardingRollback,
            Self::run_bounded_pane_creation_rollback(cleanup_cx, handle, local_pane_id),
        )
        .await;
        match rollback {
            Ok(result) => result,
            Err(_panic) => Err(Self::pane_creation_rollback_panic_error(
                PANE_CREATION_ROLLBACK_OPERATION_PANIC,
            )),
        }
    }

    async fn rollback_unencodable_pane_with_fresh_cx(
        backend: &ShardBackend,
        local_pane_id: u64,
    ) -> Result<()> {
        // Compensation must not inherit cancellation from the request that
        // already caused the creation result to become unreturnable. A
        // minimal cleanup budget plus an explicit wall-time ceiling keeps the
        // independent capability tightly bounded.
        let cleanup_cx = crate::cx::Cx::for_request_with_budget(crate::cx::Budget::MINIMAL);

        if let Some(runtime_handle) = crate::runtime_async::current_runtime_handle() {
            let cleanup_handle = backend.handle.clone();
            return match crate::cx::try_spawn_with_cx(
                &runtime_handle,
                &cleanup_cx,
                move |cleanup_cx| async move {
                    Self::run_pane_creation_rollback_catching_panic(
                        &cleanup_cx,
                        &cleanup_handle,
                        local_pane_id,
                    )
                    .await
                },
            ) {
                Ok(join) => {
                    // Normally this is structured: the creator awaits the
                    // compensator. If the creator future itself is dropped,
                    // dropping asupersync's JoinHandle detaches rather than
                    // aborts, so the independently bounded rollback survives.
                    match catch_recoverable_future(
                        RecoverablePanicSite::ShardingRollback,
                        join,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_panic) => Err(Self::pane_creation_rollback_panic_error(
                            PANE_CREATION_ROLLBACK_JOIN_PANIC,
                        )),
                    }
                }
                Err(_admission_error) => {
                    // Runtime shutdown or admission pressure can prevent task
                    // creation. Inline cleanup remains the strongest available
                    // fallback. If both paths fail, expose only a finite class;
                    // either underlying error may contain untrusted text.
                    Self::run_pane_creation_rollback_catching_panic(
                        &cleanup_cx,
                        &backend.handle,
                        local_pane_id,
                    )
                    .await
                    .map_err(|_cleanup_error| {
                        crate::Error::Wezterm(WeztermError::CommandFailed(
                            PANE_CREATION_ROLLBACK_ADMISSION_CLASS.to_owned(),
                        ))
                    })
                }
            };
        }

        // Compatibility fallback for callers polling this future outside the
        // FrankenTerm runtime. It is still caller-Cx-independent and bounded,
        // but cannot gain the runtime task's survive-parent-drop property.
        Self::run_pane_creation_rollback_catching_panic(
            &cleanup_cx,
            &backend.handle,
            local_pane_id,
        )
        .await
    }

    async fn encode_created_pane_or_rollback_after_cx_creation(
        backend: &ShardBackend,
        shard_id: ShardId,
        local_pane_id: u64,
        creation_op: &'static str,
    ) -> Result<u64> {
        Self::encode_created_pane_or_rollback(
            backend,
            shard_id,
            local_pane_id,
            creation_op,
        )
        .await
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.spawn_with_hints_with_cx(&cx, cwd, domain_name, agent_hint)
            .await
    }

    /// Spawn a new pane honoring shard-assignment hints, bound to the
    /// caller's asupersync capability context (ft-xbnl0.2.3 Cx-first
    /// entry point).
    ///
    /// Routes `cx` through subprocess I/O via
    /// [`WeztermInterface::spawn_with_cx`]. Once the backend has created a
    /// pane, the route-cache commit is deliberately synchronous and
    /// non-yielding: cancellation cannot strand a successfully created pane
    /// between the backend response and local route publication. If the
    /// backend returns an unencodable local id, rollback runs under a fresh
    /// minimal-budget Cx with a hard timeout; the normally joined compensator
    /// remains bounded but survives an abrupt drop of this creator future.
    pub async fn spawn_with_hints_with_cx(
        &self,
        cx: &crate::cx::Cx,
        cwd: Option<&str>,
        domain_name: Option<&str>,
        agent_hint: Option<AgentType>,
    ) -> Result<u64> {
        self.telemetry.spawns.fetch_add(1, Ordering::Relaxed);
        let shard = self.choose_spawn_shard(domain_name, agent_hint);
        let backend = self.backend_for_id(shard)?;
        let local_id = backend
            .handle
            .spawn_with_cx(cx, cwd, domain_name)
            .await
            .map_err(|err| Self::backend_error(shard, "spawn", None, err))?;
        let global_id = Self::encode_created_pane_or_rollback_after_cx_creation(
            backend,
            shard,
            local_id,
            "spawn",
        )
        .await?;
        self.insert_pane_route(
            global_id,
            PaneRoute {
                shard_id: shard,
                local_pane_id: local_id,
            },
        );
        Ok(global_id)
    }

    /// Aggregate panes across all shards and refresh the route index.
    ///
    /// A snapshot that overlaps a newer spawn/kill/navigation route update is
    /// returned to the caller but not published into the cache; the point
    /// update is newer routing truth and must not be overwritten.
    pub async fn list_all_panes(&self) -> Result<Vec<PaneInfo>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.list_all_panes_with_cx(&cx).await
    }

    /// Collect panes under the caller's cx. Uses each backend's
    /// `list_panes_with_cx(cx)` (added to `WeztermInterface` trait in
    /// tick 27 with a default impl that delegates to `list_panes` for
    /// backends without a Cx-aware path, and overridden by
    /// `WeztermClient` to propagate Cx through to
    /// `MuxPool::list_panes_with_cx`).
    async fn collect_panes_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<(Vec<PaneInfo>, HashMap<u64, PaneRoute>)> {
        let mut all = Vec::new();
        let mut routes = HashMap::new();

        for backend in &self.backends {
            let panes = backend
                .handle
                .list_panes_with_cx(cx)
                .await
                .map_err(|err| Self::backend_error(backend.id, "list_panes", None, err))?;

            for mut pane in panes {
                let local_pane_id = pane.pane_id;
                let global_pane_id = try_encode_sharded_pane_id(backend.id, local_pane_id)?;
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

    /// Aggregate panes across all shards, bound to the caller's
    /// asupersync capability context (ft-xbnl0.2.3 Cx-first entry
    /// point).
    ///
    /// Uses `collect_panes_with_cx(cx)` so per-backend `list_panes`
    /// calls flow through `WeztermInterface::list_panes_with_cx`
    /// (tick 27 trait extension); the concrete `WeztermClient` impl
    /// routes to `MuxPool::list_panes_with_cx` on the fast path. Snapshot
    /// publication is synchronous, generation-fenced, and non-yielding, so a
    /// cancellation observed after discovery cannot leave a half-refreshed
    /// cache or overwrite a newer point update.
    pub async fn list_all_panes_with_cx(&self, cx: &crate::cx::Cx) -> Result<Vec<PaneInfo>> {
        self.telemetry.pane_listings.fetch_add(1, Ordering::Relaxed);
        let route_generation = self.pane_route_generation();
        let (panes, routes) = self.collect_panes_with_cx(cx).await?;
        self.publish_pane_route_snapshot(route_generation, routes);
        Ok(panes)
    }

    /// Build a shard-level health report for watchdog integration.
    pub async fn shard_health_report(&self) -> ShardHealthReport {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.shard_health_report_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`shard_health_report`].
    ///
    /// Threads the caller's cx through each shard's `list_panes`
    /// call (via `list_panes_with_cx`) so a cancelled parent
    /// gets responsive shutdown across multi-shard topologies.
    /// Per-shard checkpoint between iterations lets a caller
    /// abort partway through a large fleet health scan and
    /// return a stable full-topology report whose typed outcomes identify the
    /// interrupted and not-started shards.
    pub async fn shard_health_report_with_cx(&self, cx: &crate::cx::Cx) -> ShardHealthReport {
        self.telemetry
            .health_reports
            .fetch_add(1, Ordering::Relaxed);
        // Materialize the complete, deterministically ordered topology before
        // any checkpoint. Cancellation must never erase shards from the report
        // or let an incomplete scan masquerade as healthy.
        let mut shards = self
            .backends
            .iter()
            .map(pending_shard_health_entry)
            .collect::<Vec<_>>();

        for (index, backend) in self.backends.iter().enumerate() {
            if cx.checkpoint().is_err() {
                break;
            }

            let circuit = match health_circuit_status(backend) {
                Ok(circuit) => circuit,
                Err(error) => {
                    shards[index] = completed_shard_health_entry(
                        backend,
                        CircuitBreakerStatus::default(),
                        Err(error),
                    );
                    continue;
                }
            };
            let panes = cx_health_panes(backend, cx).await;
            let cancelled = cx.is_cancel_requested();

            if cancelled {
                if panes.is_ok() {
                    let mut entry = completed_shard_health_entry(backend, circuit, panes);
                    // Keep the successfully observed pane count, but make the
                    // interrupted report state explicit and non-healthy.
                    entry.status = entry.status.max(HealthStatus::Degraded);
                    entry.probe_outcome = ShardHealthProbeOutcome::Cancelled;
                    shards[index] = entry;
                } else {
                    shards[index] = cancelled_shard_health_entry(backend, circuit);
                }
                break;
            }

            shards[index] = completed_shard_health_entry(backend, circuit, panes);
        }

        ShardHealthReport {
            timestamp_ms: now_epoch_ms(),
            overall: overall_shard_health(&shards),
            shards,
        }
    }

    /// Produce watchdog warning lines from current shard health.
    pub async fn shard_watchdog_warnings(&self) -> Vec<String> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.shard_watchdog_warnings_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`shard_watchdog_warnings`].
    pub async fn shard_watchdog_warnings_with_cx(&self, cx: &crate::cx::Cx) -> Vec<String> {
        self.shard_health_report_with_cx(cx)
            .await
            .watchdog_warnings()
    }

    fn route_for_global_pane_id(&self, pane_id: u64) -> Result<PaneRoute> {
        self.telemetry.route_lookups.fetch_add(1, Ordering::Relaxed);
        if let Some(route) = self.pane_routes.get(pane_id) {
            return Ok(route);
        }

        // Global pane ids are self-describing. Decoding the shard/local fields
        // is authoritative and avoids turning a cold keypress into sequential
        // `list_panes` I/O plus allocation across every configured backend.
        self.resolve_uncached_pane_route(pane_id)
    }

    /// Resolve a global pane_id to a `PaneRoute` bound to the caller's
    /// asupersync capability context (ft-xbnl0.2.3 Cx-first internal
    /// helper).
    ///
    /// Resolution is a synchronous decode of the self-describing global id;
    /// no backend discovery or blocking I/O is needed on a cold cache miss.
    fn route_for_global_pane_id_with_cx(
        &self,
        _cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<PaneRoute> {
        self.route_for_global_pane_id(pane_id)
    }

    fn resolve_uncached_pane_route(&self, pane_id: u64) -> Result<PaneRoute> {
        // A one-backend client historically accepts raw backend-local ids. Keep
        // that useful behavior, but only inside the actual 48-bit local field.
        // Larger values must be interpreted as encoded globals (or rejected),
        // never forwarded verbatim to the backend.
        if self.backends.len() == 1 && pane_id <= LOCAL_PANE_ID_MASK {
            return Ok(PaneRoute {
                shard_id: self.backends[0].id,
                local_pane_id: pane_id,
            });
        }

        let (decoded_shard, decoded_local) = try_decode_sharded_pane_id(pane_id)?;
        if self.backend_index.contains_key(&decoded_shard) {
            return Ok(PaneRoute {
                shard_id: decoded_shard,
                local_pane_id: decoded_local,
            });
        }

        Err(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)))
    }

    async fn route_for_window_id(&self, window_id: u64) -> Result<ShardId> {
        if self.backends.len() == 1 {
            return Ok(self.backends[0].id);
        }

        let panes = self.list_all_panes().await?;
        Self::resolve_window_shard(window_id, &panes)
    }

    /// Resolve a `window_id` to its owning shard bound to the
    /// caller's asupersync capability context (ft-xbnl0.2.3 Cx-first
    /// internal helper).
    ///
    /// Uses [`Self::list_all_panes_with_cx`] so the underlying per-backend
    /// `list_panes_with_cx` calls honor caller cancellation, budget, and
    /// virtual time. The completed route snapshot publishes synchronously.
    /// Shares the pure `resolve_window_shard` matcher with the legacy
    /// `route_for_window_id` so both variants produce bit-for-bit identical
    /// decisions for the same shard state.
    async fn route_for_window_id_with_cx(
        &self,
        cx: &crate::cx::Cx,
        window_id: u64,
    ) -> Result<ShardId> {
        if self.backends.len() == 1 {
            return Ok(self.backends[0].id);
        }

        let panes = self.list_all_panes_with_cx(cx).await?;
        Self::resolve_window_shard(window_id, &panes)
    }

    /// Pure window-to-shard matcher extracted from
    /// [`Self::route_for_window_id`] so legacy and Cx-first lookups
    /// share the same ambiguity + not-found semantics.
    fn resolve_window_shard(window_id: u64, panes: &[PaneInfo]) -> Result<ShardId> {
        let matching_shards: HashSet<_> = panes
            .iter()
            .filter(|pane| pane.window_id == window_id)
            .filter_map(|pane| {
                pane.extra
                    .get("shard_id")
                    .and_then(serde_json::Value::as_u64)
                    .map(|id| ShardId(id as usize))
            })
            .collect();

        match matching_shards.len() {
            1 => Ok(matching_shards.iter().copied().next().unwrap_or(ShardId(0))),
            0 => Err(crate::Error::Wezterm(WeztermError::CommandFailed(format!(
                "spawn_targeted failed: window {window_id} not found on any shard"
            )))),
            _ => Err(crate::Error::Wezterm(WeztermError::CommandFailed(format!(
                "spawn_targeted failed: window {window_id} is ambiguous across shards"
            )))),
        }
    }
}

impl WeztermInterface for ShardedWeztermClient {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        Box::pin(async move { self.list_all_panes().await })
    }

    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            let mut pane = backend
                .handle
                .get_pane(route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "get_pane", Some(pane_id), err)
                })?;
            pane.pane_id =
                try_encode_sharded_pane_id(route.shard_id, route.local_pane_id)?;
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
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .get_text(route.local_pane_id, escapes)
                .await
                .map_err(|err| Self::backend_error(route.shard_id, "get_text", Some(pane_id), err))
        })
    }

    fn get_semantic_zones(&self, pane_id: u64) -> WeztermFuture<'_, MuxSemanticSnapshot> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .get_semantic_zones(route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "get_semantic_zones", Some(pane_id), err)
                })
        })
    }

    fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text(route.local_pane_id, &text)
                .await
                .map_err(|err| Self::backend_error(route.shard_id, "send_text", Some(pane_id), err))
        })
    }

    fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text_no_paste(route.local_pane_id, &text)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "send_text_no_paste", Some(pane_id), err)
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
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text_with_options(route.local_pane_id, &text, no_paste, no_newline)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "send_text_with_options", Some(pane_id), err)
                })
        })
    }

    fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
        let control_char = control_char.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_control(route.local_pane_id, &control_char)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "send_control", Some(pane_id), err)
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

    fn spawn_targeted(
        &self,
        cwd: Option<&str>,
        domain_name: Option<&str>,
        target: SpawnTarget,
    ) -> WeztermFuture<'_, u64> {
        let cwd = cwd.map(ToString::to_string);
        let domain_name = domain_name.map(ToString::to_string);
        Box::pin(async move {
            self.telemetry.spawns.fetch_add(1, Ordering::Relaxed);
            let shard = if target.new_window || target.window_id.is_none() {
                self.choose_spawn_shard(domain_name.as_deref(), None)
            } else {
                match target.window_id {
                    Some(window_id) => self.route_for_window_id(window_id).await?,
                    None => self.choose_spawn_shard(domain_name.as_deref(), None),
                }
            };
            let backend = self.backend_for_id(shard)?;
            let local_id = backend
                .handle
                .spawn_targeted(cwd.as_deref(), domain_name.as_deref(), target)
                .await
                .map_err(|err| Self::backend_error(shard, "spawn_targeted", None, err))?;
            let global_id = Self::encode_created_pane_or_rollback(
                backend,
                shard,
                local_id,
                "spawn_targeted",
            )
            .await?;
            self.insert_pane_route(
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
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            let local_new = backend
                .handle
                .split_pane(route.local_pane_id, direction, cwd.as_deref(), percent)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "split_pane", Some(pane_id), err)
                })?;

            let global_new = Self::encode_created_pane_or_rollback(
                backend,
                route.shard_id,
                local_new,
                "split_pane",
            )
            .await?;
            self.insert_pane_route(
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
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .activate_pane(route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "activate_pane", Some(pane_id), err)
                })
        })
    }

    fn get_pane_direction(
        &self,
        pane_id: u64,
        direction: MoveDirection,
    ) -> WeztermFuture<'_, Option<u64>> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            let next_local = backend
                .handle
                .get_pane_direction(route.local_pane_id, direction)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "get_pane_direction", Some(pane_id), err)
                })?;

            if let Some(local_id) = next_local {
                let global_id = try_encode_sharded_pane_id(route.shard_id, local_id)?;
                self.insert_pane_route(
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
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .kill_pane(route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "kill_pane", Some(pane_id), err)
                })?;
            self.remove_pane_route(pane_id);
            Ok(())
        })
    }

    fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .zoom_pane(route.local_pane_id, zoom)
                .await
                .map_err(|err| Self::backend_error(route.shard_id, "zoom_pane", Some(pane_id), err))
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

    fn pane_tiered_scrollback_summary(
        &self,
        pane_id: u64,
    ) -> WeztermFuture<'_, PaneTieredScrollbackSummary> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id(pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .pane_tiered_scrollback_summary(route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(
                        route.shard_id,
                        "pane_tiered_scrollback_summary",
                        Some(pane_id),
                        err,
                    )
                })
        })
    }

    // --- ft-xbnl0.2.3: Cx-first overrides for ShardedWeztermClient ---
    //
    // Same rationale as tick 30 (Arc<dyn>) and tick 31 (UnifiedClient):
    // without explicit _with_cx overrides, calls fall through to the
    // trait default which loses cx at the ShardedWezterm hop. Each
    // override routes pane ids through `route_for_global_pane_id_with_cx`
    // (and targeted window ids through `route_for_window_id_with_cx`),
    // then forwards cx through `backend.handle.METHOD_with_cx(cx, ...)`.
    // The route-cache locks, backend discovery, and concrete inner path
    // (e.g. WeztermClient → MuxPool → DirectMuxClient) therefore all
    // share the caller's cancellation, budget, and virtual-time context.

    fn list_panes_with_cx<'a>(&'a self, cx: &'a crate::cx::Cx) -> WeztermFuture<'a, Vec<PaneInfo>> {
        Box::pin(async move { self.list_all_panes_with_cx(cx).await })
    }

    fn get_pane_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
    ) -> WeztermFuture<'a, PaneInfo> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            let mut pane = backend
                .handle
                .get_pane_with_cx(cx, route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "get_pane", Some(pane_id), err)
                })?;
            pane.pane_id =
                try_encode_sharded_pane_id(route.shard_id, route.local_pane_id)?;
            pane.extra
                .insert("shard_id".to_string(), Value::from(route.shard_id.0 as u64));
            pane.extra.insert(
                "local_pane_id".to_string(),
                Value::from(route.local_pane_id),
            );
            Ok(pane)
        })
    }

    fn get_text_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
        escapes: bool,
    ) -> WeztermFuture<'a, String> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .get_text_with_cx(cx, route.local_pane_id, escapes)
                .await
                .map_err(|err| Self::backend_error(route.shard_id, "get_text", Some(pane_id), err))
        })
    }

    fn get_semantic_zones_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
    ) -> WeztermFuture<'a, MuxSemanticSnapshot> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .get_semantic_zones_with_cx(cx, route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "get_semantic_zones", Some(pane_id), err)
                })
        })
    }

    fn send_text_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
        text: &str,
    ) -> WeztermFuture<'a, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text_with_cx(cx, route.local_pane_id, &text)
                .await
                .map_err(|err| Self::backend_error(route.shard_id, "send_text", Some(pane_id), err))
        })
    }

    fn send_text_no_paste_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
        text: &str,
    ) -> WeztermFuture<'a, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text_no_paste_with_cx(cx, route.local_pane_id, &text)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "send_text_no_paste", Some(pane_id), err)
                })
        })
    }

    fn send_text_with_options_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> WeztermFuture<'a, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_text_with_options_with_cx(
                    cx,
                    route.local_pane_id,
                    &text,
                    no_paste,
                    no_newline,
                )
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "send_text_with_options", Some(pane_id), err)
                })
        })
    }

    fn send_control_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
        control_char: &str,
    ) -> WeztermFuture<'a, ()> {
        let control_char = control_char.to_string();
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .send_control_with_cx(cx, route.local_pane_id, &control_char)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "send_control", Some(pane_id), err)
                })
        })
    }

    fn pane_tiered_scrollback_summary_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
    ) -> WeztermFuture<'a, PaneTieredScrollbackSummary> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .pane_tiered_scrollback_summary_with_cx(cx, route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(
                        route.shard_id,
                        "pane_tiered_scrollback_summary",
                        Some(pane_id),
                        err,
                    )
                })
        })
    }

    fn activate_pane_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
    ) -> WeztermFuture<'a, ()> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .activate_pane_with_cx(cx, route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "activate_pane", Some(pane_id), err)
                })
        })
    }

    fn kill_pane_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
    ) -> WeztermFuture<'a, ()> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .kill_pane_with_cx(cx, route.local_pane_id)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "kill_pane", Some(pane_id), err)
                })?;
            self.remove_pane_route(pane_id);
            Ok(())
        })
    }

    fn zoom_pane_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
        zoom: bool,
    ) -> WeztermFuture<'a, ()> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            backend
                .handle
                .zoom_pane_with_cx(cx, route.local_pane_id, zoom)
                .await
                .map_err(|err| Self::backend_error(route.shard_id, "zoom_pane", Some(pane_id), err))
        })
    }

    fn spawn_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        cwd: Option<&'a str>,
        domain_name: Option<&'a str>,
    ) -> WeztermFuture<'a, u64> {
        Box::pin(async move {
            self.spawn_with_hints_with_cx(cx, cwd, domain_name, None)
                .await
        })
    }

    fn spawn_targeted_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        cwd: Option<&'a str>,
        domain_name: Option<&'a str>,
        target: SpawnTarget,
    ) -> WeztermFuture<'a, u64> {
        Box::pin(async move {
            self.telemetry.spawns.fetch_add(1, Ordering::Relaxed);
            let shard = if target.new_window || target.window_id.is_none() {
                self.choose_spawn_shard(domain_name, None)
            } else {
                match target.window_id {
                    Some(window_id) => self.route_for_window_id_with_cx(cx, window_id).await?,
                    None => self.choose_spawn_shard(domain_name, None),
                }
            };
            let backend = self.backend_for_id(shard)?;
            let local_id = backend
                .handle
                .spawn_targeted_with_cx(cx, cwd, domain_name, target)
                .await
                .map_err(|err| Self::backend_error(shard, "spawn_targeted", None, err))?;
            let global_id = Self::encode_created_pane_or_rollback_after_cx_creation(
                backend,
                shard,
                local_id,
                "spawn_targeted",
            )
            .await?;
            self.insert_pane_route(
                global_id,
                PaneRoute {
                    shard_id: shard,
                    local_pane_id: local_id,
                },
            );
            Ok(global_id)
        })
    }

    fn split_pane_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        pane_id: u64,
        direction: SplitDirection,
        cwd: Option<&'a str>,
        percent: Option<u8>,
    ) -> WeztermFuture<'a, u64> {
        Box::pin(async move {
            let route = self.route_for_global_pane_id_with_cx(cx, pane_id)?;
            let backend = self.backend_for_id(route.shard_id)?;
            let local_new = backend
                .handle
                .split_pane_with_cx(cx, route.local_pane_id, direction, cwd, percent)
                .await
                .map_err(|err| {
                    Self::backend_error(route.shard_id, "split_pane", Some(pane_id), err)
                })?;

            let global_new = Self::encode_created_pane_or_rollback_after_cx_creation(
                backend,
                route.shard_id,
                local_new,
                "split_pane",
            )
            .await?;
            self.insert_pane_route(
                global_new,
                PaneRoute {
                    shard_id: route.shard_id,
                    local_pane_id: local_new,
                },
            );
            Ok(global_new)
        })
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
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
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
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    struct CreationBoundaryBackend {
        list_calls: AtomicUsize,
        ambient_kills: AtomicUsize,
        ambient_last_killed: AtomicU64,
        cx_kills: AtomicUsize,
        cx_last_killed: AtomicU64,
        cx_kills_with_cancelled_context: AtomicUsize,
        created_local_pane_id: u64,
        cancel_after_cx_creation: bool,
        cancel_during_health_probe: bool,
        fail_health_probe_with_hostile_error: bool,
        panic_health_probe: bool,
        panic_circuit_status: bool,
        fail_cleanup: bool,
        cleanup_panics_remaining: AtomicUsize,
    }

    impl CreationBoundaryBackend {
        const CLEANUP_FAILURE_SECRET: &str =
            "rollback-secret-sentinel-that-must-never-be-reflected";
        const OVERSIZED_LOCAL_PANE_ID: u64 = LOCAL_PANE_ID_MASK + 1;
        const VALID_LOCAL_PANE_ID: u64 = 41;

        const fn new(
            created_local_pane_id: u64,
            cancel_after_cx_creation: bool,
            fail_cleanup: bool,
        ) -> Self {
            Self {
                list_calls: AtomicUsize::new(0),
                ambient_kills: AtomicUsize::new(0),
                ambient_last_killed: AtomicU64::new(0),
                cx_kills: AtomicUsize::new(0),
                cx_last_killed: AtomicU64::new(0),
                cx_kills_with_cancelled_context: AtomicUsize::new(0),
                created_local_pane_id,
                cancel_after_cx_creation,
                cancel_during_health_probe: false,
                fail_health_probe_with_hostile_error: false,
                panic_health_probe: false,
                panic_circuit_status: false,
                fail_cleanup,
                cleanup_panics_remaining: AtomicUsize::new(0),
            }
        }

        const fn oversized(fail_cleanup: bool) -> Self {
            Self::new(Self::OVERSIZED_LOCAL_PANE_ID, false, fail_cleanup)
        }

        const fn cancel_after_valid_creation() -> Self {
            Self::new(Self::VALID_LOCAL_PANE_ID, true, false)
        }

        const fn cancel_after_oversized_creation() -> Self {
            Self::new(Self::OVERSIZED_LOCAL_PANE_ID, true, false)
        }

        const fn cancel_during_health_probe() -> Self {
            let mut backend = Self::new(Self::VALID_LOCAL_PANE_ID, false, false);
            backend.cancel_during_health_probe = true;
            backend
        }

        const fn hostile_health_failure() -> Self {
            let mut backend = Self::new(Self::VALID_LOCAL_PANE_ID, false, false);
            backend.fail_health_probe_with_hostile_error = true;
            backend
        }

        const fn panicking_health_probe() -> Self {
            let mut backend = Self::new(Self::VALID_LOCAL_PANE_ID, false, false);
            backend.panic_health_probe = true;
            backend
        }

        const fn panicking_circuit_status() -> Self {
            let mut backend = Self::new(Self::VALID_LOCAL_PANE_ID, false, false);
            backend.panic_circuit_status = true;
            backend
        }

        fn oversized_with_one_cleanup_panic() -> Self {
            let backend = Self::oversized(false);
            backend
                .cleanup_panics_remaining
                .store(1, Ordering::Relaxed);
            backend
        }

        fn cancel_after_creation_if_requested(&self, cx: &crate::cx::Cx) {
            if self.cancel_after_cx_creation {
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("injected cancellation after backend pane creation"),
                );
            }
        }

        fn cleanup_result(&self) -> Result<()> {
            if self.cleanup_panics_remaining.swap(0, Ordering::AcqRel) > 0 {
                std::panic::panic_any(String::from(
                    "rollback-secret-sentinel-that-must-never-be-reflected",
                ));
            }
            if self.fail_cleanup {
                Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                    format!(
                        "{}{}",
                        Self::CLEANUP_FAILURE_SECRET,
                        "x".repeat(64 * 1_024)
                    ),
                )))
            } else {
                Ok(())
            }
        }

        fn health_probe_result(&self) -> Result<Vec<PaneInfo>> {
            self.list_calls.fetch_add(1, Ordering::Relaxed);
            if self.panic_health_probe {
                std::panic::panic_any(String::from(
                    "health-panic-secret-sentinel-that-must-not-be-reflected",
                ));
            }
            if self.fail_health_probe_with_hostile_error {
                Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                    format!(
                        "health-error-secret-sentinel-{}",
                        "z".repeat(64 * 1_024)
                    ),
                )))
            } else {
                Ok(Vec::new())
            }
        }
    }

    impl WeztermInterface for CreationBoundaryBackend {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
            Box::pin(async { self.health_probe_result() })
        }

        fn list_panes_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
        ) -> WeztermFuture<'a, Vec<PaneInfo>> {
            Box::pin(async move {
                if self.cancel_during_health_probe {
                    cx.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("injected cancellation during shard health probe"),
                    );
                }
                self.health_probe_result()
            })
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
            Box::pin(async move { Err(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id))) })
        }

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> WeztermFuture<'_, String> {
            Box::pin(async { Ok(String::new()) })
        }

        fn send_text(&self, _pane_id: u64, _text: &str) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_text_no_paste(&self, _pane_id: u64, _text: &str) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_text_with_options(
            &self,
            _pane_id: u64,
            _text: &str,
            _no_paste: bool,
            _no_newline: bool,
        ) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_control(&self, _pane_id: u64, _control_char: &str) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_ctrl_c(&self, _pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_ctrl_d(&self, _pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn spawn(
            &self,
            _cwd: Option<&str>,
            _domain_name: Option<&str>,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async { Ok(self.created_local_pane_id) })
        }

        fn spawn_targeted(
            &self,
            _cwd: Option<&str>,
            _domain_name: Option<&str>,
            _target: SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async { Ok(self.created_local_pane_id) })
        }

        fn split_pane(
            &self,
            _pane_id: u64,
            _direction: SplitDirection,
            _cwd: Option<&str>,
            _percent: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async { Ok(self.created_local_pane_id) })
        }

        fn spawn_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
            _cwd: Option<&'a str>,
            _domain_name: Option<&'a str>,
        ) -> WeztermFuture<'a, u64> {
            Box::pin(async move {
                self.cancel_after_creation_if_requested(cx);
                Ok(self.created_local_pane_id)
            })
        }

        fn spawn_targeted_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
            _cwd: Option<&'a str>,
            _domain_name: Option<&'a str>,
            _target: SpawnTarget,
        ) -> WeztermFuture<'a, u64> {
            Box::pin(async move {
                self.cancel_after_creation_if_requested(cx);
                Ok(self.created_local_pane_id)
            })
        }

        fn split_pane_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
            _pane_id: u64,
            _direction: SplitDirection,
            _cwd: Option<&'a str>,
            _percent: Option<u8>,
        ) -> WeztermFuture<'a, u64> {
            Box::pin(async move {
                self.cancel_after_creation_if_requested(cx);
                Ok(self.created_local_pane_id)
            })
        }

        fn activate_pane(&self, _pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn get_pane_direction(
            &self,
            _pane_id: u64,
            _direction: MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            Box::pin(async { Ok(None) })
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                self.ambient_kills.fetch_add(1, Ordering::Relaxed);
                self.ambient_last_killed.store(pane_id, Ordering::Relaxed);
                self.cleanup_result()
            })
        }

        fn kill_pane_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
            pane_id: u64,
        ) -> WeztermFuture<'a, ()> {
            Box::pin(async move {
                // Force one scheduler boundary so the drop-safety regression
                // can discard the creator while this bounded compensator is
                // still in flight.
                crate::runtime_async::task::yield_now().await;
                self.cx_kills.fetch_add(1, Ordering::Relaxed);
                self.cx_last_killed.store(pane_id, Ordering::Relaxed);
                if cx.is_cancel_requested() {
                    self.cx_kills_with_cancelled_context
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(crate::Error::Wezterm(WeztermError::CommandFailed(
                        "rollback received the already-cancelled caller Cx".to_string(),
                    )));
                }
                self.cleanup_result()
            })
        }

        fn zoom_pane(&self, _pane_id: u64, _zoom: bool) -> WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn circuit_status(&self) -> CircuitBreakerStatus {
            if self.panic_circuit_status {
                std::panic::panic_any(String::from(
                    "circuit-panic-secret-sentinel-that-must-not-be-reflected",
                ));
            }
            CircuitBreakerStatus::default()
        }
    }

    fn oversized_creation_client(
        fail_cleanup: bool,
    ) -> (ShardedWeztermClient, Arc<CreationBoundaryBackend>) {
        let backend = Arc::new(CreationBoundaryBackend::oversized(fail_cleanup));
        let handle: WeztermHandle = backend.clone();
        let client = ShardedWeztermClient::new(
            vec![ShardBackend::new(ShardId(0), handle)],
            AssignmentStrategy::RoundRobin,
        )
        .unwrap();
        (client, backend)
    }

    fn cancelling_creation_client() -> (ShardedWeztermClient, Arc<CreationBoundaryBackend>) {
        let backend = Arc::new(CreationBoundaryBackend::cancel_after_valid_creation());
        let handle: WeztermHandle = backend.clone();
        let client = ShardedWeztermClient::new(
            vec![ShardBackend::new(ShardId(0), handle)],
            AssignmentStrategy::RoundRobin,
        )
        .unwrap();
        (client, backend)
    }

    fn cancelling_oversized_creation_client() -> (
        ShardedWeztermClient,
        Arc<CreationBoundaryBackend>,
    ) {
        let backend = Arc::new(CreationBoundaryBackend::cancel_after_oversized_creation());
        let handle: WeztermHandle = backend.clone();
        let client = ShardedWeztermClient::new(
            vec![ShardBackend::new(
                ShardId(0),
                handle,
            )],
            AssignmentStrategy::RoundRobin,
        )
        .unwrap();
        (client, backend)
    }

    fn seed_split_parent_route(client: &ShardedWeztermClient) -> u64 {
        let parent_id = try_encode_sharded_pane_id(ShardId(0), 7).unwrap();
        client.pane_routes.insert(
            parent_id,
            PaneRoute {
                shard_id: ShardId(0),
                local_pane_id: 7,
            },
        );
        parent_id
    }

    fn assert_unmasked_codec_error(error: &crate::Error) {
        let expected = try_encode_sharded_pane_id(
            ShardId(0),
            CreationBoundaryBackend::OVERSIZED_LOCAL_PANE_ID,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error.to_string(), expected);
    }

    fn assert_cx_cleanup(backend: &CreationBoundaryBackend) {
        assert_eq!(backend.ambient_kills.load(Ordering::Relaxed), 0);
        assert_eq!(backend.cx_kills.load(Ordering::Relaxed), 1);
        assert_eq!(
            backend
                .cx_kills_with_cancelled_context
                .load(Ordering::Relaxed),
            0,
            "rollback must use a fresh cleanup Cx, never the cancelled caller Cx"
        );
        assert_eq!(
            backend.cx_last_killed.load(Ordering::Relaxed),
            CreationBoundaryBackend::OVERSIZED_LOCAL_PANE_ID
        );
    }

    fn assert_cancelled_cx_rollback(
        caller_cx: &crate::cx::Cx,
        backend: &CreationBoundaryBackend,
        error: &crate::Error,
    ) {
        assert!(
            caller_cx.is_cancel_requested(),
            "creation backend must cancel the caller Cx before returning its invalid id"
        );
        assert_unmasked_codec_error(error);
        assert_cx_cleanup(backend);
    }

    fn assert_post_backend_cancellation_commit(
        cx: &crate::cx::Cx,
        client: &ShardedWeztermClient,
        backend: &CreationBoundaryBackend,
        global_pane_id: u64,
    ) {
        assert!(
            cx.is_cancel_requested(),
            "the backend must inject cancellation immediately before returning success"
        );
        assert_eq!(
            global_pane_id,
            try_encode_sharded_pane_id(ShardId(0), backend.created_local_pane_id).unwrap()
        );
        let route = client
            .pane_routes
            .get(global_pane_id)
            .expect("successful backend creation must publish its route despite cancellation");
        assert_eq!(route.shard_id, ShardId(0));
        assert_eq!(route.local_pane_id, backend.created_local_pane_id);
        assert_eq!(backend.ambient_kills.load(Ordering::Relaxed), 0);
        assert_eq!(backend.cx_kills.load(Ordering::Relaxed), 0);
        assert_eq!(
            backend
                .cx_kills_with_cancelled_context
                .load(Ordering::Relaxed),
            0
        );
    }

    fn assert_dual_codec_cleanup_error(
        error: &crate::Error,
        creation_op: &str,
        cleanup_op: &str,
    ) {
        let rendered = error.to_string();
        let primary = try_encode_sharded_pane_id(
            ShardId(0),
            CreationBoundaryBackend::OVERSIZED_LOCAL_PANE_ID,
        )
        .unwrap_err()
        .to_string();
        assert!(
            rendered.starts_with(&primary),
            "primary codec error must remain the leading error: {rendered}"
        );
        assert!(
            rendered.ends_with("on shard 0 (cleanup_failed)"),
            "cleanup failure must retain only bounded classified evidence: {rendered}"
        );
        let codec_position = rendered
            .find("local pane id")
            .expect("primary pane-id codec error must remain present");
        let cleanup_position = rendered
            .find("best-effort rollback")
            .expect("cleanup failure context must be present");
        assert!(
            codec_position < cleanup_position,
            "primary codec error must precede cleanup evidence: {rendered}"
        );
        assert!(rendered.contains(&format!("rollback after {creation_op} failed")));
        assert!(rendered.contains(cleanup_op));
        assert!(rendered.contains(&format!(
            "local pane {}",
            CreationBoundaryBackend::OVERSIZED_LOCAL_PANE_ID
        )));
        assert!(!rendered.contains("backend-secret-sentinel"));
        assert!(!rendered.contains(CreationBoundaryBackend::CLEANUP_FAILURE_SECRET));
        assert!(
            rendered.len() < 512,
            "classified rollback diagnostics must remain bounded: {} bytes",
            rendered.len()
        );
    }

    #[test]
    fn encode_decode_roundtrip() {
        let shard = ShardId(37);
        let local = 0x0000_FFFF_FFFF_u64;
        let encoded = encode_sharded_pane_id(shard, local);
        let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
        assert_eq!(decoded_shard, shard);
        assert_eq!(decoded_local, local);
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
                    ShardBackend::new(ShardId(0), handle0),
                    ShardBackend::new(ShardId(1), handle1),
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
    fn cold_global_pane_routes_decode_without_backend_discovery() {
        run_async_test(async {
            let shard0 = Arc::new(CreationBoundaryBackend::new(11, false, false));
            let shard1 = Arc::new(CreationBoundaryBackend::new(22, false, false));
            let shard0_handle: WeztermHandle = shard0.clone();
            let shard1_handle: WeztermHandle = shard1.clone();
            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), shard0_handle),
                    ShardBackend::new(ShardId(1), shard1_handle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();
            let pane_id = try_encode_sharded_pane_id(ShardId(1), 22).unwrap();

            let route = client.route_for_global_pane_id(pane_id).unwrap();
            assert_eq!(route.shard_id, ShardId(1));
            assert_eq!(route.local_pane_id, 22);
            assert_eq!(shard0.list_calls.load(Ordering::Relaxed), 0);
            assert_eq!(shard1.list_calls.load(Ordering::Relaxed), 0);
            assert!(
                !client.pane_routes.contains(pane_id),
                "decoded misses must not let arbitrary valid ids grow the route cache"
            );

            let cx_shard0 = Arc::new(CreationBoundaryBackend::new(11, false, false));
            let cx_shard1 = Arc::new(CreationBoundaryBackend::new(22, false, false));
            let cx_shard0_handle: WeztermHandle = cx_shard0.clone();
            let cx_shard1_handle: WeztermHandle = cx_shard1.clone();
            let cx_client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), cx_shard0_handle),
                    ShardBackend::new(ShardId(1), cx_shard1_handle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();
            let cx = crate::cx::for_request();
            let cx_route = cx_client
                .route_for_global_pane_id_with_cx(&cx, pane_id)
                .unwrap();
            assert_eq!(cx_route, route);
            assert_eq!(cx_shard0.list_calls.load(Ordering::Relaxed), 0);
            assert_eq!(cx_shard1.list_calls.load(Ordering::Relaxed), 0);
            assert!(
                !cx_client.pane_routes.contains(pane_id),
                "Cx-first decoded misses must not grow the route cache"
            );
        });
    }

    #[test]
    fn stale_route_snapshots_cannot_overwrite_concurrent_point_mutations() {
        let backend: WeztermHandle = Arc::new(CreationBoundaryBackend::new(11, false, false));
        let client = ShardedWeztermClient::new(
            vec![ShardBackend::new(ShardId(0), backend)],
            AssignmentStrategy::RoundRobin,
        )
        .unwrap();

        let spawned_id = try_encode_sharded_pane_id(ShardId(0), 11).unwrap();
        let before_spawn = client.pane_route_generation();
        client.insert_pane_route(
            spawned_id,
            PaneRoute {
                shard_id: ShardId(0),
                local_pane_id: 11,
            },
        );
        assert!(
            !client.publish_pane_route_snapshot(before_spawn, HashMap::new()),
            "snapshot collected before spawn must be rejected"
        );
        assert!(
            client.pane_routes.contains(spawned_id),
            "stale snapshot must not erase a concurrently published spawn route"
        );

        let killed_id = try_encode_sharded_pane_id(ShardId(0), 12).unwrap();
        assert!(
            !client.pane_routes.contains(killed_id),
            "kill regression requires an uncached, directly decodable route"
        );
        let before_kill = client.pane_route_generation();
        client.remove_pane_route(killed_id);
        let stale_route = HashMap::from([(
            killed_id,
            PaneRoute {
                shard_id: ShardId(0),
                local_pane_id: 12,
            },
        )]);
        assert!(
            !client.publish_pane_route_snapshot(before_kill, stale_route),
            "snapshot collected before kill must be rejected"
        );
        assert!(
            !client.pane_routes.contains(killed_id),
            "stale snapshot must not publish an uncached route after a successful kill"
        );

        let exhausted_id = try_encode_sharded_pane_id(ShardId(0), 13).unwrap();
        client
            .pane_route_generation
            .store(u64::MAX, Ordering::Release);
        client.insert_pane_route(
            exhausted_id,
            PaneRoute {
                shard_id: ShardId(0),
                local_pane_id: 13,
            },
        );
        assert_eq!(client.pane_route_generation(), u64::MAX);
        assert!(
            !client.publish_pane_route_snapshot(u64::MAX, HashMap::new()),
            "an exhausted generation must reject every full snapshot instead of wrapping"
        );
        assert!(
            client.pane_routes.contains(exhausted_id),
            "generation exhaustion must preserve newer point-mutation routing truth"
        );
        assert_eq!(
            client.telemetry().snapshot().route_snapshot_conflicts,
            3
        );
    }

    #[test]
    fn list_all_panes_replaces_stale_route_generation() {
        run_async_test(async {
            let shard = Arc::new(MockWezterm::new());
            shard.add_default_pane(7).await;
            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(
                    ShardId(0),
                    shard as WeztermHandle,
                )],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();
            let stale_pane_id = try_encode_sharded_pane_id(ShardId(0), 999).unwrap();
            client.pane_routes.insert(
                stale_pane_id,
                PaneRoute {
                    shard_id: ShardId(0),
                    local_pane_id: 999,
                },
            );

            let panes = client.list_all_panes().await.unwrap();
            assert_eq!(panes.len(), 1);
            assert!(client.pane_routes.contains(panes[0].pane_id));
            assert!(
                !client.pane_routes.contains(stale_pane_id),
                "whole-generation refresh must remove routes absent from discovery"
            );
            assert_eq!(client.pane_routes.len(), 1);
        });
    }

    #[test]
    fn list_panes_rejects_backend_local_id_outside_encoded_domain() {
        run_async_test(async {
            let shard = Arc::new(MockWezterm::new());
            shard.add_default_pane(LOCAL_PANE_ID_MASK + 1).await;
            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(
                    ShardId(0),
                    shard as WeztermHandle,
                )],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let err = client.list_panes().await.unwrap_err();
            assert!(
                err.to_string().contains("local pane id"),
                "unexpected error: {err}"
            );
            assert!(err.to_string().contains("48-bit encoded capacity"));
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
                    ShardBackend::new(ShardId(0), handle0),
                    ShardBackend::new(ShardId(1), handle1),
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
                    ShardBackend::new(ShardId(0), handle0),
                    ShardBackend::new(ShardId(1), handle1),
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

    /// ft-xbnl0.2.3 Cx-first: `spawn_with_hints_with_cx` must route
    /// through `backend.handle.spawn_with_cx` (tick 47 upgrade — the
    /// trait gained spawn_with_cx in tick 46, so the internal call
    /// previously left as ambient `backend.handle.spawn()` is now
    /// cx-aware). Mirrors `spawn_with_agent_hint_uses_agent_assignment`
    /// but drives the Cx-first entry point with a fresh `Cx`,
    /// asserting the agent-hint routing logic + pane route insertion
    /// work end-to-end when cx is threaded through the subprocess hop and the
    /// resulting route is committed synchronously after backend success.
    #[test]
    fn spawn_with_hints_with_cx_routes_and_records() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            let shard1 = Arc::new(MockWezterm::new());
            let handle0: WeztermHandle = shard0.clone();
            let handle1: WeztermHandle = shard1.clone();

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), handle0),
                    ShardBackend::new(ShardId(1), handle1),
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

            let cx = crate::cx::for_request();
            let pane = client
                .spawn_with_hints_with_cx(&cx, None, None, Some(AgentType::Codex))
                .await
                .unwrap();

            // Agent-hint routed to shard 1 (Codex assignment).
            assert_eq!(
                decode_sharded_pane_id(pane),
                (ShardId(1), 0),
                "Codex agent hint should route to shard 1"
            );
            assert_eq!(shard0.pane_count().await, 0);
            assert_eq!(shard1.pane_count().await, 1);

            // Pane route recorded for subsequent lookups.
            let recorded = client
                .pane_routes
                .get(pane)
                .expect("pane route must be recorded");
            assert_eq!(recorded.shard_id, ShardId(1));
            assert_eq!(recorded.local_pane_id, 0);
        });
    }

    #[test]
    fn spawn_targeted_routes_existing_window_to_matching_shard() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            let shard1 = Arc::new(MockWezterm::new());
            shard0
                .add_pane(crate::wezterm::MockPane {
                    pane_id: 10,
                    window_id: 41,
                    tab_id: 0,
                    title: "existing".to_string(),
                    domain: "local".to_string(),
                    cwd: "/tmp".to_string(),
                    is_active: false,
                    is_zoomed: false,
                    cols: 80,
                    rows: 24,
                    content: String::new(),
                })
                .await;
            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), shard0.clone() as WeztermHandle),
                    ShardBackend::new(ShardId(1), shard1.clone() as WeztermHandle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let spawned = client
                .spawn_targeted(
                    None,
                    None,
                    SpawnTarget {
                        window_id: Some(41),
                        new_window: false,
                    },
                )
                .await
                .unwrap();

            assert_eq!(decode_sharded_pane_id(spawned).0, ShardId(0));
            assert_eq!(shard0.pane_count().await, 2);
            assert_eq!(shard1.pane_count().await, 0);
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `spawn_targeted_with_cx` with a
    /// `window_id` hint must route through
    /// `route_for_window_id_with_cx` (tick 48 helper) so the underlying
    /// `list_panes_with_cx` calls propagate cx through backend I/O before the
    /// route snapshot publishes synchronously. Mirrors the legacy
    /// `spawn_targeted_routes_existing_window_to_matching_shard`
    /// topology exactly — if tick 48's Cx-first window-routing
    /// regressed the matcher semantics (matching_shards set
    /// construction, 0/1/many branches), this test would diverge
    /// from the legacy one.
    #[test]
    fn spawn_targeted_with_cx_routes_existing_window_to_matching_shard() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            let shard1 = Arc::new(MockWezterm::new());
            shard0
                .add_pane(crate::wezterm::MockPane {
                    pane_id: 10,
                    window_id: 41,
                    tab_id: 0,
                    title: "existing".to_string(),
                    domain: "local".to_string(),
                    cwd: "/tmp".to_string(),
                    is_active: false,
                    is_zoomed: false,
                    cols: 80,
                    rows: 24,
                    content: String::new(),
                })
                .await;
            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), shard0.clone() as WeztermHandle),
                    ShardBackend::new(ShardId(1), shard1.clone() as WeztermHandle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let cx = crate::cx::for_request();
            let spawned = client
                .spawn_targeted_with_cx(
                    &cx,
                    None,
                    None,
                    SpawnTarget {
                        window_id: Some(41),
                        new_window: false,
                    },
                )
                .await
                .unwrap();

            // window_id=41 exists only on shard 0 → new pane must
            // land there. Note: the local pane_id is whatever Mock
            // assigned (starts from 0), so we only check the shard
            // matches. shard_count assertions below pin the full
            // routing behavior.
            assert_eq!(decode_sharded_pane_id(spawned).0, ShardId(0));
            assert_eq!(shard0.pane_count().await, 2);
            assert_eq!(shard1.pane_count().await, 0);

            assert!(
                client.pane_routes.contains(spawned),
                "Cx-first spawn_targeted must record new pane in pane_routes"
            );
        });
    }

    #[test]
    fn spawn_targeted_rejects_ambiguous_window_id_across_shards() {
        run_async_test(async {
            let shard0 = Arc::new(MockWezterm::new());
            let shard1 = Arc::new(MockWezterm::new());
            for handle in [&shard0, &shard1] {
                handle
                    .add_pane(crate::wezterm::MockPane {
                        pane_id: 10,
                        window_id: 7,
                        tab_id: 0,
                        title: "existing".to_string(),
                        domain: "local".to_string(),
                        cwd: "/tmp".to_string(),
                        is_active: false,
                        is_zoomed: false,
                        cols: 80,
                        rows: 24,
                        content: String::new(),
                    })
                    .await;
            }

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), shard0 as WeztermHandle),
                    ShardBackend::new(ShardId(1), shard1 as WeztermHandle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let err = client
                .spawn_targeted(
                    None,
                    None,
                    SpawnTarget {
                        window_id: Some(7),
                        new_window: false,
                    },
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("ambiguous across shards"));
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
    #[should_panic(expected = "exceeds 15-bit persistence-safe encoded capacity")]
    fn encode_shard_overflow_panics() {
        let _ = encode_sharded_pane_id(ShardId(MAX_SHARD_ID + 1), 42);
    }

    #[test]
    fn checked_codec_rejects_every_out_of_domain_boundary() {
        assert!(
            try_encode_sharded_pane_id(ShardId(MAX_SHARD_ID + 1), 0).is_err(),
            "the reserved sign-bit shard range must fail closed"
        );
        assert!(
            try_encode_sharded_pane_id(ShardId(0), LOCAL_PANE_ID_MASK + 1).is_err(),
            "oversized local ids must never be silently truncated"
        );
        assert!(
            try_decode_sharded_pane_id(MAX_GLOBAL_PANE_ID + 1).is_err(),
            "negative-as-i64 global ids must not enter routing"
        );
    }

    #[test]
    fn maximum_encoded_id_is_exactly_sqlite_i64_max() {
        let encoded =
            try_encode_sharded_pane_id(ShardId(MAX_SHARD_ID), LOCAL_PANE_ID_MASK).unwrap();
        assert_eq!(encoded, MAX_GLOBAL_PANE_ID);
        assert_eq!(i64::try_from(encoded).unwrap(), i64::MAX);
        assert_eq!(
            try_decode_sharded_pane_id(encoded).unwrap(),
            (ShardId(MAX_SHARD_ID), LOCAL_PANE_ID_MASK)
        );
    }

    #[test]
    fn ambient_creation_paths_use_bounded_fresh_cx_rollback() {
        run_async_test(async {
            let (spawn_client, spawn_backend) = oversized_creation_client(false);
            let spawn_error = spawn_client
                .spawn_with_hints(None, None, None)
                .await
                .unwrap_err();
            assert_unmasked_codec_error(&spawn_error);
            assert_cx_cleanup(&spawn_backend);

            let (targeted_client, targeted_backend) = oversized_creation_client(false);
            let targeted_error = targeted_client
                .spawn_targeted(
                    None,
                    None,
                    SpawnTarget {
                        window_id: None,
                        new_window: true,
                    },
                )
                .await
                .unwrap_err();
            assert_unmasked_codec_error(&targeted_error);
            assert_cx_cleanup(&targeted_backend);

            let (split_client, split_backend) = oversized_creation_client(false);
            let parent_id = seed_split_parent_route(&split_client);
            let split_error = split_client
                .split_pane(parent_id, SplitDirection::Right, None, None)
                .await
                .unwrap_err();
            assert_unmasked_codec_error(&split_error);
            assert_cx_cleanup(&split_backend);
        });
    }

    #[test]
    fn cx_creation_paths_rollback_unencodable_panes_with_cx() {
        run_async_test(async {
            let cx = crate::cx::for_request();
            let (spawn_client, spawn_backend) = oversized_creation_client(false);
            let spawn_error = spawn_client
                .spawn_with_hints_with_cx(&cx, None, None, None)
                .await
                .unwrap_err();
            assert_unmasked_codec_error(&spawn_error);
            assert_cx_cleanup(&spawn_backend);

            let (targeted_client, targeted_backend) = oversized_creation_client(false);
            let targeted_error = targeted_client
                .spawn_targeted_with_cx(
                    &cx,
                    None,
                    None,
                    SpawnTarget {
                        window_id: None,
                        new_window: true,
                    },
                )
                .await
                .unwrap_err();
            assert_unmasked_codec_error(&targeted_error);
            assert_cx_cleanup(&targeted_backend);

            let (split_client, split_backend) = oversized_creation_client(false);
            let parent_id = seed_split_parent_route(&split_client);
            let split_error = split_client
                .split_pane_with_cx(&cx, parent_id, SplitDirection::Right, None, None)
                .await
                .unwrap_err();
            assert_unmasked_codec_error(&split_error);
            assert_cx_cleanup(&split_backend);
        });
    }

    #[test]
    fn cx_unencodable_creation_rollback_uses_fresh_bounded_cleanup_context() {
        run_async_test(async {
            let spawn_cx = crate::cx::for_request();
            let (spawn_client, spawn_backend) = cancelling_oversized_creation_client();
            let spawn_error = spawn_client
                .spawn_with_hints_with_cx(&spawn_cx, None, None, None)
                .await
                .expect_err("oversized pane id must fail after bounded compensation");
            assert_cancelled_cx_rollback(&spawn_cx, &spawn_backend, &spawn_error);

            let targeted_cx = crate::cx::for_request();
            let (targeted_client, targeted_backend) = cancelling_oversized_creation_client();
            let targeted_error = targeted_client
                .spawn_targeted_with_cx(
                    &targeted_cx,
                    None,
                    None,
                    SpawnTarget {
                        window_id: None,
                        new_window: true,
                    },
                )
                .await
                .expect_err("targeted oversized pane id must be compensated");
            assert_cancelled_cx_rollback(
                &targeted_cx,
                &targeted_backend,
                &targeted_error,
            );

            let split_cx = crate::cx::for_request();
            let (split_client, split_backend) = cancelling_oversized_creation_client();
            let parent_id = seed_split_parent_route(&split_client);
            let split_error = split_client
                .split_pane_with_cx(&split_cx, parent_id, SplitDirection::Right, None, None)
                .await
                .expect_err("split oversized pane id must be compensated");
            assert_cancelled_cx_rollback(&split_cx, &split_backend, &split_error);
        });
    }

    #[test]
    fn cx_bounded_unencodable_creation_rollback_survives_creator_future_drop() {
        run_async_test(async {
            let caller_cx = crate::cx::for_request();
            let (client, backend) = cancelling_oversized_creation_client();
            let mut creation = Box::pin(client.spawn_with_hints_with_cx(
                &caller_cx,
                None,
                None,
                None,
            ));

            assert!(
                futures::poll!(creation.as_mut()).is_pending(),
                "creator must be waiting on the independently spawned compensator"
            );
            assert!(caller_cx.is_cancel_requested());
            drop(creation);

            for _ in 0..32 {
                if backend.cx_kills.load(Ordering::Acquire) == 1 {
                    break;
                }
                crate::runtime_async::task::yield_now().await;
            }
            assert_cx_cleanup(&backend);
        });
    }

    #[test]
    fn ambient_bounded_unencodable_creation_rollback_survives_creator_future_drop() {
        run_async_test(async {
            let (client, backend) = oversized_creation_client(false);
            let mut creation = Box::pin(client.spawn_with_hints(None, None, None));

            assert!(
                futures::poll!(creation.as_mut()).is_pending(),
                "ambient creator must be waiting on the independently spawned compensator"
            );
            drop(creation);

            for _ in 0..32 {
                if backend.cx_kills.load(Ordering::Acquire) == 1 {
                    break;
                }
                crate::runtime_async::task::yield_now().await;
            }
            assert_cx_cleanup(&backend);
        });
    }

    #[test]
    fn cx_creation_routes_commit_when_backend_cancels_before_returning_success() {
        run_async_test(async {
            let spawn_cx = crate::cx::for_request();
            let (spawn_client, spawn_backend) = cancelling_creation_client();
            let spawned = spawn_client
                .spawn_with_hints_with_cx(&spawn_cx, None, None, None)
                .await
                .expect("post-success cancellation must not strand an unrecorded pane");
            assert_post_backend_cancellation_commit(
                &spawn_cx,
                &spawn_client,
                &spawn_backend,
                spawned,
            );

            let targeted_cx = crate::cx::for_request();
            let (targeted_client, targeted_backend) = cancelling_creation_client();
            let targeted = targeted_client
                .spawn_targeted_with_cx(
                    &targeted_cx,
                    None,
                    None,
                    SpawnTarget {
                        window_id: None,
                        new_window: true,
                    },
                )
                .await
                .expect("targeted creation must commit after backend success");
            assert_post_backend_cancellation_commit(
                &targeted_cx,
                &targeted_client,
                &targeted_backend,
                targeted,
            );

            let split_cx = crate::cx::for_request();
            let (split_client, split_backend) = cancelling_creation_client();
            let parent_id = seed_split_parent_route(&split_client);
            let split = split_client
                .split_pane_with_cx(&split_cx, parent_id, SplitDirection::Right, None, None)
                .await
                .expect("split creation must commit after backend success");
            assert_post_backend_cancellation_commit(
                &split_cx,
                &split_client,
                &split_backend,
                split,
            );
        });
    }

    #[test]
    fn creation_codec_errors_retain_cleanup_failure_evidence() {
        run_async_test(async {
            let (ambient_client, ambient_backend) = oversized_creation_client(true);
            let ambient_error = ambient_client
                .spawn_with_hints(None, None, None)
                .await
                .unwrap_err();
            assert_dual_codec_cleanup_error(
                &ambient_error,
                "spawn",
                "kill_pane_with_fresh_cleanup_cx",
            );
            assert_cx_cleanup(&ambient_backend);

            let cx = crate::cx::for_request();
            let (cx_client, cx_backend) = oversized_creation_client(true);
            let cx_error = cx_client
                .spawn_targeted_with_cx(
                    &cx,
                    None,
                    None,
                    SpawnTarget {
                        window_id: None,
                        new_window: true,
                    },
                )
                .await
                .unwrap_err();
            assert_dual_codec_cleanup_error(
                &cx_error,
                "spawn_targeted",
                "kill_pane_with_fresh_cleanup_cx",
            );
            assert_cx_cleanup(&cx_backend);
        });
    }

    #[test]
    fn rollback_admission_diagnostics_use_only_finite_classes() {
        let backend = ShardBackend::new(
            ShardId(0),
            Arc::new(MockWezterm::new()) as WeztermHandle,
        );
        let hostile_cleanup = crate::Error::Wezterm(WeztermError::CommandFailed(format!(
            "admission-secret-sentinel:{}:cleanup-secret-sentinel:{}",
            "a".repeat(64 * 1_024),
            "b".repeat(64 * 1_024)
        )));
        let error = ShardedWeztermClient::codec_error_with_cleanup_failure(
            "spawn",
            "kill_pane_with_fresh_cleanup_cx",
            &backend,
            CreationBoundaryBackend::OVERSIZED_LOCAL_PANE_ID,
            &hostile_cleanup,
        );
        let rendered = error.to_string();

        assert!(rendered.contains("cleanup_failed"));
        assert!(!rendered.contains("admission-secret-sentinel"));
        assert!(!rendered.contains("cleanup-secret-sentinel"));
        assert!(rendered.len() < 512);

        let classified_admission = crate::Error::Wezterm(WeztermError::CommandFailed(
            PANE_CREATION_ROLLBACK_ADMISSION_CLASS.to_owned(),
        ));
        let classified = ShardedWeztermClient::codec_error_with_cleanup_failure(
            "spawn",
            "kill_pane_with_fresh_cleanup_cx",
            &backend,
            CreationBoundaryBackend::OVERSIZED_LOCAL_PANE_ID,
            &classified_admission,
        )
        .to_string();
        assert!(classified.contains(PANE_CREATION_ROLLBACK_ADMISSION_CLASS));
        assert!(classified.len() < 512);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn rollback_panic_is_bounded_nonreflecting_and_later_cleanup_recovers() {
        run_async_test(async {
            let backend = Arc::new(CreationBoundaryBackend::oversized_with_one_cleanup_panic());
            let handle: WeztermHandle = backend.clone();
            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), handle)],
                AssignmentStrategy::RoundRobin,
            )
            .expect("construct panic-boundary sharded client");

            let first_error = client
                .spawn_with_hints(None, None, None)
                .await
                .expect_err("the unencodable pane and panicking rollback must fail closed");
            let first_rendered = first_error.to_string();
            assert!(first_rendered.contains("WA-SHARDING-ROLLBACK-PANIC"));
            assert!(first_rendered.contains("rollback operation panicked"));
            assert!(
                !first_rendered.contains("rollback-secret-sentinel"),
                "panic payload text must never reach the returned error: {first_rendered}"
            );

            let second_error = client
                .spawn_with_hints(None, None, None)
                .await
                .expect_err("the second unencodable pane still fails after successful cleanup");
            assert_unmasked_codec_error(&second_error);
            assert_eq!(backend.ambient_kills.load(Ordering::Relaxed), 0);
            assert_eq!(backend.cx_kills.load(Ordering::Relaxed), 2);
            assert_eq!(
                backend.cx_last_killed.load(Ordering::Relaxed),
                CreationBoundaryBackend::OVERSIZED_LOCAL_PANE_ID
            );
        });
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
                status: HealthStatus::Healthy,
                pane_count: Some(3),
                circuit: CircuitBreakerStatus::default(),
                probe_outcome: ShardHealthProbeOutcome::Complete,
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
                    status: HealthStatus::Healthy,
                    pane_count: Some(3),
                    circuit: CircuitBreakerStatus::default(),
                    probe_outcome: ShardHealthProbeOutcome::Complete,
                },
                ShardHealthEntry {
                    shard_id: ShardId(1),
                    status: HealthStatus::Degraded,
                    pane_count: None,
                    circuit: CircuitBreakerStatus::default(),
                    probe_outcome: ShardHealthProbeOutcome::Failed(
                        ShardBackendErrorClass::Other,
                    ),
                },
            ],
        };
        let unhealthy = report.unhealthy_shards();
        assert_eq!(unhealthy.len(), 1);
        assert_eq!(unhealthy[0].shard_id, ShardId(1));

        let warnings = report.watchdog_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Shard 1 unhealthy"));
        assert!(warnings[0].contains("probe=other"));
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
        assert_eq!(back.outcome(), ShardHealthReportOutcome::Complete);
        assert!(json.contains("\"outcome\":\"complete\""));
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
                    ShardBackend::new(ShardId(0), healthy_handle),
                    ShardBackend::new(ShardId(1), failing_handle),
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
            assert_eq!(healthy_entry.probe_outcome, ShardHealthProbeOutcome::Complete);

            let failing_entry = report
                .shards
                .iter()
                .find(|entry| entry.shard_id == ShardId(1))
                .unwrap();
            assert_eq!(failing_entry.status, HealthStatus::Hung);
            assert_eq!(failing_entry.pane_count, None);
            assert_eq!(
                failing_entry.probe_outcome,
                ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::CommandFailed)
            );

            let warnings = report.watchdog_warnings();
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("Shard 1 unhealthy"));

            let trait_warnings = client.watchdog_warnings().await.unwrap();
            assert_eq!(trait_warnings.len(), 1);
            assert!(trait_warnings[0].contains("Shard 1 unhealthy"));
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `shard_health_report_with_cx` must
    /// produce an equivalent report to `shard_health_report` on
    /// a 2-shard topology (healthy + failing), returning the
    /// same overall status, shard count, and watchdog warnings.
    #[test]
    fn shard_health_report_with_cx_matches_legacy() {
        run_async_test(async {
            let healthy = Arc::new(MockWezterm::new());
            healthy.add_default_pane(1).await;

            let healthy_handle: WeztermHandle = healthy.clone();
            let failing_handle: WeztermHandle = crate::wezterm::mock_wezterm_handle_failing();

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), healthy_handle),
                    ShardBackend::new(ShardId(1), failing_handle),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let cx = crate::cx::for_testing();
            let report = client.shard_health_report_with_cx(&cx).await;

            assert_eq!(report.shards.len(), 2);
            assert_eq!(report.overall, HealthStatus::Hung);

            let healthy_entry = report
                .shards
                .iter()
                .find(|entry| entry.shard_id == ShardId(0))
                .unwrap();
            assert_eq!(healthy_entry.status, HealthStatus::Healthy);
            assert_eq!(healthy_entry.pane_count, Some(1));

            let warnings = client.shard_watchdog_warnings_with_cx(&cx).await;
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("Shard 1 unhealthy"));
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `shard_health_report_with_cx` must
    /// bail early on a pre-cancelled cx without touching a backend while still
    /// returning one explicit not-started entry per configured shard.
    #[test]
    fn shard_health_report_with_cx_bails_on_precancelled_cx() {
        run_async_test(async {
            let healthy = Arc::new(CreationBoundaryBackend::new(
                CreationBoundaryBackend::VALID_LOCAL_PANE_ID,
                false,
                false,
            ));
            let healthy_handle: WeztermHandle = healthy.clone();
            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(ShardId(0), healthy_handle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel shard health test"),
            );

            let report = client.shard_health_report_with_cx(&cx).await;
            assert_eq!(report.shards.len(), 1);
            assert_eq!(report.outcome(), ShardHealthReportOutcome::Cancelled);
            assert_eq!(report.overall, HealthStatus::Degraded);
            assert_eq!(
                report.shards[0].probe_outcome,
                ShardHealthProbeOutcome::NotStarted
            );
            assert_eq!(healthy.list_calls.load(Ordering::Relaxed), 0);
            let warnings = report.watchdog_warnings();
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("health unknown"));
            assert!(warnings[0].contains("circuit=not_observed"));
            assert!(warnings[0].contains("probe=not_started"));
        });
    }

    #[test]
    fn shard_health_report_with_cx_preserves_mid_cancel_topology() {
        run_async_test(async {
            let first = Arc::new(CreationBoundaryBackend::new(
                CreationBoundaryBackend::VALID_LOCAL_PANE_ID,
                false,
                false,
            ));
            let cancelling = Arc::new(CreationBoundaryBackend::cancel_during_health_probe());
            let not_started = Arc::new(CreationBoundaryBackend::new(
                CreationBoundaryBackend::VALID_LOCAL_PANE_ID,
                false,
                false,
            ));

            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(ShardId(0), first.clone() as WeztermHandle),
                    ShardBackend::new(
                        ShardId(1),
                        cancelling.clone() as WeztermHandle,
                    ),
                    ShardBackend::new(
                        ShardId(2),
                        not_started.clone() as WeztermHandle,
                    ),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let cx = crate::cx::for_testing();
            let report = client.shard_health_report_with_cx(&cx).await;

            assert_eq!(report.shards.len(), 3);
            assert_eq!(report.outcome(), ShardHealthReportOutcome::Cancelled);
            assert_eq!(report.overall, HealthStatus::Degraded);
            assert_eq!(
                report.shards[0].probe_outcome,
                ShardHealthProbeOutcome::Complete
            );
            assert_eq!(
                report.shards[1].probe_outcome,
                ShardHealthProbeOutcome::Cancelled
            );
            assert_eq!(report.shards[1].pane_count, Some(0));
            assert_eq!(
                report.shards[2].probe_outcome,
                ShardHealthProbeOutcome::NotStarted
            );
            assert_eq!(first.list_calls.load(Ordering::Relaxed), 1);
            assert_eq!(cancelling.list_calls.load(Ordering::Relaxed), 1);
            assert_eq!(not_started.list_calls.load(Ordering::Relaxed), 0);

            let json = serde_json::to_string(&report).unwrap();
            assert!(json.contains("\"outcome\":\"cancelled\""));
            assert!(!json.contains("injected cancellation"));
        });
    }

    #[test]
    fn hostile_health_errors_are_classified_and_bounded() {
        run_async_test(async {
            let backend = Arc::new(CreationBoundaryBackend::hostile_health_failure());
            let handle: WeztermHandle = backend.clone();
            let debug_backend = ShardBackend::new(ShardId(0), handle.clone());
            let debug = format!("{debug_backend:?}");
            assert!(debug.len() < 128);

            let shard_count = ShardHealthReport::WATCHDOG_WARNING_LIMIT + 2;
            let backends = (0..shard_count)
                .map(|index| ShardBackend::new(ShardId(index), handle.clone()))
                .collect();
            let client =
                ShardedWeztermClient::new(backends, AssignmentStrategy::RoundRobin).unwrap();
            let client_debug = format!("{client:?}");
            assert!(client_debug.len() < 512);

            let routed_error = client
                .list_all_panes()
                .await
                .expect_err("hostile backend must fail pane listing");
            let routed_rendered = routed_error.to_string();
            assert!(routed_rendered.contains("class=command_failed"));
            assert!(!routed_rendered.contains("health-error-secret-sentinel"));
            assert!(routed_rendered.len() < 256);

            let report = client.shard_health_report().await;
            assert_eq!(report.shards.len(), shard_count);
            assert_eq!(report.outcome(), ShardHealthReportOutcome::Complete);
            assert_eq!(report.overall, HealthStatus::Hung);
            assert!(report.shards.iter().all(|entry| {
                entry.probe_outcome
                    == ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::CommandFailed)
            }));

            let json = serde_json::to_string(&report).unwrap();
            assert!(!json.contains("health-error-secret-sentinel"));
            assert!(json.len() < 128 * 1_024);
            let report_debug = format!("{report:?}");
            assert!(!report_debug.contains("health-error-secret-sentinel"));
            assert!(report_debug.contains("omitted_shards: 50"));
            assert!(report_debug.len() < 16 * 1_024);

            let warnings = report.watchdog_warnings();
            assert_eq!(warnings.len(), ShardHealthReport::WATCHDOG_WARNING_LIMIT + 1);
            assert!(warnings.iter().all(|warning| warning.len() < 256));
            assert!(warnings.iter().all(|warning| {
                !warning.contains("health-error-secret-sentinel")
            }));
            assert_eq!(
                warnings.last().map(String::as_str),
                Some(
                    "Shard watchdog omitted 2 additional unhealthy shard(s) after bounded limit 64"
                )
            );
        });
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn health_callback_panics_are_contained_classified_and_nonreflecting() {
        run_async_test(async {
            let probe_panic = Arc::new(CreationBoundaryBackend::panicking_health_probe());
            let circuit_panic = Arc::new(CreationBoundaryBackend::panicking_circuit_status());
            let healthy = Arc::new(CreationBoundaryBackend::new(
                CreationBoundaryBackend::VALID_LOCAL_PANE_ID,
                false,
                false,
            ));
            let client = ShardedWeztermClient::new(
                vec![
                    ShardBackend::new(
                        ShardId(0),
                        probe_panic.clone() as WeztermHandle,
                    ),
                    ShardBackend::new(
                        ShardId(1),
                        circuit_panic.clone() as WeztermHandle,
                    ),
                    ShardBackend::new(
                        ShardId(2),
                        healthy.clone() as WeztermHandle,
                    ),
                ],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let report = client.shard_health_report().await;
            assert_eq!(report.shards.len(), 3);
            assert_eq!(report.outcome(), ShardHealthReportOutcome::Complete);
            assert_eq!(report.overall, HealthStatus::Hung);
            assert_eq!(
                report.shards[0].probe_outcome,
                ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Panicked)
            );
            assert_eq!(
                report.shards[1].probe_outcome,
                ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Panicked)
            );
            assert_eq!(
                report.shards[2].probe_outcome,
                ShardHealthProbeOutcome::Complete
            );
            assert_eq!(probe_panic.list_calls.load(Ordering::Relaxed), 1);
            assert_eq!(circuit_panic.list_calls.load(Ordering::Relaxed), 0);
            assert_eq!(healthy.list_calls.load(Ordering::Relaxed), 1);

            let json = serde_json::to_string(&report).unwrap();
            let debug = format!("{report:?}");
            let warnings = report.watchdog_warnings();
            for projection in std::iter::once(json.as_str())
                .chain(std::iter::once(debug.as_str()))
                .chain(warnings.iter().map(String::as_str))
            {
                assert!(!projection.contains("health-panic-secret-sentinel"));
                assert!(!projection.contains("circuit-panic-secret-sentinel"));
            }
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
        let s = AssignmentStrategy::Manual {
            pane_to_shard: HashMap::from([(7, ShardId(1)), (u64::MAX, ShardId(2))]),
            default_shard: Some(ShardId(0)),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert!(json.contains("\"7\""));
        assert!(json.contains(&format!("\"{}\"", u64::MAX)));
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
        let backend = ShardBackend::new(ShardId(3), mock);
        let debug = format!("{:?}", backend);
        assert!(debug.contains("id: ShardId(3)"));
        // handle should be omitted via finish_non_exhaustive
        assert!(debug.contains(".."));
    }

    #[allow(deprecated)]
    #[test]
    fn sharded_backend_projection_preserves_retry_authority_and_redacts_details() {
        let indeterminate = ShardedWeztermClient::backend_error(
            ShardId(3),
            "split_pane",
            Some(77),
            crate::Error::Wezterm(WeztermError::IndeterminateMutation {
                operation: "cli_split_pane",
            }),
        );
        assert!(matches!(
            &indeterminate,
            crate::Error::Wezterm(WeztermError::IndeterminateMutation {
                operation: "split_pane"
            })
        ));
        assert!(!crate::retry::is_retryable(&indeterminate));
        assert_eq!(
            classify_backend_error(&indeterminate),
            ShardBackendErrorClass::IndeterminateMutation
        );

        for (storage_error, expected) in [
            (
                StorageError::IndeterminateMutation {
                    operation: "hostile_static_operation_identity",
                },
                StorageError::IndeterminateMutation {
                    operation: "custom_backend",
                },
            ),
            (
                StorageError::WriterSettlementIndeterminate {
                    phase: "hostile_static_phase_identity",
                },
                StorageError::WriterSettlementIndeterminate {
                    phase: "shard_backend_writer_settlement",
                },
            ),
        ] {
            let projected = ShardedWeztermClient::backend_error(
                ShardId(3),
                "custom_backend",
                None,
                crate::Error::Storage(storage_error),
            );
            assert_eq!(projected.to_string(), expected.to_string());
            assert!(!projected.to_string().contains("hostile_static"));
            assert!(!crate::retry::is_retryable(&projected));
            assert_eq!(
                classify_backend_error(&projected),
                ShardBackendErrorClass::IndeterminateMutation
            );
        }

        let storage_backend_detail = ShardedWeztermClient::backend_error(
            ShardId(3),
            "custom_backend",
            None,
            crate::Error::Storage(StorageError::Database(
                "hostile-storage-secret".repeat(1_024),
            )),
        );
        assert!(matches!(
            &storage_backend_detail,
            crate::Error::Storage(StorageError::Database(_))
        ));
        assert!(!storage_backend_detail
            .to_string()
            .contains("hostile-storage-secret"));
        assert!(storage_backend_detail.to_string().len() < 256);

        let missing = ShardedWeztermClient::backend_error(
            ShardId(3),
            "get_text",
            Some(77),
            crate::Error::Wezterm(WeztermError::PaneNotFound(5)),
        );
        assert!(matches!(
            &missing,
            crate::Error::Wezterm(WeztermError::PaneNotFound(77))
        ));
        assert!(!crate::retry::is_retryable(&missing));

        let circuit = ShardedWeztermClient::backend_error(
            ShardId(3),
            "list_panes",
            None,
            crate::Error::Wezterm(WeztermError::CircuitOpen {
                retry_after_ms: 250,
            }),
        );
        assert!(matches!(
            &circuit,
            crate::Error::Wezterm(WeztermError::CircuitOpen {
                retry_after_ms: 250
            })
        ));
        assert!(!crate::retry::is_retryable(&circuit));

        let cancelled = ShardedWeztermClient::backend_error(
            ShardId(3),
            "send_text",
            Some(77),
            crate::Error::Cancelled("hostile-pane-secret".repeat(1_024)),
        );
        assert!(matches!(&cancelled, crate::Error::Cancelled(_)));
        assert!(!crate::retry::is_retryable(&cancelled));
        assert!(!cancelled.to_string().contains("hostile-pane-secret"));

        let command = ShardedWeztermClient::backend_error(
            ShardId(3),
            "send_text",
            Some(77),
            crate::Error::Wezterm(WeztermError::CommandFailed(
                "hostile-command-secret".repeat(1_024),
            )),
        );
        let rendered = command.to_string();
        assert!(rendered.contains("class=command_failed"));
        assert!(!rendered.contains("hostile-command-secret"));
        assert!(rendered.len() < 256);
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
                ShardBackend::new(ShardId(0), mock1),
                ShardBackend::new(ShardId(0), mock2),
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
                mock,
            )],
            AssignmentStrategy::RoundRobin,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds 15-bit persistence-safe encoded pane id capacity")
        );
    }

    #[test]
    fn one_backend_uncached_route_rejects_non_persistable_global_id() {
        let mock = Arc::new(MockWezterm::new()) as WeztermHandle;
        let client = ShardedWeztermClient::new(
            vec![ShardBackend::new(ShardId(0), mock)],
            AssignmentStrategy::RoundRobin,
        )
        .unwrap();

        let err = client.resolve_uncached_pane_route(u64::MAX).unwrap_err();
        assert!(
            err.to_string().contains("persistence-safe signed-64 range"),
            "unexpected error: {err}"
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
                ShardBackend::new(ShardId(5), mock0),
                ShardBackend::new(ShardId(2), mock1),
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
            status: HealthStatus::Degraded,
            pane_count: None,
            circuit: CircuitBreakerStatus::default(),
            probe_outcome: ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ShardHealthEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.shard_id, ShardId(2));
        assert_eq!(back.status, HealthStatus::Degraded);
        assert!(back.pane_count.is_none());
        assert_eq!(back.probe_outcome, entry.probe_outcome);
        let projection: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(projection["probe_outcome"]["state"], "failed");
        assert_eq!(projection["probe_outcome"]["error_class"], "other");
    }

    #[test]
    fn shard_health_entries_decoder_rejects_oversized_sequence_from_size_hint() {
        let sequence = 0..(MAX_CONFIGURED_SHARDS + 1);
        let deserializer = serde::de::value::SeqDeserializer::<
            _,
            serde::de::value::Error,
        >::new(sequence);
        let error = deserialize_bounded_shard_health_entries(deserializer).unwrap_err();
        assert!(error.to_string().contains("at most 32768 shard health entries"));
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
                    ShardBackend::new(ShardId(0), shard0.clone() as WeztermHandle),
                    ShardBackend::new(ShardId(1), shard1.clone() as WeztermHandle),
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
                vec![ShardBackend::new(ShardId(0), shard0 as WeztermHandle)],
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
                vec![ShardBackend::new(ShardId(0), shard0 as WeztermHandle)],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_panes().await.unwrap();
            assert_eq!(panes.len(), 1);
            let global_id = panes[0].pane_id;

            client.kill_pane(global_id).await.unwrap();

            // Route should be removed
            assert!(!client.pane_routes.contains(global_id));
        });
    }

    #[test]
    fn kill_pane_with_cx_removes_route_only_after_backend_success() {
        run_async_test(async {
            let cx = crate::cx::for_request();
            let shard0 = Arc::new(MockWezterm::new());
            shard0.add_default_pane(1).await;

            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(
                    ShardId(0),
                    shard0 as WeztermHandle,
                )],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();

            let panes = client.list_all_panes_with_cx(&cx).await.unwrap();
            assert_eq!(panes.len(), 1);
            let global_id = panes[0].pane_id;
            assert!(
                client.pane_routes.contains(global_id),
                "list_all_panes_with_cx must seed the route before the kill"
            );

            client.kill_pane_with_cx(&cx, global_id).await.unwrap();

            assert!(
                !client.pane_routes.contains(global_id),
                "a successful Cx-aware backend kill must evict its cached route"
            );

            let failing_client = ShardedWeztermClient::new(
                vec![ShardBackend::new(
                    ShardId(0),
                    crate::wezterm::mock_wezterm_handle_failing(),
                )],
                AssignmentStrategy::RoundRobin,
            )
            .unwrap();
            let failed_global_id = try_encode_sharded_pane_id(ShardId(0), 7).unwrap();
            failing_client.pane_routes.insert(
                failed_global_id,
                PaneRoute {
                    shard_id: ShardId(0),
                    local_pane_id: 7,
                },
            );

            let error = failing_client
                .kill_pane_with_cx(&cx, failed_global_id)
                .await
                .expect_err("failing backend kill must propagate its error");
            assert!(
                error
                    .to_string()
                    .contains("kill_pane failed on shard 0, pane=7 (class=command_failed)"),
                "unexpected backend error: {error}"
            );
            assert!(
                failing_client.pane_routes.contains(failed_global_id),
                "a failed Cx-aware backend kill must preserve its cached route"
            );
        });
    }

    #[test]
    fn circuit_status_aggregates_worst_state() {
        run_async_test(async {
            let healthy = Arc::new(MockWezterm::new());
            let client = ShardedWeztermClient::new(
                vec![ShardBackend::new(
                    ShardId(0),
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
                vec![ShardBackend::new(ShardId(0), shard0 as WeztermHandle)],
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
                vec![ShardBackend::new(ShardId(0), shard0 as WeztermHandle)],
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
                vec![ShardBackend::new(ShardId(0), shard0 as WeztermHandle)],
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
                vec![ShardBackend::new(ShardId(0), shard0 as WeztermHandle)],
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
                vec![ShardBackend::new(ShardId(0), shard0 as WeztermHandle)],
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
    fn watchdog_warnings_classifies_complete_unhealthy_probe() {
        let report = ShardHealthReport {
            timestamp_ms: 1000,
            overall: HealthStatus::Critical,
            shards: vec![ShardHealthEntry {
                shard_id: ShardId(0),
                status: HealthStatus::Critical,
                pane_count: Some(0),
                circuit: CircuitBreakerStatus::default(),
                probe_outcome: ShardHealthProbeOutcome::Complete,
            }],
        };
        let warnings = report.watchdog_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("probe=complete"));
    }

    #[test]
    fn watchdog_warnings_are_content_free_and_count_bounded() {
        let shard_count = ShardHealthReport::WATCHDOG_WARNING_LIMIT + 2;
        let shards = (0..shard_count)
            .map(|index| ShardHealthEntry {
                shard_id: ShardId(index),
                status: HealthStatus::Critical,
                pane_count: None,
                circuit: CircuitBreakerStatus::default(),
                probe_outcome: ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
            })
            .collect();
        let report = ShardHealthReport {
            timestamp_ms: 1_000,
            overall: HealthStatus::Critical,
            shards,
        };

        let warnings = report.watchdog_warnings();
        assert_eq!(
            warnings.len(),
            ShardHealthReport::WATCHDOG_WARNING_LIMIT + 1
        );
        assert!(warnings[..ShardHealthReport::WATCHDOG_WARNING_LIMIT]
            .iter()
            .all(|warning| warning.len() < 256));
        assert!(warnings[..ShardHealthReport::WATCHDOG_WARNING_LIMIT]
            .iter()
            .all(|warning| warning.contains("probe=other")));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.len() < 128 * 1_024);
        let roundtrip: ShardHealthReport = serde_json::from_str(&json).unwrap();
        assert!(roundtrip
            .shards
            .iter()
            .all(|entry| {
                entry.probe_outcome
                    == ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other)
            }));
        assert_eq!(
            warnings.last().map(String::as_str),
            Some(
                "Shard watchdog omitted 2 additional unhealthy shard(s) after bounded limit 64"
            )
        );
    }

    // -----------------------------------------------------------------------
    // SHARD_ID_BITS / LOCAL_PANE_ID_MASK constants
    // -----------------------------------------------------------------------

    #[test]
    fn shard_id_bits_and_mask_are_consistent() {
        assert_eq!(SHARD_ID_BITS, 15);
        assert_eq!(LOCAL_PANE_ID_BITS, 48);
        assert_eq!(SHARD_ID_BITS + LOCAL_PANE_ID_BITS, 63);
        assert_eq!(LOCAL_PANE_ID_MASK, (1u64 << LOCAL_PANE_ID_BITS) - 1);
        assert_eq!(MAX_CONFIGURED_SHARDS, MAX_SHARD_ID + 1);
    }

    // -- Batch: DarkBadger wa-1u90p.7.1 ----------------------------------------

    #[test]
    fn shard_id_display_v2() {
        assert_eq!(ShardId(0).to_string(), "0");
        assert_eq!(ShardId(42).to_string(), "42");
        assert_eq!(ShardId(MAX_SHARD_ID).to_string(), "32767");
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
    fn assignment_strategy_debug_reports_cardinality_without_raw_keys() {
        let secret = format!("strategy-secret-sentinel-{}", "q".repeat(64 * 1_024));
        let strategy = AssignmentStrategy::ByDomain {
            domain_to_shard: HashMap::from([(secret.clone(), ShardId(0))]),
            default_shard: Some(ShardId(0)),
        };

        let debug = format!("{strategy:?}");
        assert!(debug.contains("mapping_count: 1"));
        assert!(!debug.contains("strategy-secret-sentinel"));
        assert!(debug.len() < 128);
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
    fn encode_local_id_overflow_panics_in_infallible_helper() {
        let big_local = LOCAL_PANE_ID_MASK + 1;
        let panic = std::panic::catch_unwind(|| encode_sharded_pane_id(ShardId(0), big_local));
        assert!(panic.is_err());
    }
}
