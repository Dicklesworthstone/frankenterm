//! Post-cutover soak and user-journey confidence gate (ft-e34d9.10.8.5).
//!
//! Defines the soak matrix, user-journey scenarios, failure-injection soak
//! profiles, and confidence gate evaluation for final migration closure.
//!
//! # Architecture
//!
//! ```text
//! SoakMatrix
//!   ├── UserJourneyScenario (ft watch, robot loops, session, SSH, restart)
//!   │     ├── WorkloadProfile (steady, burst, mixed, degraded)
//!   │     └── FailureInjectionProfile (none, light, heavy, cascade)
//!   │
//!   ├── SoakExecutionPlan (matrix → executable plan)
//!   │     └── SoakCell (scenario × profile × injection)
//!   │
//!   └── SoakExecutionResult
//!         ├── CellResult (per-cell pass/fail + telemetry)
//!         └── SoakInvariantCheck (task leaks, deadlocks, message loss, latency)
//!
//! ConfidenceGate
//!   ├── evaluate(results) → ConfidenceVerdict
//!   └── to_evidence() → SoakOutcome (for cutover_evidence.rs)
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cutover_evidence::SoakOutcome;

// =============================================================================
// User journey scenarios
// =============================================================================

/// Categorization of user-facing workflows to validate during soak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum JourneyCategory {
    /// `ft watch` — continuous pane monitoring and pattern detection.
    Watch,
    /// Robot orchestration loops — MCP-driven agent workflows.
    RobotOrchestration,
    /// Session persistence — session save/restore across restart.
    SessionPersistence,
    /// Remote SSH flows — mux client over SSH transport.
    RemoteSsh,
    /// Restart cycles — clean shutdown and recovery.
    RestartCycle,
    /// Mixed workload bursts — concurrent multi-category operations.
    MixedBurst,
    /// Search — semantic + lexical search under load.
    Search,
    /// Recording/replay — event recording and deterministic replay.
    RecordingReplay,
}

impl JourneyCategory {
    /// All defined journey categories.
    pub const ALL: &'static [JourneyCategory] = &[
        Self::Watch,
        Self::RobotOrchestration,
        Self::SessionPersistence,
        Self::RemoteSsh,
        Self::RestartCycle,
        Self::MixedBurst,
        Self::Search,
        Self::RecordingReplay,
    ];

    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Watch => "ft watch",
            Self::RobotOrchestration => "robot orchestration",
            Self::SessionPersistence => "session persistence",
            Self::RemoteSsh => "remote SSH",
            Self::RestartCycle => "restart cycle",
            Self::MixedBurst => "mixed burst",
            Self::Search => "search",
            Self::RecordingReplay => "recording/replay",
        }
    }

    /// Whether this journey is critical-path (failure blocks cutover).
    #[must_use]
    pub fn is_critical(&self) -> bool {
        matches!(
            self,
            Self::Watch | Self::RobotOrchestration | Self::SessionPersistence | Self::RestartCycle
        )
    }
}

/// Typed runner-owned driver for one user journey.
///
/// This deliberately replaces free-form shell command strings. The eventual
/// soak runner must dispatch these variants through audited in-process or
/// handle-owned adapters rather than interpreting shell text from a corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyDriver {
    /// Observe panes and pattern transitions through the watch path.
    Watch,
    /// Exercise Robot/MCP orchestration requests.
    RobotOrchestration,
    /// Save, restore, and compare session state.
    SessionPersistence,
    /// Exercise an isolated remote-SSH route.
    RemoteSsh,
    /// Exercise an isolated restart and recovery cycle.
    RestartCycle,
    /// Interleave multiple workload classes.
    MixedBurst,
    /// Exercise lexical, semantic, and hybrid search.
    Search,
    /// Record and replay a deterministic event stream.
    RecordingReplay,
}

impl From<JourneyCategory> for JourneyDriver {
    fn from(category: JourneyCategory) -> Self {
        match category {
            JourneyCategory::Watch => Self::Watch,
            JourneyCategory::RobotOrchestration => Self::RobotOrchestration,
            JourneyCategory::SessionPersistence => Self::SessionPersistence,
            JourneyCategory::RemoteSsh => Self::RemoteSsh,
            JourneyCategory::RestartCycle => Self::RestartCycle,
            JourneyCategory::MixedBurst => Self::MixedBurst,
            JourneyCategory::Search => Self::Search,
            JourneyCategory::RecordingReplay => Self::RecordingReplay,
        }
    }
}

/// A single user-journey scenario definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserJourneyScenario {
    /// Scenario identifier.
    pub scenario_id: String,
    /// Which journey category this tests.
    pub category: JourneyCategory,
    /// Human-readable description.
    pub description: String,
    /// Expected duration for a single run (ms).
    pub expected_duration_ms: u64,
    /// Whether failure of this scenario blocks cutover.
    pub blocking: bool,
    /// Deterministic seed for reproducibility.
    pub seed: Option<u64>,
}

// =============================================================================
// Workload and failure injection profiles
// =============================================================================

/// Workload intensity profile for soak scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WorkloadProfile {
    /// Steady-state: constant low-moderate load.
    Steady,
    /// Burst: periodic high-load spikes.
    Burst,
    /// Mixed: varying concurrent workloads.
    Mixed,
    /// Degraded: running under resource pressure.
    Degraded,
}

impl WorkloadProfile {
    /// All defined workload profiles.
    pub const ALL: &'static [WorkloadProfile] =
        &[Self::Steady, Self::Burst, Self::Mixed, Self::Degraded];
}

/// Failure injection intensity for soak scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FailureInjectionProfile {
    /// No failure injection — baseline correctness.
    None,
    /// Light: occasional transient faults.
    Light,
    /// Heavy: frequent faults across multiple points.
    Heavy,
    /// Cascade: simultaneous multi-point failures.
    Cascade,
}

impl FailureInjectionProfile {
    /// All defined injection profiles.
    pub const ALL: &'static [FailureInjectionProfile] =
        &[Self::None, Self::Light, Self::Heavy, Self::Cascade];
}

// =============================================================================
// Deterministic long-haul workload corpus
// =============================================================================

/// Schema version for the deterministic long-haul workload corpus.
pub const SOAK_WORKLOAD_CORPUS_VERSION: &str = "ft.soak_workload_corpus.v1";
/// Tracked machine-readable corpus used by the 4h/24h/72h runner.
pub const SOAK_WORKLOAD_CORPUS_FIXTURE: &str = "fixtures/perf/soak-workload-corpus-v1.json";
/// Required fleet sizes for the long-haul workload contract.
pub const SOAK_WORKLOAD_REQUIRED_PANE_COUNTS: &[u32] = &[20, 50, 200];
const SOAK_INTERACTIVE_REQUIRED_DIMENSIONS: &[SoakWorkloadDimension] = &[
    SoakWorkloadDimension::EditorTui,
    SoakWorkloadDimension::AgentLikeStream,
    SoakWorkloadDimension::ProgressRedraw,
    SoakWorkloadDimension::ResizeZoom,
    SoakWorkloadDimension::LayoutChurn,
    SoakWorkloadDimension::Reconnect,
];
const MAX_SOAK_ASSETS: usize = 64;
const MAX_SOAK_ASSET_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SOAK_ACTOR_ARGS: usize = 32;
const MAX_SOAK_ARG_BYTES: usize = 256;
const MAX_SOAK_PHASES: usize = 32;
const MAX_SOAK_CYCLE_MS: u64 = 3_600_000;
const MAX_SOAK_ACTIONS: usize = 1_000_000;
const MAX_SOAK_LIFECYCLE_OPERATIONS: usize = 1_024;
const MAX_SOAK_ACTOR_OUTPUT_BYTES_PER_SECOND: u64 = 64 * 1024 * 1024;
const MAX_SOAK_AGGREGATE_OUTPUT_BYTES_PER_SECOND: u64 = 1024 * 1024 * 1024;
const MAX_SOAK_MATRIX_CELLS: usize = 4_096;
const MAX_SOAK_MATRIX_SCENARIO_MS: u64 = 86_400_000;

/// One required dimension in the deterministic workload corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakWorkloadDimension {
    QuietShell,
    EditorTui,
    BuildTestOutput,
    ProgressRedraw,
    AgentLikeStream,
    Images,
    GlyphDiversity,
    Search,
    Capture,
    Workflow,
    Maintenance,
    LayoutChurn,
    ResizeZoom,
    Reconnect,
    OutputBurst,
}

impl SoakWorkloadDimension {
    /// Complete dimension set required at every scale point.
    pub const ALL: &'static [Self] = &[
        Self::QuietShell,
        Self::EditorTui,
        Self::BuildTestOutput,
        Self::ProgressRedraw,
        Self::AgentLikeStream,
        Self::Images,
        Self::GlyphDiversity,
        Self::Search,
        Self::Capture,
        Self::Workflow,
        Self::Maintenance,
        Self::LayoutChurn,
        Self::ResizeZoom,
        Self::Reconnect,
        Self::OutputBurst,
    ];
}

/// How the runner must dispatch an actor command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakActorDriver {
    /// Project-owned typed operation; no child process or shell parsing.
    Builtin,
    /// Replay a pinned fixture through a project-owned adapter.
    FixtureReplay,
    /// Spawn one exact argv vector in the isolated runner child registry.
    IsolatedArgv,
}

/// Whether an actor adapter remains active or is invoked for each scheduled action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakActorActivation {
    /// Start one owned isolated child during setup and stop it during teardown.
    Persistent,
    /// Invoke one bounded adapter action for each scheduled corpus action.
    Scheduled,
}

/// Bounded graceful shutdown contract for a persistent isolated actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SoakPersistentShutdown {
    /// Send one line through the owned child's stdin, then await settlement.
    StdinLine { line: String },
    /// Interrupt the owned child process group, then await settlement.
    Interrupt,
}

/// One content-addressed asset reused by deterministic actors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakWorkloadAsset {
    pub asset_id: String,
    pub path: String,
    pub sha256: String,
    pub executable: bool,
    pub purpose: String,
}

/// Structured actor command. `program` is never interpreted by a shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakActorCommand {
    pub driver: SoakActorDriver,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// One deterministic actor template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakActorSpec {
    pub actor_id: String,
    pub dimension: SoakWorkloadDimension,
    pub activation: SoakActorActivation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown: Option<SoakPersistentShutdown>,
    pub command: SoakActorCommand,
    #[serde(default)]
    pub asset_ids: Vec<String>,
    pub payload_profile: String,
    pub output_bytes_per_second: u64,
    pub expected_final_marker: String,
}

/// Pane allocation for one actor template at a scale point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakActorAllocation {
    pub actor_id: String,
    pub pane_count: u32,
}

/// Exact deterministic fleet layout at one required scale point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakScaleSpec {
    pub pane_count: u32,
    pub window_count: u32,
    pub tab_count: u32,
    pub interactive_pane_count: u32,
    pub aggregate_output_bytes_per_second: u64,
    pub allocations: Vec<SoakActorAllocation>,
}

/// One non-overlapping phase in a repeatable workload cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakPhaseSpec {
    pub phase_id: String,
    pub start_offset_ms: u64,
    pub duration_ms: u64,
    pub action_cadence_ms: u64,
    pub dimensions: Vec<SoakWorkloadDimension>,
}

/// Which layer owns the final oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakOracleAuthority {
    /// The deterministic logical replay can adjudicate the oracle.
    LogicalReplay,
    /// The isolated production runner must retain and adjudicate the oracle.
    ProductionRunner,
}

/// Required final-state oracle kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakFinalOracleKind {
    NoOwnedWorkspace,
    NoLiveActors,
    NoOpenPanes,
    NoOpenTabs,
    NoOpenWindows,
    NoPendingActions,
    AllDimensionsObserved,
    StableFleetIdentityMap,
    FinalTerminalStateHash,
    FinalLayoutStateHash,
    CaptureSearchQuiescent,
    NoOrphanChildren,
    NoOpenTransports,
    ResourceOwnershipAtBaseline,
}

impl SoakFinalOracleKind {
    const ALL: &'static [Self] = &[
        Self::NoOwnedWorkspace,
        Self::NoLiveActors,
        Self::NoOpenPanes,
        Self::NoOpenTabs,
        Self::NoOpenWindows,
        Self::NoPendingActions,
        Self::AllDimensionsObserved,
        Self::StableFleetIdentityMap,
        Self::FinalTerminalStateHash,
        Self::FinalLayoutStateHash,
        Self::CaptureSearchQuiescent,
        Self::NoOrphanChildren,
        Self::NoOpenTransports,
        Self::ResourceOwnershipAtBaseline,
    ];
}

/// One final-state oracle and the layer authorized to adjudicate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakFinalOracleSpec {
    pub oracle: SoakFinalOracleKind,
    pub authority: SoakOracleAuthority,
}

/// Real-agent dogfood is deliberately separate from deterministic evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakDogfoodPolicy {
    pub excluded_from_deterministic_verdict: bool,
    pub required_identity_fields: Vec<String>,
}

/// Compact source-of-truth specification for deterministic long-haul work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakWorkloadCorpus {
    pub version: String,
    pub corpus_id: String,
    pub base_seed: u64,
    pub interactive_dimension_priority: Vec<SoakWorkloadDimension>,
    pub assets: Vec<SoakWorkloadAsset>,
    pub actors: Vec<SoakActorSpec>,
    pub scales: Vec<SoakScaleSpec>,
    pub phases: Vec<SoakPhaseSpec>,
    pub final_oracles: Vec<SoakFinalOracleSpec>,
    pub dogfood: SoakDogfoodPolicy,
}

/// Stable logical identity, independent of transient mux pane identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakFleetIdentity {
    pub identity_id: String,
    pub fleet_slot: u32,
    pub actor_id: String,
    pub actor_instance: u32,
    pub actor_seed: u64,
    pub interactive: bool,
    pub dimension: SoakWorkloadDimension,
    pub activation: SoakActorActivation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown: Option<SoakPersistentShutdown>,
    pub command: SoakActorCommand,
    pub asset_ids: Vec<String>,
    pub payload_profile: String,
    pub output_bytes_per_second: u64,
    pub expected_final_marker: String,
    pub window_id: String,
    pub tab_id: String,
    pub pane_id: String,
}

/// One scheduled logical actor action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakScheduledAction {
    pub action_id: String,
    pub phase_id: String,
    pub at_ms: u64,
    pub identity_id: String,
    pub actor_id: String,
    pub dimension: SoakWorkloadDimension,
    pub payload_profile: String,
}

/// Lifecycle operations are idempotent and applied through owned adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakLifecycleKind {
    AcquireWorkspace,
    OpenWindow,
    OpenTab,
    OpenPane,
    StartActor,
    StopActor,
    ClosePane,
    CloseTab,
    CloseWindow,
    ReleaseWorkspace,
}

/// One setup or teardown operation with a stable idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakLifecycleOperation {
    pub operation_id: String,
    pub kind: SoakLifecycleKind,
    pub resource_id: String,
    pub parent_id: Option<String>,
}

/// Fully materialized deterministic plan for a single scale point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakWorkloadPlan {
    pub version: String,
    pub corpus_id: String,
    pub base_seed: u64,
    pub interactive_dimension_priority: Vec<SoakWorkloadDimension>,
    pub pane_count: u32,
    pub window_count: u32,
    pub tab_count: u32,
    pub interactive_pane_count: u32,
    pub aggregate_output_bytes_per_second: u64,
    pub cycle_duration_ms: u64,
    pub assets: Vec<SoakWorkloadAsset>,
    pub identities: Vec<SoakFleetIdentity>,
    pub phases: Vec<SoakPhaseSpec>,
    pub setup: Vec<SoakLifecycleOperation>,
    pub actions: Vec<SoakScheduledAction>,
    pub teardown: Vec<SoakLifecycleOperation>,
    pub final_oracles: Vec<SoakFinalOracleSpec>,
    pub plan_sha256: String,
}

/// Truthful logical replay summary. Production-only oracles remain required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakWorkloadReplaySummary {
    pub plan_sha256: String,
    pub failed_identity_id: Option<String>,
    pub actions_executed: u64,
    pub actions_skipped: u64,
    pub action_counts_by_dimension: BTreeMap<SoakWorkloadDimension, u64>,
    pub remaining_workspaces: u64,
    pub remaining_windows: u64,
    pub remaining_tabs: u64,
    pub remaining_panes: u64,
    pub remaining_actors: u64,
    pub all_dimensions_observed: bool,
    pub teardown_complete: bool,
    pub logical_oracle_results: BTreeMap<SoakFinalOracleKind, bool>,
    pub production_runner_oracles: Vec<SoakFinalOracleKind>,
    pub summary_sha256: String,
}

/// Validation/materialization failures for the deterministic workload corpus.
#[derive(Debug)]
pub enum SoakWorkloadCorpusError {
    Json(serde_json::Error),
    Io(std::io::Error),
    Invalid(String),
    MissingScale(u32),
}

impl std::fmt::Display for SoakWorkloadCorpusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid soak workload JSON: {error}"),
            Self::Io(error) => write!(formatter, "soak workload asset I/O failed: {error}"),
            Self::Invalid(message) => write!(formatter, "invalid soak workload corpus: {message}"),
            Self::MissingScale(panes) => {
                write!(formatter, "soak workload has no {panes}-pane scale")
            }
        }
    }
}

impl std::error::Error for SoakWorkloadCorpusError {}

impl From<serde_json::Error> for SoakWorkloadCorpusError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<std::io::Error> for SoakWorkloadCorpusError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Parse and structurally validate a workload corpus.
pub fn parse_soak_workload_corpus(
    json: &str,
) -> Result<SoakWorkloadCorpus, SoakWorkloadCorpusError> {
    let corpus: SoakWorkloadCorpus = serde_json::from_str(json)?;
    validate_soak_workload_corpus(&corpus)?;
    Ok(corpus)
}

/// Validate the schema, allocation, coverage, and authority invariants.
pub fn validate_soak_workload_corpus(
    corpus: &SoakWorkloadCorpus,
) -> Result<(), SoakWorkloadCorpusError> {
    if corpus.version != SOAK_WORKLOAD_CORPUS_VERSION {
        return invalid_soak_workload(format!("unsupported version {}", corpus.version));
    }
    validate_soak_id("corpus_id", &corpus.corpus_id)?;
    if corpus.base_seed == 0 {
        return invalid_soak_workload("base_seed must be non-zero");
    }
    validate_soak_interactive_priority(&corpus.interactive_dimension_priority)?;
    if corpus.assets.is_empty() || corpus.assets.len() > MAX_SOAK_ASSETS {
        return invalid_soak_workload("asset count is empty or exceeds the corpus cap");
    }

    let mut asset_ids = BTreeSet::new();
    let mut unique_asset_paths = BTreeSet::new();
    let mut assets_by_id = BTreeMap::new();
    for asset in &corpus.assets {
        validate_soak_id("asset_id", &asset.asset_id)?;
        validate_relative_asset_path(&asset.path)?;
        validate_sha256(&asset.sha256)?;
        if asset.purpose.trim().is_empty()
            || asset.purpose.len() > MAX_SOAK_ARG_BYTES
            || asset.purpose.chars().any(char::is_control)
        {
            return invalid_soak_workload(format!(
                "asset {} has an invalid purpose",
                asset.asset_id
            ));
        }
        if !asset_ids.insert(asset.asset_id.clone()) {
            return invalid_soak_workload(format!("duplicate asset {}", asset.asset_id));
        }
        if !unique_asset_paths.insert(asset.path.as_str()) {
            return invalid_soak_workload(format!("duplicate asset path {}", asset.path));
        }
        assets_by_id.insert(asset.asset_id.as_str(), asset);
    }

    let mut actors = BTreeMap::new();
    let mut actor_dimensions = BTreeSet::new();
    let mut referenced_asset_ids = BTreeSet::new();
    for actor in &corpus.actors {
        validate_soak_id("actor_id", &actor.actor_id)?;
        validate_soak_id("payload_profile", &actor.payload_profile)?;
        if actor.output_bytes_per_second > MAX_SOAK_ACTOR_OUTPUT_BYTES_PER_SECOND {
            return invalid_soak_workload(format!(
                "actor {} exceeds the per-actor output safety cap",
                actor.actor_id
            ));
        }
        validate_soak_actor_binding(
            &format!("actor {}", actor.actor_id),
            actor.dimension,
            actor.activation,
            actor.shutdown.as_ref(),
            &actor.command,
            &actor.asset_ids,
            &assets_by_id,
            actor.output_bytes_per_second,
            &actor.expected_final_marker,
        )?;
        referenced_asset_ids.extend(actor.asset_ids.iter().map(String::as_str));
        if actors.insert(actor.actor_id.as_str(), actor).is_some() {
            return invalid_soak_workload(format!("duplicate actor {}", actor.actor_id));
        }
        if !actor_dimensions.insert(actor.dimension) {
            return invalid_soak_workload(format!(
                "dimension {:?} has more than one actor template",
                actor.dimension
            ));
        }
    }
    let required_dimensions = SoakWorkloadDimension::ALL
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if actor_dimensions != required_dimensions {
        return invalid_soak_workload("actor templates do not cover every required dimension");
    }
    if referenced_asset_ids != assets_by_id.keys().copied().collect::<BTreeSet<_>>() {
        return invalid_soak_workload(
            "every content-addressed asset must be referenced by at least one actor",
        );
    }

    let mut observed_scales = BTreeSet::new();
    for scale in &corpus.scales {
        if !observed_scales.insert(scale.pane_count) {
            return invalid_soak_workload(format!("duplicate {}-pane scale", scale.pane_count));
        }
        validate_soak_scale(scale, &actors)?;
    }
    let required_scales = SOAK_WORKLOAD_REQUIRED_PANE_COUNTS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if observed_scales != required_scales {
        return invalid_soak_workload("scale points must be exactly 20, 50, and 200 panes");
    }

    validate_soak_phases(&corpus.phases, &required_dimensions)?;

    if corpus.final_oracles.len() != SoakFinalOracleKind::ALL.len() {
        return invalid_soak_workload("final oracle count is not the exact versioned contract");
    }
    let oracle_kinds = corpus
        .final_oracles
        .iter()
        .map(|oracle| oracle.oracle)
        .collect::<BTreeSet<_>>();
    let required_oracles = SoakFinalOracleKind::ALL
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if oracle_kinds != required_oracles || oracle_kinds.len() != corpus.final_oracles.len() {
        return invalid_soak_workload("final oracle set is incomplete or duplicated");
    }
    for oracle in &corpus.final_oracles {
        let must_be_production = matches!(
            oracle.oracle,
            SoakFinalOracleKind::FinalTerminalStateHash
                | SoakFinalOracleKind::FinalLayoutStateHash
                | SoakFinalOracleKind::CaptureSearchQuiescent
                | SoakFinalOracleKind::NoOrphanChildren
                | SoakFinalOracleKind::NoOpenTransports
                | SoakFinalOracleKind::ResourceOwnershipAtBaseline
        );
        if must_be_production && oracle.authority != SoakOracleAuthority::ProductionRunner {
            return invalid_soak_workload(format!(
                "oracle {:?} must remain production-runner authority",
                oracle.oracle
            ));
        }
        if !must_be_production && oracle.authority != SoakOracleAuthority::LogicalReplay {
            return invalid_soak_workload(format!(
                "oracle {:?} must remain logical-replay authority",
                oracle.oracle
            ));
        }
    }

    if !corpus.dogfood.excluded_from_deterministic_verdict {
        return invalid_soak_workload(
            "real-agent dogfood must be excluded from deterministic verdicts",
        );
    }
    if corpus.dogfood.required_identity_fields.len() != 6 {
        return invalid_soak_workload("dogfood identity field count is not the exact contract");
    }
    let dogfood_fields = corpus
        .dogfood
        .required_identity_fields
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let required_dogfood_fields = [
        "agent_name",
        "agent_version",
        "model_id",
        "config_digest",
        "session_id",
        "transcript_digest",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if dogfood_fields != required_dogfood_fields
        || dogfood_fields.len() != corpus.dogfood.required_identity_fields.len()
    {
        return invalid_soak_workload(
            "dogfood identity fields must be the exact versioned identity contract",
        );
    }

    Ok(())
}

/// Verify every content-addressed asset against the checked-out source tree.
pub fn verify_soak_workload_assets(
    workspace_root: &Path,
    corpus: &SoakWorkloadCorpus,
) -> Result<(), SoakWorkloadCorpusError> {
    validate_soak_workload_corpus(corpus)?;
    verify_soak_assets(workspace_root, &corpus.assets)
}

/// Verify a validated materialized plan's content-addressed assets.
pub fn verify_soak_workload_plan_assets(
    workspace_root: &Path,
    plan: &SoakWorkloadPlan,
) -> Result<(), SoakWorkloadCorpusError> {
    validate_soak_workload_plan(plan)?;
    verify_soak_assets(workspace_root, &plan.assets)
}

fn verify_soak_assets(
    workspace_root: &Path,
    assets: &[SoakWorkloadAsset],
) -> Result<(), SoakWorkloadCorpusError> {
    let canonical_root = std::fs::canonicalize(workspace_root)?;
    for asset in assets {
        let path = canonical_root.join(&asset.path);
        let path_metadata = std::fs::symlink_metadata(&path)?;
        if !path_metadata.file_type().is_file() || path_metadata.file_type().is_symlink() {
            return invalid_soak_workload(format!(
                "asset {} is not a regular non-symlink file",
                asset.asset_id
            ));
        }
        let canonical_path = std::fs::canonicalize(&path)?;
        if !canonical_path.starts_with(&canonical_root) {
            return invalid_soak_workload(format!(
                "asset {} resolves outside the workspace root",
                asset.asset_id
            ));
        }
        let file = std::fs::File::open(&canonical_path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_SOAK_ASSET_BYTES {
            return invalid_soak_workload(format!(
                "asset {} is not a bounded regular file under the {} byte cap",
                asset.asset_id, MAX_SOAK_ASSET_BYTES
            ));
        }
        #[cfg(unix)]
        if asset.executable {
            use std::os::unix::fs::PermissionsExt;

            if metadata.permissions().mode() & 0o111 == 0 {
                return invalid_soak_workload(format!(
                    "asset {} is declared executable but has no executable mode bit",
                    asset.asset_id
                ));
            }
        }
        #[cfg(not(unix))]
        if asset.executable {
            return invalid_soak_workload(format!(
                "asset {} requires Unix executable-mode verification",
                asset.asset_id
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_SOAK_ASSET_BYTES + 1)
            .read_to_end(&mut bytes)?;
        let bytes_read = u64::try_from(bytes.len()).map_err(|_| {
            SoakWorkloadCorpusError::Invalid(format!(
                "asset {} byte count does not fit u64",
                asset.asset_id
            ))
        })?;
        if bytes_read > MAX_SOAK_ASSET_BYTES {
            return invalid_soak_workload(format!(
                "asset {} grew beyond the {} byte cap while being verified",
                asset.asset_id, MAX_SOAK_ASSET_BYTES
            ));
        }
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != asset.sha256 {
            return invalid_soak_workload(format!(
                "asset {} sha256 mismatch: expected {}, got {actual}",
                asset.asset_id, asset.sha256
            ));
        }
    }
    Ok(())
}

/// Materialize one order-independent deterministic plan.
pub fn materialize_soak_workload_plan(
    corpus: &SoakWorkloadCorpus,
    pane_count: u32,
) -> Result<SoakWorkloadPlan, SoakWorkloadCorpusError> {
    validate_soak_workload_corpus(corpus)?;
    let scale = corpus
        .scales
        .iter()
        .find(|scale| scale.pane_count == pane_count)
        .ok_or(SoakWorkloadCorpusError::MissingScale(pane_count))?;
    let actors = corpus
        .actors
        .iter()
        .map(|actor| (actor.actor_id.as_str(), actor))
        .collect::<BTreeMap<_, _>>();
    let allocations = scale
        .allocations
        .iter()
        .map(|allocation| (allocation.actor_id.as_str(), allocation.pane_count))
        .collect::<BTreeMap<_, _>>();

    let workspace_id = format!("{}-{pane_count}p-workspace", corpus.corpus_id);
    let mut identities = Vec::new();
    let mut fleet_slot = 0_u32;
    for (actor_id, allocation_count) in allocations {
        let actor = actors
            .get(actor_id)
            .ok_or_else(|| SoakWorkloadCorpusError::Invalid(format!("unknown actor {actor_id}")))?;
        for actor_instance in 0..allocation_count {
            let window_index = fleet_slot % scale.window_count;
            let tab_index = fleet_slot % scale.tab_count;
            let identity_id = format!("{}-{pane_count}p-slot-{fleet_slot:04}", corpus.corpus_id);
            identities.push(SoakFleetIdentity {
                identity_id,
                fleet_slot,
                actor_id: actor.actor_id.clone(),
                actor_instance,
                actor_seed: soak_actor_seed(
                    corpus.base_seed,
                    pane_count,
                    &actor.actor_id,
                    actor_instance,
                ),
                interactive: false,
                dimension: actor.dimension,
                activation: actor.activation,
                shutdown: actor.shutdown.clone(),
                command: actor.command.clone(),
                asset_ids: {
                    let mut asset_ids = actor.asset_ids.clone();
                    asset_ids.sort();
                    asset_ids
                },
                payload_profile: actor.payload_profile.clone(),
                output_bytes_per_second: actor.output_bytes_per_second,
                expected_final_marker: actor.expected_final_marker.clone(),
                window_id: format!("{workspace_id}-window-{window_index:03}"),
                tab_id: format!("{workspace_id}-tab-{tab_index:03}"),
                pane_id: format!("{workspace_id}-pane-{fleet_slot:04}"),
            });
            fleet_slot = fleet_slot
                .checked_add(1)
                .ok_or_else(|| SoakWorkloadCorpusError::Invalid("fleet slot overflow".into()))?;
        }
    }
    let interactive_slots = select_soak_interactive_slots(
        &identities,
        scale.interactive_pane_count,
        &corpus.interactive_dimension_priority,
    )?;
    for identity in &mut identities {
        identity.interactive = interactive_slots.contains(&identity.fleet_slot);
    }

    let setup = build_soak_setup(
        &workspace_id,
        scale.window_count,
        scale.tab_count,
        &identities,
    );
    let teardown = build_soak_teardown(
        &workspace_id,
        scale.window_count,
        scale.tab_count,
        &identities,
    );
    let mut phases = corpus.phases.clone();
    canonicalize_soak_phases(&mut phases);
    let required_dimensions = SoakWorkloadDimension::ALL
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let cycle_duration_ms = validate_soak_phases(&phases, &required_dimensions)?;
    let actions = build_soak_actions(&phases, &identities)?;
    let mut final_oracles = corpus.final_oracles.clone();
    final_oracles.sort_by_key(|oracle| oracle.oracle);
    let mut assets = corpus.assets.clone();
    assets.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
    let mut plan = SoakWorkloadPlan {
        version: corpus.version.clone(),
        corpus_id: corpus.corpus_id.clone(),
        base_seed: corpus.base_seed,
        interactive_dimension_priority: corpus.interactive_dimension_priority.clone(),
        pane_count,
        window_count: scale.window_count,
        tab_count: scale.tab_count,
        interactive_pane_count: scale.interactive_pane_count,
        aggregate_output_bytes_per_second: scale.aggregate_output_bytes_per_second,
        cycle_duration_ms,
        assets,
        identities,
        phases,
        setup,
        actions,
        teardown,
        final_oracles,
        plan_sha256: String::new(),
    };
    plan.plan_sha256 = digest_soak_plan(&plan)?;
    validate_soak_workload_plan(&plan)?;
    Ok(plan)
}

/// Validate a serialized materialized plan before a runner or replay trusts it.
pub fn validate_soak_workload_plan(plan: &SoakWorkloadPlan) -> Result<(), SoakWorkloadCorpusError> {
    if plan.version != SOAK_WORKLOAD_CORPUS_VERSION {
        return invalid_soak_workload(format!("unsupported plan version {}", plan.version));
    }
    validate_soak_id("corpus_id", &plan.corpus_id)?;
    validate_soak_interactive_priority(&plan.interactive_dimension_priority)?;
    if plan.base_seed == 0
        || !SOAK_WORKLOAD_REQUIRED_PANE_COUNTS.contains(&plan.pane_count)
        || plan.window_count == 0
        || plan.tab_count < plan.window_count
        || plan.tab_count % plan.window_count != 0
        || plan.tab_count > plan.pane_count
        || plan.interactive_pane_count == 0
        || plan.interactive_pane_count > plan.pane_count
        || plan.aggregate_output_bytes_per_second > MAX_SOAK_AGGREGATE_OUTPUT_BYTES_PER_SECOND
        || plan.cycle_duration_ms == 0
        || plan.cycle_duration_ms > MAX_SOAK_CYCLE_MS
        || plan.phases.is_empty()
        || plan.phases.len() > MAX_SOAK_PHASES
        || plan.actions.is_empty()
        || plan.actions.len() > MAX_SOAK_ACTIONS
        || plan.setup.len() > MAX_SOAK_LIFECYCLE_OPERATIONS
        || plan.teardown.len() > MAX_SOAK_LIFECYCLE_OPERATIONS
        || plan.final_oracles.len() != SoakFinalOracleKind::ALL.len()
    {
        return invalid_soak_workload(
            "materialized plan has invalid scale, duration, or bounded collection fields",
        );
    }
    if u32::try_from(plan.identities.len()) != Ok(plan.pane_count) {
        return invalid_soak_workload("materialized plan identity count does not match pane_count");
    }

    if plan.assets.is_empty() || plan.assets.len() > MAX_SOAK_ASSETS {
        return invalid_soak_workload("materialized plan asset count exceeds the corpus cap");
    }
    let mut asset_ids = BTreeSet::new();
    let mut asset_paths = BTreeSet::new();
    let mut assets_by_id = BTreeMap::new();
    let mut previous_asset_id: Option<&str> = None;
    for asset in &plan.assets {
        validate_soak_id("asset_id", &asset.asset_id)?;
        validate_relative_asset_path(&asset.path)?;
        validate_sha256(&asset.sha256)?;
        if asset.purpose.trim().is_empty()
            || asset.purpose.len() > MAX_SOAK_ARG_BYTES
            || asset.purpose.chars().any(char::is_control)
        {
            return invalid_soak_workload(format!(
                "asset {} has an invalid purpose",
                asset.asset_id
            ));
        }
        if previous_asset_id.is_some_and(|previous| previous >= asset.asset_id.as_str()) {
            return invalid_soak_workload("materialized plan assets are not canonical");
        }
        if !asset_ids.insert(asset.asset_id.as_str()) || !asset_paths.insert(asset.path.as_str()) {
            return invalid_soak_workload("materialized plan has duplicate assets");
        }
        assets_by_id.insert(asset.asset_id.as_str(), asset);
        previous_asset_id = Some(asset.asset_id.as_str());
    }

    let mut identity_ids = BTreeSet::new();
    let mut window_ids = BTreeSet::new();
    let mut tab_ids = BTreeSet::new();
    let mut pane_ids = BTreeSet::new();
    let mut actor_seeds = BTreeSet::new();
    let mut actor_instances = BTreeMap::<&str, u32>::new();
    let mut actor_template_digests = BTreeMap::<&str, String>::new();
    let mut dimension_actor_ids = BTreeMap::<SoakWorkloadDimension, &str>::new();
    let mut identity_dimensions = BTreeSet::new();
    let mut referenced_asset_ids = BTreeSet::new();
    let mut previous_actor_id: Option<&str> = None;
    let mut calculated_output = 0_u64;
    let mut interactive_count = 0_u32;
    let workspace_id = format!("{}-{}p-workspace", plan.corpus_id, plan.pane_count);
    let expected_interactive_slots = select_soak_interactive_slots(
        &plan.identities,
        plan.interactive_pane_count,
        &plan.interactive_dimension_priority,
    )?;
    for (slot, identity) in plan.identities.iter().enumerate() {
        let fleet_slot = u32::try_from(slot)
            .map_err(|_| SoakWorkloadCorpusError::Invalid("fleet slot does not fit u32".into()))?;
        let expected_identity_id = format!(
            "{}-{}p-slot-{fleet_slot:04}",
            plan.corpus_id, plan.pane_count
        );
        let expected_window_id = format!(
            "{workspace_id}-window-{:03}",
            fleet_slot % plan.window_count
        );
        let expected_tab_id = format!("{workspace_id}-tab-{:03}", fleet_slot % plan.tab_count);
        let expected_pane_id = format!("{workspace_id}-pane-{fleet_slot:04}");
        if fleet_slot != identity.fleet_slot
            || identity.identity_id != expected_identity_id
            || identity.window_id != expected_window_id
            || identity.tab_id != expected_tab_id
            || identity.pane_id != expected_pane_id
            || !identity_ids.insert(identity.identity_id.as_str())
            || !pane_ids.insert(identity.pane_id.as_str())
            || !actor_seeds.insert(identity.actor_seed)
        {
            return invalid_soak_workload(
                "materialized plan identity map is not canonical or unique",
            );
        }
        window_ids.insert(identity.window_id.as_str());
        tab_ids.insert(identity.tab_id.as_str());
        validate_soak_id("actor_id", &identity.actor_id)?;
        validate_soak_id("payload_profile", &identity.payload_profile)?;
        if identity.output_bytes_per_second > MAX_SOAK_ACTOR_OUTPUT_BYTES_PER_SECOND {
            return invalid_soak_workload(format!(
                "identity {} exceeds the per-actor output safety cap",
                identity.identity_id
            ));
        }
        if previous_actor_id.is_some_and(|previous| previous > identity.actor_id.as_str()) {
            return invalid_soak_workload("materialized plan actors are not canonical");
        }
        let expected_instance = actor_instances
            .entry(identity.actor_id.as_str())
            .or_insert(0);
        if identity.actor_instance != *expected_instance {
            return invalid_soak_workload(format!(
                "identity {} has a non-contiguous actor instance",
                identity.identity_id
            ));
        }
        *expected_instance = expected_instance.checked_add(1).ok_or_else(|| {
            SoakWorkloadCorpusError::Invalid("actor instance count overflow".into())
        })?;
        previous_actor_id = Some(identity.actor_id.as_str());
        let expected_seed = soak_actor_seed(
            plan.base_seed,
            plan.pane_count,
            &identity.actor_id,
            identity.actor_instance,
        );
        if identity.actor_seed != expected_seed {
            return invalid_soak_workload(format!(
                "identity {} has a non-canonical actor seed",
                identity.identity_id
            ));
        }
        validate_soak_actor_binding(
            &format!("identity {}", identity.identity_id),
            identity.dimension,
            identity.activation,
            identity.shutdown.as_ref(),
            &identity.command,
            &identity.asset_ids,
            &assets_by_id,
            identity.output_bytes_per_second,
            &identity.expected_final_marker,
        )?;
        referenced_asset_ids.extend(identity.asset_ids.iter().map(String::as_str));
        let actor_template_digest = digest_soak_value(&(
            identity.dimension,
            identity.activation,
            &identity.shutdown,
            &identity.command,
            &identity.asset_ids,
            &identity.payload_profile,
            identity.output_bytes_per_second,
            &identity.expected_final_marker,
        ))?;
        match actor_template_digests.entry(identity.actor_id.as_str()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(actor_template_digest);
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if entry.get() != &actor_template_digest =>
            {
                return invalid_soak_workload(format!(
                    "actor {} has inconsistent materialized templates",
                    identity.actor_id
                ));
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
        if let Some(existing_actor_id) =
            dimension_actor_ids.insert(identity.dimension, identity.actor_id.as_str())
            && existing_actor_id != identity.actor_id.as_str()
        {
            return invalid_soak_workload(format!(
                "dimension {:?} is assigned to more than one actor",
                identity.dimension
            ));
        }
        if identity.asset_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return invalid_soak_workload(format!(
                "identity {} asset ids are not canonical",
                identity.identity_id
            ));
        }
        if identity.interactive != expected_interactive_slots.contains(&fleet_slot) {
            return invalid_soak_workload(format!(
                "identity {} has a non-canonical interactive designation",
                identity.identity_id
            ));
        }
        if identity.interactive {
            interactive_count = interactive_count.checked_add(1).ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid("interactive pane count overflow".into())
            })?;
        }
        calculated_output = calculated_output
            .checked_add(identity.output_bytes_per_second)
            .ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid(
                    "materialized output envelope addition overflow".into(),
                )
            })?;
        identity_dimensions.insert(identity.dimension);
    }
    let required_dimensions = SoakWorkloadDimension::ALL
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if interactive_count != plan.interactive_pane_count
        || calculated_output != plan.aggregate_output_bytes_per_second
        || u32::try_from(window_ids.len()) != Ok(plan.window_count)
        || u32::try_from(tab_ids.len()) != Ok(plan.tab_count)
        || identity_dimensions != required_dimensions
        || referenced_asset_ids != asset_ids
    {
        return invalid_soak_workload(
            "materialized plan layout, coverage, interactive, or output envelope is invalid",
        );
    }

    let mut canonical_phases = plan.phases.clone();
    canonicalize_soak_phases(&mut canonical_phases);
    if canonical_phases != plan.phases {
        return invalid_soak_workload("materialized plan phases are not canonical");
    }
    let cycle_duration_ms = validate_soak_phases(&plan.phases, &required_dimensions)?;
    if plan.cycle_duration_ms != cycle_duration_ms {
        return invalid_soak_workload("materialized plan cycle duration does not match phases");
    }
    let expected_actions = build_soak_actions(&plan.phases, &plan.identities)?;
    if plan.actions != expected_actions {
        return invalid_soak_workload("materialized plan actions do not match the phase contract");
    }

    let expected_setup = build_soak_setup(
        &workspace_id,
        plan.window_count,
        plan.tab_count,
        &plan.identities,
    );
    let expected_teardown = build_soak_teardown(
        &workspace_id,
        plan.window_count,
        plan.tab_count,
        &plan.identities,
    );
    if plan.setup != expected_setup || plan.teardown != expected_teardown {
        return invalid_soak_workload(
            "materialized plan lifecycle resources, parents, or ordering are not canonical",
        );
    }
    validate_materialized_oracles(&plan.final_oracles)?;
    if plan
        .final_oracles
        .windows(2)
        .any(|pair| pair[0].oracle >= pair[1].oracle)
    {
        return invalid_soak_workload("materialized plan final oracles are not canonical");
    }

    let expected_plan_sha256 = digest_soak_plan(plan)?;
    if plan.plan_sha256 != expected_plan_sha256 {
        return invalid_soak_workload(format!(
            "plan sha256 mismatch: expected {expected_plan_sha256}, got {}",
            plan.plan_sha256
        ));
    }
    Ok(())
}

/// Replay the logical plan, optionally failing one stable actor identity.
pub fn replay_soak_workload_plan(
    plan: &SoakWorkloadPlan,
    failed_identity_id: Option<&str>,
) -> Result<SoakWorkloadReplaySummary, SoakWorkloadCorpusError> {
    validate_soak_workload_plan(plan)?;
    if let Some(identity_id) = failed_identity_id
        && !plan
            .identities
            .iter()
            .any(|identity| identity.identity_id == identity_id)
    {
        return invalid_soak_workload(format!("unknown failed identity {identity_id}"));
    }

    let mut state = SoakLogicalState::default();
    let mut applied_setup = BTreeSet::new();
    for operation in &plan.setup {
        if applied_setup.insert(operation.operation_id.as_str()) {
            apply_soak_lifecycle(&mut state, operation);
        }
    }

    let mut action_counts = BTreeMap::new();
    let mut actions_executed = 0_u64;
    let mut actions_skipped = 0_u64;
    for action in &plan.actions {
        if failed_identity_id == Some(action.identity_id.as_str()) {
            actions_skipped = actions_skipped.checked_add(1).ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid("skipped action count overflow".into())
            })?;
        } else {
            actions_executed = actions_executed.checked_add(1).ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid("executed action count overflow".into())
            })?;
            let dimension_count = action_counts.entry(action.dimension).or_insert(0_u64);
            *dimension_count = dimension_count.checked_add(1).ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid("dimension action count overflow".into())
            })?;
        }
    }

    let mut applied_teardown = BTreeSet::new();
    for operation in &plan.teardown {
        if applied_teardown.insert(operation.operation_id.as_str()) {
            apply_soak_lifecycle(&mut state, operation);
        }
    }
    let all_dimensions_observed = SoakWorkloadDimension::ALL
        .iter()
        .all(|dimension| action_counts.get(dimension).copied().unwrap_or(0) > 0);
    let teardown_complete = state.workspaces.is_empty()
        && state.windows.is_empty()
        && state.tabs.is_empty()
        && state.panes.is_empty()
        && state.actors.is_empty();
    let settled_action_count = actions_executed
        .checked_add(actions_skipped)
        .ok_or_else(|| SoakWorkloadCorpusError::Invalid("settled action count overflow".into()))?;
    let all_actions_settled = settled_action_count == soak_count_to_u64(plan.actions.len())?;
    let stable_identity_map = plan
        .identities
        .iter()
        .enumerate()
        .all(|(slot, identity)| u32::try_from(slot) == Ok(identity.fleet_slot))
        && plan
            .identities
            .iter()
            .map(|identity| identity.identity_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            == plan.identities.len();
    let mut logical_oracle_results = BTreeMap::new();
    logical_oracle_results.insert(
        SoakFinalOracleKind::NoOwnedWorkspace,
        state.workspaces.is_empty(),
    );
    logical_oracle_results.insert(SoakFinalOracleKind::NoLiveActors, state.actors.is_empty());
    logical_oracle_results.insert(SoakFinalOracleKind::NoOpenPanes, state.panes.is_empty());
    logical_oracle_results.insert(SoakFinalOracleKind::NoOpenTabs, state.tabs.is_empty());
    logical_oracle_results.insert(SoakFinalOracleKind::NoOpenWindows, state.windows.is_empty());
    logical_oracle_results.insert(SoakFinalOracleKind::NoPendingActions, all_actions_settled);
    logical_oracle_results.insert(
        SoakFinalOracleKind::AllDimensionsObserved,
        all_dimensions_observed,
    );
    logical_oracle_results.insert(
        SoakFinalOracleKind::StableFleetIdentityMap,
        stable_identity_map,
    );
    let production_runner_oracles = plan
        .final_oracles
        .iter()
        .filter(|oracle| oracle.authority == SoakOracleAuthority::ProductionRunner)
        .map(|oracle| oracle.oracle)
        .collect();
    let mut summary = SoakWorkloadReplaySummary {
        plan_sha256: plan.plan_sha256.clone(),
        failed_identity_id: failed_identity_id.map(str::to_owned),
        actions_executed,
        actions_skipped,
        action_counts_by_dimension: action_counts,
        remaining_workspaces: soak_count_to_u64(state.workspaces.len())?,
        remaining_windows: soak_count_to_u64(state.windows.len())?,
        remaining_tabs: soak_count_to_u64(state.tabs.len())?,
        remaining_panes: soak_count_to_u64(state.panes.len())?,
        remaining_actors: soak_count_to_u64(state.actors.len())?,
        all_dimensions_observed,
        teardown_complete,
        logical_oracle_results,
        production_runner_oracles,
        summary_sha256: String::new(),
    };
    summary.summary_sha256 = digest_soak_value(&summary)?;
    Ok(summary)
}

fn validate_soak_scale(
    scale: &SoakScaleSpec,
    actors: &BTreeMap<&str, &SoakActorSpec>,
) -> Result<(), SoakWorkloadCorpusError> {
    if scale.window_count == 0
        || scale.tab_count < scale.window_count
        || scale.tab_count % scale.window_count != 0
        || scale.tab_count > scale.pane_count
        || scale.interactive_pane_count == 0
        || scale.interactive_pane_count > scale.pane_count
        || scale.aggregate_output_bytes_per_second > MAX_SOAK_AGGREGATE_OUTPUT_BYTES_PER_SECOND
    {
        return invalid_soak_workload(format!(
            "{}-pane scale has invalid layout counts",
            scale.pane_count
        ));
    }
    let mut allocation_ids = BTreeSet::new();
    let mut allocated_panes = 0_u32;
    let mut interactive_eligible_panes = 0_u32;
    let mut expected_output = 0_u64;
    if scale.allocations.len() != actors.len() {
        return invalid_soak_workload(format!(
            "{}-pane scale allocation count does not match actor count",
            scale.pane_count
        ));
    }
    for allocation in &scale.allocations {
        let actor = actors.get(allocation.actor_id.as_str()).ok_or_else(|| {
            SoakWorkloadCorpusError::Invalid(format!(
                "{}-pane scale references unknown actor {}",
                scale.pane_count, allocation.actor_id
            ))
        })?;
        if allocation.pane_count == 0 || !allocation_ids.insert(allocation.actor_id.as_str()) {
            return invalid_soak_workload(format!(
                "{}-pane scale has zero or duplicate allocation {}",
                scale.pane_count, allocation.actor_id
            ));
        }
        allocated_panes = allocated_panes
            .checked_add(allocation.pane_count)
            .ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid(format!(
                    "{}-pane allocation count overflow",
                    scale.pane_count
                ))
            })?;
        if SOAK_INTERACTIVE_REQUIRED_DIMENSIONS.contains(&actor.dimension) {
            interactive_eligible_panes = interactive_eligible_panes
                .checked_add(allocation.pane_count)
                .ok_or_else(|| {
                    SoakWorkloadCorpusError::Invalid(format!(
                        "{}-pane interactive allocation count overflow",
                        scale.pane_count
                    ))
                })?;
        }
        let actor_output = actor
            .output_bytes_per_second
            .checked_mul(u64::from(allocation.pane_count))
            .ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid(format!(
                    "{}-pane output envelope multiplication overflow",
                    scale.pane_count
                ))
            })?;
        expected_output = expected_output.checked_add(actor_output).ok_or_else(|| {
            SoakWorkloadCorpusError::Invalid(format!(
                "{}-pane output envelope addition overflow",
                scale.pane_count
            ))
        })?;
    }
    if allocated_panes != scale.pane_count {
        return invalid_soak_workload(format!(
            "{}-pane scale allocations sum to {allocated_panes}",
            scale.pane_count
        ));
    }
    if interactive_eligible_panes < scale.interactive_pane_count {
        return invalid_soak_workload(format!(
            "{}-pane scale requests more interactive panes than eligible actors",
            scale.pane_count
        ));
    }
    if allocation_ids.len() != actors.len() {
        return invalid_soak_workload(format!(
            "{}-pane scale does not allocate every actor",
            scale.pane_count
        ));
    }
    if expected_output != scale.aggregate_output_bytes_per_second {
        return invalid_soak_workload(format!(
            "{}-pane aggregate output is {}, expected {expected_output}",
            scale.pane_count, scale.aggregate_output_bytes_per_second
        ));
    }
    Ok(())
}

fn canonicalize_soak_phases(phases: &mut [SoakPhaseSpec]) {
    for phase in phases.iter_mut() {
        phase.dimensions.sort_unstable();
    }
    phases.sort_by(|left, right| {
        (left.start_offset_ms, left.phase_id.as_str())
            .cmp(&(right.start_offset_ms, right.phase_id.as_str()))
    });
}

fn validate_soak_interactive_priority(
    priority: &[SoakWorkloadDimension],
) -> Result<(), SoakWorkloadCorpusError> {
    let observed = priority.iter().copied().collect::<BTreeSet<_>>();
    let required = SOAK_INTERACTIVE_REQUIRED_DIMENSIONS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if observed != required || observed.len() != priority.len() {
        return invalid_soak_workload(
            "interactive priority must contain every required interactive dimension exactly once",
        );
    }
    Ok(())
}

fn select_soak_interactive_slots(
    identities: &[SoakFleetIdentity],
    interactive_pane_count: u32,
    priority: &[SoakWorkloadDimension],
) -> Result<BTreeSet<u32>, SoakWorkloadCorpusError> {
    validate_soak_interactive_priority(priority)?;
    let target = usize::try_from(interactive_pane_count).map_err(|_| {
        SoakWorkloadCorpusError::Invalid("interactive pane count does not fit usize".into())
    })?;
    let mut selected = BTreeSet::new();
    while selected.len() < target {
        let before = selected.len();
        for dimension in priority {
            if let Some(identity) = identities.iter().find(|identity| {
                identity.dimension == *dimension && !selected.contains(&identity.fleet_slot)
            }) {
                selected.insert(identity.fleet_slot);
                if selected.len() == target {
                    break;
                }
            }
        }
        if selected.len() == before {
            return invalid_soak_workload(
                "interactive pane count exceeds the eligible deterministic actors",
            );
        }
    }
    Ok(selected)
}

fn validate_soak_phases(
    phases: &[SoakPhaseSpec],
    required_dimensions: &BTreeSet<SoakWorkloadDimension>,
) -> Result<u64, SoakWorkloadCorpusError> {
    if phases.is_empty() || phases.len() > MAX_SOAK_PHASES {
        return invalid_soak_workload("phase count is empty or exceeds the corpus cap");
    }
    let mut phase_ids = BTreeSet::new();
    let mut phase_dimensions = BTreeSet::new();
    let mut previous_end = 0_u64;
    let mut ordered = phases.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        (left.start_offset_ms, left.phase_id.as_str())
            .cmp(&(right.start_offset_ms, right.phase_id.as_str()))
    });
    for phase in ordered {
        validate_soak_id("phase_id", &phase.phase_id)?;
        if !phase_ids.insert(phase.phase_id.as_str()) {
            return invalid_soak_workload(format!("duplicate phase {}", phase.phase_id));
        }
        if phase.duration_ms == 0
            || phase.action_cadence_ms == 0
            || phase.action_cadence_ms > phase.duration_ms
            || phase.duration_ms % phase.action_cadence_ms != 0
        {
            return invalid_soak_workload(format!(
                "phase {} has an invalid duration/cadence",
                phase.phase_id
            ));
        }
        if phase.start_offset_ms != previous_end {
            return invalid_soak_workload(format!(
                "phase {} does not begin at the preceding phase boundary {previous_end}",
                phase.phase_id
            ));
        }
        previous_end = phase
            .start_offset_ms
            .checked_add(phase.duration_ms)
            .ok_or_else(|| {
                SoakWorkloadCorpusError::Invalid(format!(
                    "phase {} end overflows u64",
                    phase.phase_id
                ))
            })?;
        if previous_end > MAX_SOAK_CYCLE_MS {
            return invalid_soak_workload(
                "workload cycle exceeds the one-hour materialization cap",
            );
        }
        let dimensions = phase.dimensions.iter().copied().collect::<BTreeSet<_>>();
        if dimensions.is_empty() || dimensions.len() != phase.dimensions.len() {
            return invalid_soak_workload(format!(
                "phase {} has no dimensions or repeats a dimension",
                phase.phase_id
            ));
        }
        phase_dimensions.extend(dimensions);
    }
    if phase_dimensions != *required_dimensions {
        return invalid_soak_workload("phase schedule does not cover every required dimension");
    }
    Ok(previous_end)
}

fn validate_soak_actor_binding(
    label: &str,
    dimension: SoakWorkloadDimension,
    activation: SoakActorActivation,
    shutdown: Option<&SoakPersistentShutdown>,
    command: &SoakActorCommand,
    asset_ids: &[String],
    assets_by_id: &BTreeMap<&str, &SoakWorkloadAsset>,
    output_bytes_per_second: u64,
    expected_final_marker: &str,
) -> Result<(), SoakWorkloadCorpusError> {
    let (expected_driver, expected_program) = match dimension {
        SoakWorkloadDimension::QuietShell => (SoakActorDriver::Builtin, "ft.soak.quiet-shell.v1"),
        SoakWorkloadDimension::EditorTui => (
            SoakActorDriver::IsolatedArgv,
            "fixtures/e2e/dummy_alt_screen.sh",
        ),
        SoakWorkloadDimension::BuildTestOutput => {
            (SoakActorDriver::FixtureReplay, "ft.soak.fixture-replay.v1")
        }
        SoakWorkloadDimension::ProgressRedraw => {
            (SoakActorDriver::Builtin, "ft.soak.progress-redraw.v1")
        }
        SoakWorkloadDimension::AgentLikeStream => {
            (SoakActorDriver::IsolatedArgv, "fixtures/e2e/dummy_agent.sh")
        }
        SoakWorkloadDimension::Images => {
            (SoakActorDriver::FixtureReplay, "ft.soak.image-replay.v1")
        }
        SoakWorkloadDimension::GlyphDiversity => {
            (SoakActorDriver::FixtureReplay, "ft.soak.glyph-replay.v1")
        }
        SoakWorkloadDimension::Search => (SoakActorDriver::Builtin, "ft.soak.search.v1"),
        SoakWorkloadDimension::Capture => {
            (SoakActorDriver::FixtureReplay, "ft.soak.capture-replay.v1")
        }
        SoakWorkloadDimension::Workflow => (SoakActorDriver::Builtin, "ft.soak.workflow.v1"),
        SoakWorkloadDimension::Maintenance => (SoakActorDriver::Builtin, "ft.soak.maintenance.v1"),
        SoakWorkloadDimension::LayoutChurn => {
            (SoakActorDriver::FixtureReplay, "ft.soak.layout-replay.v1")
        }
        SoakWorkloadDimension::ResizeZoom => {
            (SoakActorDriver::FixtureReplay, "ft.soak.resize-replay.v1")
        }
        SoakWorkloadDimension::Reconnect => (SoakActorDriver::Builtin, "ft.soak.reconnect.v1"),
        SoakWorkloadDimension::OutputBurst => {
            (SoakActorDriver::IsolatedArgv, "fixtures/e2e/dummy_burst.sh")
        }
    };
    let expected_activation = if matches!(
        dimension,
        SoakWorkloadDimension::EditorTui | SoakWorkloadDimension::AgentLikeStream
    ) {
        SoakActorActivation::Persistent
    } else {
        SoakActorActivation::Scheduled
    };
    if command.driver != expected_driver
        || command.program != expected_program
        || activation != expected_activation
        || output_bytes_per_second != expected_soak_output_bytes_per_second(dimension)
    {
        return invalid_soak_workload(format!(
            "{label} does not use the registered {:?} adapter or output envelope {expected_program}",
            dimension
        ));
    }
    if activation == SoakActorActivation::Persistent
        && command.driver != SoakActorDriver::IsolatedArgv
    {
        return invalid_soak_workload(format!(
            "{label} persistent activation requires an isolated argv driver"
        ));
    }
    match (activation, shutdown) {
        (SoakActorActivation::Persistent, Some(SoakPersistentShutdown::StdinLine { line })) => {
            if line.trim().is_empty()
                || line.len() > MAX_SOAK_ARG_BYTES
                || line.chars().any(char::is_control)
            {
                return invalid_soak_workload(format!(
                    "{label} has an invalid persistent shutdown line"
                ));
            }
        }
        (SoakActorActivation::Persistent, Some(SoakPersistentShutdown::Interrupt)) => {}
        (SoakActorActivation::Persistent, None) => {
            return invalid_soak_workload(format!(
                "{label} persistent activation has no shutdown contract"
            ));
        }
        (SoakActorActivation::Scheduled, None) => {}
        (SoakActorActivation::Scheduled, Some(_)) => {
            return invalid_soak_workload(format!(
                "{label} scheduled activation must not declare persistent shutdown"
            ));
        }
    }
    if command.program.trim().is_empty()
        || command.program.chars().any(char::is_control)
        || command.program.len() > MAX_SOAK_ARG_BYTES
    {
        return invalid_soak_workload(format!("{label} has an invalid program"));
    }
    if command.driver != SoakActorDriver::IsolatedArgv {
        validate_soak_id("actor program", &command.program)?;
    }
    if command.args.len() > MAX_SOAK_ACTOR_ARGS
        || command.args.iter().any(|arg| {
            arg.is_empty() || arg.len() > MAX_SOAK_ARG_BYTES || arg.chars().any(char::is_control)
        })
    {
        return invalid_soak_workload(format!("{label} has invalid argv"));
    }
    if expected_final_marker.trim().is_empty()
        || expected_final_marker.len() > MAX_SOAK_ARG_BYTES
        || expected_final_marker.chars().any(char::is_control)
    {
        return invalid_soak_workload(format!("{label} has an invalid final marker"));
    }
    let mut referenced_assets = BTreeSet::new();
    for asset_id in asset_ids {
        if !assets_by_id.contains_key(asset_id.as_str()) {
            return invalid_soak_workload(format!("{label} references unknown asset {asset_id}"));
        }
        if !referenced_assets.insert(asset_id.as_str()) {
            return invalid_soak_workload(format!("{label} repeats asset {asset_id}"));
        }
    }
    if command.driver == SoakActorDriver::IsolatedArgv
        && !asset_ids
            .iter()
            .filter_map(|asset_id| assets_by_id.get(asset_id.as_str()))
            .any(|asset| asset.path == command.program && asset.executable)
    {
        return invalid_soak_workload(format!(
            "{label} program is not one of its pinned executable assets"
        ));
    }
    if command.driver == SoakActorDriver::FixtureReplay {
        let replay_assets = command
            .args
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if replay_assets.is_empty()
            || replay_assets.len() != command.args.len()
            || replay_assets != referenced_assets
        {
            return invalid_soak_workload(format!(
                "{label} fixture argv must name every pinned asset exactly once"
            ));
        }
    }
    validate_isolated_soak_argv(label, dimension, shutdown, command, expected_final_marker)?;
    Ok(())
}

fn validate_isolated_soak_argv(
    label: &str,
    dimension: SoakWorkloadDimension,
    shutdown: Option<&SoakPersistentShutdown>,
    command: &SoakActorCommand,
    expected_final_marker: &str,
) -> Result<(), SoakWorkloadCorpusError> {
    if command.driver != SoakActorDriver::IsolatedArgv {
        return Ok(());
    }
    match (dimension, command.args.as_slice()) {
        (SoakWorkloadDimension::EditorTui, [duration]) => {
            parse_bounded_soak_u64(label, "duration", duration, 1, 7_200)?;
            if shutdown != Some(&SoakPersistentShutdown::Interrupt)
                || expected_final_marker != "Exited alternate screen mode."
            {
                return invalid_soak_workload(format!(
                    "{label} has an invalid editor shutdown or final marker contract"
                ));
            }
        }
        (
            SoakWorkloadDimension::AgentLikeStream,
            [delay, repeats, repeat_interval, read_timeout],
        ) => {
            let delay = parse_bounded_soak_u64(label, "delay", delay, 0, 60)?;
            let repeats = parse_bounded_soak_u64(label, "repeat count", repeats, 1, 100)?;
            let repeat_interval =
                parse_bounded_soak_u64(label, "repeat interval", repeat_interval, 0, 60)?;
            let read_timeout =
                parse_bounded_soak_u64(label, "read timeout", read_timeout, 1, 7_200)?;
            let repeat_delay = repeats
                .checked_sub(1)
                .and_then(|count| count.checked_mul(repeat_interval))
                .ok_or_else(|| {
                    SoakWorkloadCorpusError::Invalid(format!("{label} prelude duration overflow"))
                })?;
            let lifetime = delay
                .checked_add(repeat_delay)
                .and_then(|value| value.checked_add(read_timeout))
                .ok_or_else(|| {
                    SoakWorkloadCorpusError::Invalid(format!("{label} lifetime overflow"))
                })?;
            if lifetime > 7_200 {
                return invalid_soak_workload(format!(
                    "{label} exceeds the isolated actor lifetime cap"
                ));
            }
            let graceful_shutdown = matches!(
                shutdown,
                Some(SoakPersistentShutdown::StdinLine { line }) if line == "exit"
            );
            if !graceful_shutdown || expected_final_marker != "[CODEX] Session ended" {
                return invalid_soak_workload(format!(
                    "{label} has an invalid agent shutdown or final marker contract"
                ));
            }
        }
        (SoakWorkloadDimension::OutputBurst, [count, marker]) => {
            parse_bounded_soak_u64(label, "burst count", count, 1, 1_000_000)?;
            if marker.len() > 128 || marker != expected_final_marker {
                return invalid_soak_workload(format!(
                    "{label} burst marker is too long or does not match its final marker"
                ));
            }
        }
        _ => {
            return invalid_soak_workload(format!("{label} has an invalid isolated argv contract"));
        }
    }
    Ok(())
}

const fn expected_soak_output_bytes_per_second(dimension: SoakWorkloadDimension) -> u64 {
    match dimension {
        SoakWorkloadDimension::QuietShell => 0,
        SoakWorkloadDimension::EditorTui => 512,
        SoakWorkloadDimension::BuildTestOutput => 32_768,
        SoakWorkloadDimension::ProgressRedraw => 8_192,
        SoakWorkloadDimension::AgentLikeStream => 4_096,
        SoakWorkloadDimension::Images | SoakWorkloadDimension::Capture => 2_048,
        SoakWorkloadDimension::GlyphDiversity => 4_096,
        SoakWorkloadDimension::Search | SoakWorkloadDimension::Workflow => 512,
        SoakWorkloadDimension::Maintenance => 256,
        SoakWorkloadDimension::LayoutChurn
        | SoakWorkloadDimension::ResizeZoom
        | SoakWorkloadDimension::Reconnect => 128,
        SoakWorkloadDimension::OutputBurst => 802_688,
    }
}

fn parse_bounded_soak_u64(
    label: &str,
    field: &str,
    value: &str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, SoakWorkloadCorpusError> {
    if value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return invalid_soak_workload(format!("{label} has an invalid {field}"));
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| SoakWorkloadCorpusError::Invalid(format!("{label} has an invalid {field}")))?;
    if !(minimum..=maximum).contains(&parsed) {
        return invalid_soak_workload(format!("{label} has an out-of-range {field}"));
    }
    Ok(parsed)
}

fn validate_materialized_oracles(
    oracles: &[SoakFinalOracleSpec],
) -> Result<(), SoakWorkloadCorpusError> {
    let kinds = oracles
        .iter()
        .map(|oracle| oracle.oracle)
        .collect::<BTreeSet<_>>();
    let required = SoakFinalOracleKind::ALL
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if kinds != required || kinds.len() != oracles.len() {
        return invalid_soak_workload("materialized plan final oracle set is invalid");
    }
    for oracle in oracles {
        let production = matches!(
            oracle.oracle,
            SoakFinalOracleKind::FinalTerminalStateHash
                | SoakFinalOracleKind::FinalLayoutStateHash
                | SoakFinalOracleKind::CaptureSearchQuiescent
                | SoakFinalOracleKind::NoOrphanChildren
                | SoakFinalOracleKind::NoOpenTransports
                | SoakFinalOracleKind::ResourceOwnershipAtBaseline
        );
        let expected = if production {
            SoakOracleAuthority::ProductionRunner
        } else {
            SoakOracleAuthority::LogicalReplay
        };
        if oracle.authority != expected {
            return invalid_soak_workload(format!(
                "materialized oracle {:?} has wrong authority",
                oracle.oracle
            ));
        }
    }
    Ok(())
}

fn validate_soak_id(field: &str, value: &str) -> Result<(), SoakWorkloadCorpusError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte)
        })
    {
        return invalid_soak_workload(format!("{field} has invalid identifier {value:?}"));
    }
    Ok(())
}

fn validate_relative_asset_path(path: &str) -> Result<(), SoakWorkloadCorpusError> {
    let parsed = Path::new(path);
    if path.is_empty()
        || path.len() > MAX_SOAK_ARG_BYTES
        || path.contains('\\')
        || path.split('/').any(str::is_empty)
        || path.chars().any(char::is_control)
        || parsed.is_absolute()
        || parsed
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return invalid_soak_workload(format!("invalid asset path {path:?}"));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), SoakWorkloadCorpusError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid_soak_workload(format!("invalid sha256 {value:?}"));
    }
    Ok(())
}

fn invalid_soak_workload<T>(message: impl Into<String>) -> Result<T, SoakWorkloadCorpusError> {
    Err(SoakWorkloadCorpusError::Invalid(message.into()))
}

fn soak_actor_seed(base_seed: u64, panes: u32, actor_id: &str, instance: u32) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"frankenterm:soak-workload-actor:v1\0");
    hasher.update(base_seed.to_be_bytes());
    hasher.update(panes.to_be_bytes());
    hasher.update(actor_id.as_bytes());
    hasher.update(instance.to_be_bytes());
    let digest = hasher.finalize();
    let mut seed_bytes = [0_u8; 8];
    seed_bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(seed_bytes)
}

fn digest_soak_value<T: Serialize>(value: &T) -> Result<String, SoakWorkloadCorpusError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn digest_soak_plan(plan: &SoakWorkloadPlan) -> Result<String, SoakWorkloadCorpusError> {
    let mut unsigned = plan.clone();
    unsigned.plan_sha256.clear();
    digest_soak_value(&unsigned)
}

fn build_soak_actions(
    phases: &[SoakPhaseSpec],
    identities: &[SoakFleetIdentity],
) -> Result<Vec<SoakScheduledAction>, SoakWorkloadCorpusError> {
    let mut actions = Vec::new();
    for phase in phases {
        let active = phase.dimensions.iter().copied().collect::<BTreeSet<_>>();
        let repeats = phase.duration_ms / phase.action_cadence_ms;
        for identity in identities {
            if !active.contains(&identity.dimension) {
                continue;
            }
            for sequence in 0..repeats {
                let at_ms = phase
                    .start_offset_ms
                    .checked_add(sequence.checked_mul(phase.action_cadence_ms).ok_or_else(
                        || {
                            SoakWorkloadCorpusError::Invalid(
                                "scheduled action time multiplication overflow".into(),
                            )
                        },
                    )?)
                    .ok_or_else(|| {
                        SoakWorkloadCorpusError::Invalid(
                            "scheduled action time addition overflow".into(),
                        )
                    })?;
                if actions.len() >= MAX_SOAK_ACTIONS {
                    return invalid_soak_workload("materialized action count exceeds safety cap");
                }
                actions.push(SoakScheduledAction {
                    action_id: format!("{}-{}-{sequence:06}", phase.phase_id, identity.identity_id),
                    phase_id: phase.phase_id.clone(),
                    at_ms,
                    identity_id: identity.identity_id.clone(),
                    actor_id: identity.actor_id.clone(),
                    dimension: identity.dimension,
                    payload_profile: identity.payload_profile.clone(),
                });
            }
        }
    }
    actions.sort_by(|left, right| {
        (
            left.at_ms,
            left.identity_id.as_str(),
            left.action_id.as_str(),
        )
            .cmp(&(
                right.at_ms,
                right.identity_id.as_str(),
                right.action_id.as_str(),
            ))
    });
    if actions.is_empty() {
        return invalid_soak_workload("materialized plan has no actions");
    }
    Ok(actions)
}

fn build_soak_setup(
    workspace_id: &str,
    window_count: u32,
    tab_count: u32,
    identities: &[SoakFleetIdentity],
) -> Vec<SoakLifecycleOperation> {
    let mut operations = vec![SoakLifecycleOperation {
        operation_id: format!("setup-acquire-{workspace_id}"),
        kind: SoakLifecycleKind::AcquireWorkspace,
        resource_id: workspace_id.to_owned(),
        parent_id: None,
    }];
    for window_index in 0..window_count {
        let window_id = format!("{workspace_id}-window-{window_index:03}");
        operations.push(SoakLifecycleOperation {
            operation_id: format!("setup-open-{window_id}"),
            kind: SoakLifecycleKind::OpenWindow,
            resource_id: window_id,
            parent_id: Some(workspace_id.to_owned()),
        });
    }
    for tab_index in 0..tab_count {
        let tab_id = format!("{workspace_id}-tab-{tab_index:03}");
        operations.push(SoakLifecycleOperation {
            operation_id: format!("setup-open-{tab_id}"),
            kind: SoakLifecycleKind::OpenTab,
            resource_id: tab_id,
            parent_id: Some(format!(
                "{workspace_id}-window-{:03}",
                tab_index % window_count
            )),
        });
    }
    for identity in identities {
        operations.push(SoakLifecycleOperation {
            operation_id: format!("setup-open-{}", identity.pane_id),
            kind: SoakLifecycleKind::OpenPane,
            resource_id: identity.pane_id.clone(),
            parent_id: Some(identity.tab_id.clone()),
        });
        operations.push(SoakLifecycleOperation {
            operation_id: format!("setup-start-{}", identity.identity_id),
            kind: SoakLifecycleKind::StartActor,
            resource_id: identity.identity_id.clone(),
            parent_id: Some(identity.pane_id.clone()),
        });
    }
    operations
}

fn build_soak_teardown(
    workspace_id: &str,
    window_count: u32,
    tab_count: u32,
    identities: &[SoakFleetIdentity],
) -> Vec<SoakLifecycleOperation> {
    let mut operations = Vec::new();
    for identity in identities.iter().rev() {
        operations.push(SoakLifecycleOperation {
            operation_id: format!("teardown-stop-{}", identity.identity_id),
            kind: SoakLifecycleKind::StopActor,
            resource_id: identity.identity_id.clone(),
            parent_id: Some(identity.pane_id.clone()),
        });
        operations.push(SoakLifecycleOperation {
            operation_id: format!("teardown-close-{}", identity.pane_id),
            kind: SoakLifecycleKind::ClosePane,
            resource_id: identity.pane_id.clone(),
            parent_id: Some(identity.tab_id.clone()),
        });
    }
    for tab_index in (0..tab_count).rev() {
        let tab_id = format!("{workspace_id}-tab-{tab_index:03}");
        operations.push(SoakLifecycleOperation {
            operation_id: format!("teardown-close-{tab_id}"),
            kind: SoakLifecycleKind::CloseTab,
            resource_id: tab_id,
            parent_id: Some(format!(
                "{workspace_id}-window-{:03}",
                tab_index % window_count
            )),
        });
    }
    for window_index in (0..window_count).rev() {
        let window_id = format!("{workspace_id}-window-{window_index:03}");
        operations.push(SoakLifecycleOperation {
            operation_id: format!("teardown-close-{window_id}"),
            kind: SoakLifecycleKind::CloseWindow,
            resource_id: window_id,
            parent_id: Some(workspace_id.to_owned()),
        });
    }
    operations.push(SoakLifecycleOperation {
        operation_id: format!("teardown-release-{workspace_id}"),
        kind: SoakLifecycleKind::ReleaseWorkspace,
        resource_id: workspace_id.to_owned(),
        parent_id: None,
    });
    operations
}

#[derive(Default)]
struct SoakLogicalState {
    workspaces: BTreeSet<String>,
    windows: BTreeSet<String>,
    tabs: BTreeSet<String>,
    panes: BTreeSet<String>,
    actors: BTreeSet<String>,
}

fn apply_soak_lifecycle(state: &mut SoakLogicalState, operation: &SoakLifecycleOperation) {
    match operation.kind {
        SoakLifecycleKind::AcquireWorkspace => {
            state.workspaces.insert(operation.resource_id.clone());
        }
        SoakLifecycleKind::OpenWindow => {
            state.windows.insert(operation.resource_id.clone());
        }
        SoakLifecycleKind::OpenTab => {
            state.tabs.insert(operation.resource_id.clone());
        }
        SoakLifecycleKind::OpenPane => {
            state.panes.insert(operation.resource_id.clone());
        }
        SoakLifecycleKind::StartActor => {
            state.actors.insert(operation.resource_id.clone());
        }
        SoakLifecycleKind::StopActor => {
            state.actors.remove(&operation.resource_id);
        }
        SoakLifecycleKind::ClosePane => {
            state.panes.remove(&operation.resource_id);
        }
        SoakLifecycleKind::CloseTab => {
            state.tabs.remove(&operation.resource_id);
        }
        SoakLifecycleKind::CloseWindow => {
            state.windows.remove(&operation.resource_id);
        }
        SoakLifecycleKind::ReleaseWorkspace => {
            state.workspaces.remove(&operation.resource_id);
        }
    }
}

fn soak_count_to_u64(value: usize) -> Result<u64, SoakWorkloadCorpusError> {
    u64::try_from(value)
        .map_err(|_| SoakWorkloadCorpusError::Invalid("soak count does not fit u64".into()))
}

// =============================================================================
// Soak matrix
// =============================================================================

/// The full soak matrix: scenarios × workload profiles × injection profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakMatrix {
    /// Registered scenarios.
    pub scenarios: Vec<UserJourneyScenario>,
    /// Which workload profiles to test.
    pub workload_profiles: Vec<WorkloadProfile>,
    /// Which injection profiles to test.
    pub injection_profiles: Vec<FailureInjectionProfile>,
}

/// A custom legacy soak matrix cannot be expanded safely or deterministically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoakMatrixPlanError {
    EmptyAxis { axis: &'static str },
    DuplicateScenarioId { scenario_id: String },
    InvalidScenario { scenario_id: String },
    DuplicateWorkloadProfile,
    DuplicateInjectionProfile,
    CellCountOverflow,
    CellCountLimit { count: usize, maximum: usize },
}

impl std::fmt::Display for SoakMatrixPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyAxis { axis } => write!(formatter, "soak matrix {axis} axis is empty"),
            Self::DuplicateScenarioId { scenario_id } => {
                write!(formatter, "soak matrix repeats scenario id {scenario_id:?}")
            }
            Self::InvalidScenario { scenario_id } => {
                write!(formatter, "soak matrix scenario {scenario_id:?} is invalid")
            }
            Self::DuplicateWorkloadProfile => {
                write!(formatter, "soak matrix repeats a workload profile")
            }
            Self::DuplicateInjectionProfile => {
                write!(formatter, "soak matrix repeats an injection profile")
            }
            Self::CellCountOverflow => write!(formatter, "soak matrix cell count overflowed"),
            Self::CellCountLimit { count, maximum } => write!(
                formatter,
                "soak matrix has {count} cells, exceeding the {maximum}-cell cap"
            ),
        }
    }
}

impl std::error::Error for SoakMatrixPlanError {}

impl SoakMatrix {
    /// Create a default soak matrix with all standard scenarios and profiles.
    #[must_use]
    pub fn standard() -> Self {
        let scenarios: Vec<UserJourneyScenario> = JourneyCategory::ALL
            .iter()
            .zip(42_u64..)
            .map(|(cat, seed)| UserJourneyScenario {
                scenario_id: format!("soak-{}", cat.label().replace(' ', "-")),
                category: *cat,
                description: format!("Standard soak scenario for {}", cat.label()),
                expected_duration_ms: 60_000,
                blocking: cat.is_critical(),
                seed: Some(seed),
            })
            .collect();

        Self {
            scenarios,
            workload_profiles: WorkloadProfile::ALL.to_vec(),
            injection_profiles: FailureInjectionProfile::ALL.to_vec(),
        }
    }

    /// Create a minimal matrix for CI (fewer profiles, faster execution).
    #[must_use]
    pub fn ci_minimal() -> Self {
        let scenarios: Vec<UserJourneyScenario> = JourneyCategory::ALL
            .iter()
            .filter(|c| c.is_critical())
            .zip(100_u64..)
            .map(|(cat, seed)| UserJourneyScenario {
                scenario_id: format!("ci-soak-{}", cat.label().replace(' ', "-")),
                category: *cat,
                description: format!("CI soak for {}", cat.label()),
                expected_duration_ms: 10_000,
                blocking: true,
                seed: Some(seed),
            })
            .collect();

        Self {
            scenarios,
            workload_profiles: vec![WorkloadProfile::Steady, WorkloadProfile::Burst],
            injection_profiles: vec![
                FailureInjectionProfile::None,
                FailureInjectionProfile::Light,
            ],
        }
    }

    /// Custom matrix builder.
    #[must_use]
    pub fn custom(
        scenarios: Vec<UserJourneyScenario>,
        workload_profiles: Vec<WorkloadProfile>,
        injection_profiles: Vec<FailureInjectionProfile>,
    ) -> Self {
        Self {
            scenarios,
            workload_profiles,
            injection_profiles,
        }
    }

    /// Validated number of cells in the matrix (scenarios × workloads × injections).
    pub fn cell_count(&self) -> Result<usize, SoakMatrixPlanError> {
        if self.scenarios.is_empty() {
            return Err(SoakMatrixPlanError::EmptyAxis { axis: "scenario" });
        }
        if self.workload_profiles.is_empty() {
            return Err(SoakMatrixPlanError::EmptyAxis { axis: "workload" });
        }
        if self.injection_profiles.is_empty() {
            return Err(SoakMatrixPlanError::EmptyAxis { axis: "injection" });
        }
        let mut scenario_ids = BTreeSet::new();
        for scenario in &self.scenarios {
            if scenario.scenario_id.is_empty()
                || scenario.scenario_id.len() > MAX_SOAK_ARG_BYTES
                || scenario.scenario_id.chars().any(char::is_control)
                || scenario.description.trim().is_empty()
                || scenario.description.len() > 4_096
                || scenario.expected_duration_ms == 0
                || scenario.expected_duration_ms > MAX_SOAK_MATRIX_SCENARIO_MS
            {
                return Err(SoakMatrixPlanError::InvalidScenario {
                    scenario_id: scenario.scenario_id.clone(),
                });
            }
            if !scenario_ids.insert(scenario.scenario_id.as_str()) {
                return Err(SoakMatrixPlanError::DuplicateScenarioId {
                    scenario_id: scenario.scenario_id.clone(),
                });
            }
        }
        if self
            .workload_profiles
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != self.workload_profiles.len()
        {
            return Err(SoakMatrixPlanError::DuplicateWorkloadProfile);
        }
        if self
            .injection_profiles
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != self.injection_profiles.len()
        {
            return Err(SoakMatrixPlanError::DuplicateInjectionProfile);
        }
        let count = self
            .scenarios
            .len()
            .checked_mul(self.workload_profiles.len())
            .and_then(|count| count.checked_mul(self.injection_profiles.len()))
            .ok_or(SoakMatrixPlanError::CellCountOverflow)?;
        if count > MAX_SOAK_MATRIX_CELLS {
            return Err(SoakMatrixPlanError::CellCountLimit {
                count,
                maximum: MAX_SOAK_MATRIX_CELLS,
            });
        }
        Ok(count)
    }

    /// Generate the execution plan from this matrix.
    pub fn to_plan(&self) -> Result<SoakExecutionPlan, SoakMatrixPlanError> {
        let mut cells = Vec::with_capacity(self.cell_count()?);
        for scenario in &self.scenarios {
            for workload in &self.workload_profiles {
                for injection in &self.injection_profiles {
                    cells.push(SoakCell {
                        cell_id: format!("{}/{:?}/{:?}", scenario.scenario_id, workload, injection),
                        scenario_id: scenario.scenario_id.clone(),
                        category: scenario.category,
                        driver: scenario.category.into(),
                        expected_duration_ms: scenario.expected_duration_ms,
                        workload: *workload,
                        injection: *injection,
                        blocking: scenario.blocking,
                        seed: scenario.seed,
                    });
                }
            }
        }

        Ok(SoakExecutionPlan { cells })
    }

    /// Number of blocking scenarios.
    #[must_use]
    pub fn blocking_scenario_count(&self) -> usize {
        self.scenarios.iter().filter(|s| s.blocking).count()
    }
}

/// An executable soak plan derived from the matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakExecutionPlan {
    /// Individual cells to execute.
    pub cells: Vec<SoakCell>,
}

/// A single cell in the soak matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakCell {
    /// Unique cell identifier.
    pub cell_id: String,
    /// Which scenario this cell runs.
    pub scenario_id: String,
    /// Journey category.
    pub category: JourneyCategory,
    /// Typed runner adapter inherited from the scenario.
    pub driver: JourneyDriver,
    /// Bounded duration contract inherited from the scenario.
    pub expected_duration_ms: u64,
    /// Workload profile for this cell.
    pub workload: WorkloadProfile,
    /// Failure injection profile.
    pub injection: FailureInjectionProfile,
    /// Whether this cell is blocking.
    pub blocking: bool,
    /// Deterministic seed.
    pub seed: Option<u64>,
}

impl SoakExecutionPlan {
    /// Total cells.
    #[must_use]
    pub fn total_cells(&self) -> usize {
        self.cells.len()
    }

    /// Blocking cells only.
    #[must_use]
    pub fn blocking_cells(&self) -> Vec<&SoakCell> {
        self.cells.iter().filter(|c| c.blocking).collect()
    }
}

// =============================================================================
// Soak execution results
// =============================================================================

/// Complete results from executing a soak matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakExecutionResult {
    /// Per-cell results.
    pub cell_results: Vec<CellResult>,
    /// Soak-wide invariant checks.
    pub invariant_checks: Vec<SoakInvariantCheck>,
    /// Total soak duration (ms).
    pub total_duration_ms: u64,
    /// When this soak was started (epoch ms).
    pub started_at_ms: u64,
    /// When this soak completed (epoch ms).
    pub completed_at_ms: u64,
}

/// A completion timestamp cannot precede the corresponding soak start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakCompletionTimeError {
    pub started_at_ms: u64,
    pub completed_at_ms: u64,
}

impl std::fmt::Display for SoakCompletionTimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "soak completion {}ms precedes start {}ms",
            self.completed_at_ms, self.started_at_ms
        )
    }
}

impl std::error::Error for SoakCompletionTimeError {}

/// Result from executing a single soak cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellResult {
    /// Which cell this result is for.
    pub cell_id: String,
    /// Journey category.
    pub category: JourneyCategory,
    /// Workload profile.
    pub workload: WorkloadProfile,
    /// Injection profile.
    pub injection: FailureInjectionProfile,
    /// Whether this cell passed.
    pub passed: bool,
    /// Whether this cell was blocking.
    pub blocking: bool,
    /// Execution duration (ms).
    pub duration_ms: u64,
    /// Failure reason (if failed).
    pub failure_reason: Option<String>,
    /// Error rate during execution.
    pub error_rate: f64,
    /// P95 latency during execution (ms).
    pub p95_latency_ms: f64,
    /// Seed used.
    pub seed: Option<u64>,
    /// Structured telemetry from this cell.
    pub telemetry: CellTelemetry,
}

/// Telemetry captured during a single soak cell execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CellTelemetry {
    /// Total operations attempted.
    pub ops_attempted: u64,
    /// Operations that succeeded.
    pub ops_succeeded: u64,
    /// Operations that failed.
    pub ops_failed: u64,
    /// Tasks spawned.
    pub tasks_spawned: u64,
    /// Tasks completed normally.
    pub tasks_completed: u64,
    /// Tasks cancelled.
    pub tasks_cancelled: u64,
    /// Faults injected.
    pub faults_injected: u64,
    /// Recovery events.
    pub recoveries: u64,
    /// Deadlocks detected by the cell watchdog.
    pub deadlock_detected_count: u64,
}

/// A soak-wide invariant check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoakInvariantCheck {
    /// Invariant identifier.
    pub invariant_id: String,
    /// Human-readable description.
    pub description: String,
    /// Whether the invariant held.
    pub passed: bool,
    /// Evidence for the result.
    pub evidence: String,
    /// Whether this invariant is mandatory.
    pub mandatory: bool,
}

impl SoakExecutionResult {
    /// Create a new empty result.
    #[must_use]
    pub fn new(started_at_ms: u64) -> Self {
        Self {
            cell_results: Vec::new(),
            invariant_checks: Vec::new(),
            total_duration_ms: 0,
            started_at_ms,
            completed_at_ms: 0,
        }
    }

    /// Record a cell result.
    pub fn record_cell(&mut self, result: CellResult) {
        self.cell_results.push(result);
    }

    /// Record an invariant check.
    pub fn record_invariant(&mut self, check: SoakInvariantCheck) {
        self.invariant_checks.push(check);
    }

    /// Mark the soak as completed, rejecting a backwards timestamp without mutation.
    pub fn complete(&mut self, completed_at_ms: u64) -> Result<(), SoakCompletionTimeError> {
        if completed_at_ms < self.started_at_ms {
            return Err(SoakCompletionTimeError {
                started_at_ms: self.started_at_ms,
                completed_at_ms,
            });
        }
        self.completed_at_ms = completed_at_ms;
        self.total_duration_ms = completed_at_ms - self.started_at_ms;
        Ok(())
    }

    /// Count of passing cells.
    #[must_use]
    pub fn cells_passed(&self) -> usize {
        self.cell_results.iter().filter(|c| c.passed).count()
    }

    /// Count of failing cells.
    #[must_use]
    pub fn cells_failed(&self) -> usize {
        self.cell_results.iter().filter(|c| !c.passed).count()
    }

    /// Count of blocking cells that failed.
    #[must_use]
    pub fn blocking_failures(&self) -> usize {
        self.cell_results
            .iter()
            .filter(|c| c.blocking && !c.passed)
            .count()
    }

    /// Count of mandatory invariants that failed.
    #[must_use]
    pub fn mandatory_invariant_failures(&self) -> usize {
        self.invariant_checks
            .iter()
            .filter(|c| c.mandatory && !c.passed)
            .count()
    }

    /// Overall pass rate.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        if self.cell_results.is_empty() {
            return 0.0;
        }
        self.cells_passed() as f64 / self.cell_results.len() as f64
    }

    /// Aggregate error rate across all cells.
    #[must_use]
    pub fn aggregate_error_rate(&self) -> f64 {
        if self.cell_results.is_empty() {
            return 0.0;
        }
        if self
            .cell_results
            .iter()
            .any(|cell| !cell.error_rate.is_finite() || !(0.0..=1.0).contains(&cell.error_rate))
        {
            return 1.0;
        }
        let total: f64 = self.cell_results.iter().map(|c| c.error_rate).sum();
        total / self.cell_results.len() as f64
    }

    fn maximum_p95_latency_ms(&self) -> f64 {
        let mut maximum = 0.0_f64;
        for cell in &self.cell_results {
            if !cell.p95_latency_ms.is_finite() || cell.p95_latency_ms < 0.0 {
                return f64::MAX;
            }
            maximum = maximum.max(cell.p95_latency_ms);
        }
        maximum
    }

    fn evidence_shape_is_valid(&self) -> bool {
        let completion_is_valid = self.total_duration_ms > 0
            && self
                .completed_at_ms
                .checked_sub(self.started_at_ms)
                .is_some_and(|duration| duration == self.total_duration_ms);
        let mut cell_ids = BTreeSet::new();
        completion_is_valid
            && !self.cell_results.is_empty()
            && self.cell_results.iter().all(|cell| {
                !cell.cell_id.is_empty()
                    && cell.cell_id.len() <= MAX_SOAK_ARG_BYTES
                    && !cell.cell_id.chars().any(char::is_control)
                    && cell_ids.insert(cell.cell_id.as_str())
                    && cell.error_rate.is_finite()
                    && (0.0..=1.0).contains(&cell.error_rate)
                    && cell.p95_latency_ms.is_finite()
                    && cell.p95_latency_ms >= 0.0
                    && if cell.passed {
                        cell.failure_reason.is_none()
                    } else {
                        cell.failure_reason
                            .as_deref()
                            .is_some_and(|reason| !reason.trim().is_empty())
                    }
            })
    }

    fn standard_invariant_evidence_is_authoritative(&self) -> bool {
        AggregatedSoakTelemetry::from_cells(&self.cell_results)
            .map(|telemetry| self.invariant_checks == Self::standard_invariants(&telemetry))
            .unwrap_or(false)
    }

    /// Results grouped by journey category.
    #[must_use]
    pub fn by_category(&self) -> BTreeMap<JourneyCategory, Vec<&CellResult>> {
        let mut map: BTreeMap<JourneyCategory, Vec<&CellResult>> = BTreeMap::new();
        for result in &self.cell_results {
            map.entry(result.category).or_default().push(result);
        }
        map
    }

    /// Standard soak invariants to check after execution.
    #[must_use]
    pub fn standard_invariants(telemetry: &AggregatedSoakTelemetry) -> Vec<SoakInvariantCheck> {
        vec![
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-01".into(),
                description: "No task leaks — all spawned tasks completed or cancelled".into(),
                passed: telemetry
                    .tasks_completed
                    .checked_add(telemetry.tasks_cancelled)
                    == Some(telemetry.tasks_spawned)
                    && telemetry.cells_with_task_accounting_mismatch == 0,
                evidence: format!(
                    "spawned={}, completed={}, cancelled={}, mismatched_cells={}",
                    telemetry.tasks_spawned,
                    telemetry.tasks_completed,
                    telemetry.tasks_cancelled,
                    telemetry.cells_with_task_accounting_mismatch
                ),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-02".into(),
                description: "No deadlocks — all cells completed within timeout".into(),
                passed: telemetry.deadlock_detected_count == 0,
                evidence: format!("deadlocks_detected={}", telemetry.deadlock_detected_count),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-03".into(),
                description: "No message loss — ops_attempted == ops_succeeded + ops_failed".into(),
                passed: telemetry.ops_succeeded.checked_add(telemetry.ops_failed)
                    == Some(telemetry.ops_attempted)
                    && telemetry.cells_with_operation_accounting_mismatch == 0,
                evidence: format!(
                    "attempted={}, succeeded={}, failed={}, mismatched_cells={}",
                    telemetry.ops_attempted,
                    telemetry.ops_succeeded,
                    telemetry.ops_failed,
                    telemetry.cells_with_operation_accounting_mismatch
                ),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-04".into(),
                description: "No unbounded latency — p95 < 5000ms across all cells".into(),
                passed: telemetry.max_p95_latency_ms < 5000.0,
                evidence: format!("max_p95_latency_ms={:.1}", telemetry.max_p95_latency_ms),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-05".into(),
                description: "Recovery path exercised whenever aggregate faults were injected"
                    .into(),
                passed: telemetry.faults_injected == 0 || telemetry.recoveries > 0,
                evidence: format!(
                    "faults_injected={}, recoveries={}",
                    telemetry.faults_injected, telemetry.recoveries
                ),
                mandatory: false,
            },
        ]
    }
}

/// Aggregated telemetry across all soak cells.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregatedSoakTelemetry {
    pub ops_attempted: u64,
    pub ops_succeeded: u64,
    pub ops_failed: u64,
    pub tasks_spawned: u64,
    pub tasks_completed: u64,
    pub tasks_cancelled: u64,
    pub faults_injected: u64,
    pub recoveries: u64,
    pub deadlock_detected_count: u64,
    pub cells_with_task_accounting_mismatch: u64,
    pub cells_with_operation_accounting_mismatch: u64,
    pub max_p95_latency_ms: f64,
}

/// A cell metric could not be represented truthfully in aggregate telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoakTelemetryAggregationError {
    /// A per-cell counter sum exceeded the aggregate representation.
    CounterOverflow { field: &'static str },
    /// A latency sample was negative or non-finite.
    InvalidP95Latency { cell_index: usize },
}

impl std::fmt::Display for SoakTelemetryAggregationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CounterOverflow { field } => {
                write!(formatter, "soak telemetry counter overflowed: {field}")
            }
            Self::InvalidP95Latency { cell_index } => {
                write!(
                    formatter,
                    "soak cell at index {cell_index} has an invalid p95 latency"
                )
            }
        }
    }
}

impl std::error::Error for SoakTelemetryAggregationError {}

impl AggregatedSoakTelemetry {
    /// Aggregate from individual cell results, failing closed on counter overflow.
    pub fn from_cells(cells: &[CellResult]) -> Result<Self, SoakTelemetryAggregationError> {
        let mut agg = Self::default();
        for (cell_index, cell) in cells.iter().enumerate() {
            if cell
                .telemetry
                .tasks_completed
                .checked_add(cell.telemetry.tasks_cancelled)
                != Some(cell.telemetry.tasks_spawned)
            {
                checked_add_soak_telemetry(
                    &mut agg.cells_with_task_accounting_mismatch,
                    1,
                    "cells_with_task_accounting_mismatch",
                )?;
            }
            if cell
                .telemetry
                .ops_succeeded
                .checked_add(cell.telemetry.ops_failed)
                != Some(cell.telemetry.ops_attempted)
            {
                checked_add_soak_telemetry(
                    &mut agg.cells_with_operation_accounting_mismatch,
                    1,
                    "cells_with_operation_accounting_mismatch",
                )?;
            }
            checked_add_soak_telemetry(
                &mut agg.ops_attempted,
                cell.telemetry.ops_attempted,
                "ops_attempted",
            )?;
            checked_add_soak_telemetry(
                &mut agg.ops_succeeded,
                cell.telemetry.ops_succeeded,
                "ops_succeeded",
            )?;
            checked_add_soak_telemetry(
                &mut agg.ops_failed,
                cell.telemetry.ops_failed,
                "ops_failed",
            )?;
            checked_add_soak_telemetry(
                &mut agg.tasks_spawned,
                cell.telemetry.tasks_spawned,
                "tasks_spawned",
            )?;
            checked_add_soak_telemetry(
                &mut agg.tasks_completed,
                cell.telemetry.tasks_completed,
                "tasks_completed",
            )?;
            checked_add_soak_telemetry(
                &mut agg.tasks_cancelled,
                cell.telemetry.tasks_cancelled,
                "tasks_cancelled",
            )?;
            checked_add_soak_telemetry(
                &mut agg.faults_injected,
                cell.telemetry.faults_injected,
                "faults_injected",
            )?;
            checked_add_soak_telemetry(
                &mut agg.recoveries,
                cell.telemetry.recoveries,
                "recoveries",
            )?;
            checked_add_soak_telemetry(
                &mut agg.deadlock_detected_count,
                cell.telemetry.deadlock_detected_count,
                "deadlock_detected_count",
            )?;
            if !cell.p95_latency_ms.is_finite() || cell.p95_latency_ms < 0.0 {
                return Err(SoakTelemetryAggregationError::InvalidP95Latency { cell_index });
            }
            if cell.p95_latency_ms > agg.max_p95_latency_ms {
                agg.max_p95_latency_ms = cell.p95_latency_ms;
            }
        }
        Ok(agg)
    }
}

fn checked_add_soak_telemetry(
    aggregate: &mut u64,
    value: u64,
    field: &'static str,
) -> Result<(), SoakTelemetryAggregationError> {
    *aggregate = aggregate
        .checked_add(value)
        .ok_or(SoakTelemetryAggregationError::CounterOverflow { field })?;
    Ok(())
}

// =============================================================================
// Confidence gate
// =============================================================================

/// Confidence gate that evaluates soak results for cutover readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceGate {
    /// Minimum pass rate across all cells (0.0–1.0).
    pub min_pass_rate: f64,
    /// Maximum allowed aggregate error rate.
    pub max_error_rate: f64,
    /// Maximum allowed p95 latency (ms).
    pub max_p95_latency_ms: f64,
    /// Whether blocking cell failures are hard stops.
    pub blocking_failures_are_hard_stop: bool,
    /// Whether mandatory invariant failures are hard stops.
    pub mandatory_invariants_are_hard_stop: bool,
}

impl ConfidenceGate {
    /// Standard confidence gate with production-grade thresholds.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            min_pass_rate: 0.95,
            max_error_rate: 0.05,
            max_p95_latency_ms: 5000.0,
            blocking_failures_are_hard_stop: true,
            mandatory_invariants_are_hard_stop: true,
        }
    }

    /// Strict confidence gate (100% pass rate required).
    #[must_use]
    pub fn strict() -> Self {
        Self {
            min_pass_rate: 1.0,
            max_error_rate: 0.01,
            max_p95_latency_ms: 2000.0,
            blocking_failures_are_hard_stop: true,
            mandatory_invariants_are_hard_stop: true,
        }
    }

    /// Evaluate soak results against this gate.
    #[must_use]
    pub fn evaluate(&self, results: &SoakExecutionResult) -> ConfidenceVerdict {
        let mut checks = Vec::new();

        let configuration_valid = self.min_pass_rate.is_finite()
            && (0.0..=1.0).contains(&self.min_pass_rate)
            && self.max_error_rate.is_finite()
            && (0.0..=1.0).contains(&self.max_error_rate)
            && self.max_p95_latency_ms.is_finite()
            && self.max_p95_latency_ms >= 0.0;
        let evidence_shape_valid = results.evidence_shape_is_valid();
        checks.push(GateCondition {
            condition_id: "CONF-00-evidence-integrity".into(),
            description: "Gate configuration and soak evidence are structurally valid".into(),
            passed: configuration_valid && evidence_shape_valid,
            measured: format!(
                "configuration_valid={configuration_valid}, evidence_shape_valid={evidence_shape_valid}"
            ),
            blocking: true,
        });

        // Check 1: Pass rate.
        let pass_rate = results.pass_rate();
        checks.push(GateCondition {
            condition_id: "CONF-01-pass-rate".into(),
            description: format!("Pass rate >= {:.0}%", self.min_pass_rate * 100.0),
            passed: pass_rate >= self.min_pass_rate,
            measured: format!("{:.1}%", pass_rate * 100.0),
            blocking: true,
        });

        // Check 2: Blocking cell failures.
        let blocking_fails = results.blocking_failures();
        checks.push(GateCondition {
            condition_id: "CONF-02-blocking-cells".into(),
            description: "No blocking cell failures".into(),
            passed: blocking_fails == 0,
            measured: format!("{blocking_fails} blocking failures"),
            blocking: self.blocking_failures_are_hard_stop,
        });

        // Check 3: Mandatory invariants.
        let inv_fails = results.mandatory_invariant_failures();
        let invariant_evidence_authoritative =
            results.standard_invariant_evidence_is_authoritative();
        checks.push(GateCondition {
            condition_id: "CONF-03-invariants".into(),
            description: "Canonical invariant evidence is complete and all mandatory invariants hold"
                .into(),
            passed: invariant_evidence_authoritative && inv_fails == 0,
            measured: format!(
                "authoritative={invariant_evidence_authoritative}, {inv_fails} mandatory invariant failures"
            ),
            blocking: self.mandatory_invariants_are_hard_stop,
        });

        // Check 4: Error rate.
        let error_rate = results.aggregate_error_rate();
        checks.push(GateCondition {
            condition_id: "CONF-04-error-rate".into(),
            description: format!("Error rate <= {:.1}%", self.max_error_rate * 100.0),
            passed: error_rate <= self.max_error_rate,
            measured: format!("{:.2}%", error_rate * 100.0),
            blocking: false,
        });

        // Check 5: Latency.
        let max_latency = results.maximum_p95_latency_ms();
        checks.push(GateCondition {
            condition_id: "CONF-05-latency".into(),
            description: format!("Max p95 latency <= {:.0}ms", self.max_p95_latency_ms),
            passed: max_latency <= self.max_p95_latency_ms,
            measured: format!("{max_latency:.1}ms"),
            blocking: false,
        });

        // Determine verdict.
        let blocking_check_failures = checks.iter().filter(|c| c.blocking && !c.passed).count();
        let non_blocking_failures = checks.iter().filter(|c| !c.blocking && !c.passed).count();

        let decision = if blocking_check_failures > 0 {
            ConfidenceDecision::NotConfident
        } else if non_blocking_failures > 0 {
            ConfidenceDecision::ConditionallyConfident
        } else {
            ConfidenceDecision::Confident
        };

        ConfidenceVerdict {
            decision,
            checks,
            cells_total: results.cell_results.len(),
            cells_passed: results.cells_passed(),
            cells_failed: results.cells_failed(),
            soak_duration_ms: results.total_duration_ms,
        }
    }

    /// Convert soak results to a SoakOutcome for the cutover evidence package.
    #[must_use]
    pub fn to_evidence(
        &self,
        results: &SoakExecutionResult,
        period_id: impl Into<String>,
    ) -> SoakOutcome {
        let verdict = self.evaluate(results);
        SoakOutcome {
            period_id: period_id.into(),
            start_ms: results.started_at_ms,
            end_ms: results.completed_at_ms,
            slo_conforming: verdict.decision != ConfidenceDecision::NotConfident,
            error_rate: results.aggregate_error_rate(),
            p95_latency_ms: results.maximum_p95_latency_ms(),
            incident_count: u32::try_from(results.cells_failed()).unwrap_or(u32::MAX),
            rollback_triggered: false,
            notes: format!(
                "Soak verdict: {:?}. {}/{} cells passed.",
                verdict.decision, verdict.cells_passed, verdict.cells_total
            ),
        }
    }
}

/// Confidence verdict from gate evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceVerdict {
    /// The confidence decision.
    pub decision: ConfidenceDecision,
    /// Individual gate condition results.
    pub checks: Vec<GateCondition>,
    /// Total cells in the soak.
    pub cells_total: usize,
    /// Cells that passed.
    pub cells_passed: usize,
    /// Cells that failed.
    pub cells_failed: usize,
    /// Total soak duration.
    pub soak_duration_ms: u64,
}

/// Confidence decision outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceDecision {
    /// All gates pass — high confidence in cutover.
    Confident,
    /// No blocking failures, but some non-blocking concerns.
    ConditionallyConfident,
    /// Blocking failures — not confident, cutover blocked.
    NotConfident,
}

/// A single gate condition check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCondition {
    /// Condition identifier.
    pub condition_id: String,
    /// Description of what's being checked.
    pub description: String,
    /// Whether this condition passed.
    pub passed: bool,
    /// What was measured.
    pub measured: String,
    /// Whether failure blocks cutover.
    pub blocking: bool,
}

impl ConfidenceVerdict {
    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== Confidence Verdict: {:?} ===", self.decision));
        lines.push(format!(
            "Cells: {}/{} passed ({:.1}%)",
            self.cells_passed,
            self.cells_total,
            if self.cells_total > 0 {
                self.cells_passed as f64 / self.cells_total as f64 * 100.0
            } else {
                0.0
            }
        ));
        lines.push(format!("Duration: {}ms", self.soak_duration_ms));
        lines.push(String::new());
        for check in &self.checks {
            let icon = if check.passed { "[PASS]" } else { "[FAIL]" };
            let blocking = if check.blocking { " (blocking)" } else { "" };
            lines.push(format!(
                "{} {} — {}{} [{}]",
                icon, check.condition_id, check.description, blocking, check.measured
            ));
        }
        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_cell(id: &str, cat: JourneyCategory, blocking: bool) -> CellResult {
        CellResult {
            cell_id: id.into(),
            category: cat,
            workload: WorkloadProfile::Steady,
            injection: FailureInjectionProfile::None,
            passed: true,
            blocking,
            duration_ms: 1000,
            failure_reason: None,
            error_rate: 0.001,
            p95_latency_ms: 50.0,
            seed: Some(42),
            telemetry: CellTelemetry {
                ops_attempted: 100,
                ops_succeeded: 99,
                ops_failed: 1,
                tasks_spawned: 10,
                tasks_completed: 10,
                tasks_cancelled: 0,
                faults_injected: 0,
                recoveries: 0,
                deadlock_detected_count: 0,
            },
        }
    }

    fn failing_cell(id: &str, cat: JourneyCategory, blocking: bool) -> CellResult {
        CellResult {
            cell_id: id.into(),
            category: cat,
            workload: WorkloadProfile::Burst,
            injection: FailureInjectionProfile::Heavy,
            passed: false,
            blocking,
            duration_ms: 5000,
            failure_reason: Some("timeout exceeded".into()),
            error_rate: 0.15,
            p95_latency_ms: 3000.0,
            seed: Some(43),
            telemetry: CellTelemetry {
                ops_attempted: 100,
                ops_succeeded: 85,
                ops_failed: 15,
                tasks_spawned: 10,
                tasks_completed: 8,
                tasks_cancelled: 2,
                faults_injected: 20,
                recoveries: 5,
                deadlock_detected_count: 0,
            },
        }
    }

    fn sample_results(cells: Vec<CellResult>) -> SoakExecutionResult {
        let mut result = SoakExecutionResult::new(0);
        for cell in cells {
            result.record_cell(cell);
        }

        let agg = AggregatedSoakTelemetry::from_cells(&result.cell_results)
            .expect("sample telemetry does not overflow");
        for inv in SoakExecutionResult::standard_invariants(&agg) {
            result.record_invariant(inv);
        }
        result.complete(10000).expect("valid completion time");
        result
    }

    #[test]
    fn test_confident_verdict() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::RobotOrchestration, true),
            passing_cell("c3", JourneyCategory::Search, false),
        ]);

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::Confident);
        assert_eq!(verdict.cells_passed, 3);
        assert_eq!(verdict.cells_failed, 0);
    }

    #[test]
    fn test_confidence_gate_rejects_missing_forged_or_non_finite_evidence() {
        let mut missing = SoakExecutionResult::new(1);
        missing.record_cell(passing_cell("missing", JourneyCategory::Watch, true));
        missing.complete(1_001).expect("valid completion time");
        assert_eq!(
            ConfidenceGate::standard().evaluate(&missing).decision,
            ConfidenceDecision::NotConfident
        );

        let mut forged = sample_results(vec![passing_cell("forged", JourneyCategory::Watch, true)]);
        forged.invariant_checks[0].evidence.push_str(" (rewritten)");
        assert_eq!(
            ConfidenceGate::standard().evaluate(&forged).decision,
            ConfidenceDecision::NotConfident
        );

        let mut invalid_metric = sample_results(vec![passing_cell(
            "invalid-metric",
            JourneyCategory::Watch,
            true,
        )]);
        invalid_metric.cell_results[0].p95_latency_ms = f64::NAN;
        let verdict = ConfidenceGate::standard().evaluate(&invalid_metric);
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
        assert_eq!(
            ConfidenceGate::standard()
                .to_evidence(&invalid_metric, "invalid-metric")
                .p95_latency_ms
                .to_bits(),
            f64::MAX.to_bits()
        );

        let invalid_gate = ConfidenceGate {
            min_pass_rate: f64::NAN,
            ..ConfidenceGate::standard()
        };
        assert_eq!(
            invalid_gate
                .evaluate(&sample_results(vec![passing_cell(
                    "valid-cell",
                    JourneyCategory::Watch,
                    true,
                )]))
                .decision,
            ConfidenceDecision::NotConfident
        );
    }

    #[test]
    fn test_not_confident_blocking_failure() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            failing_cell("c2", JourneyCategory::RobotOrchestration, true), // blocking fail
        ]);

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_conditionally_confident_non_blocking_failure() {
        // Create results with high error rate on non-blocking cell.
        let mut custom = SoakExecutionResult::new(0);
        for c in [
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::RobotOrchestration, true),
        ] {
            custom.record_cell(c);
        }
        // Add a cell with high error rate.
        let mut high_err = passing_cell("c3", JourneyCategory::Search, false);
        high_err.error_rate = 0.20; // 20% error rate
        custom.record_cell(high_err);
        let telemetry = AggregatedSoakTelemetry::from_cells(&custom.cell_results)
            .expect("finite sample telemetry");
        for invariant in SoakExecutionResult::standard_invariants(&telemetry) {
            custom.record_invariant(invariant);
        }
        custom.complete(10000).expect("valid completion time");

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&custom);
        // Error rate gate is non-blocking, so should be ConditionallyConfident.
        assert_eq!(verdict.decision, ConfidenceDecision::ConditionallyConfident);
    }

    #[test]
    fn test_pass_rate_below_threshold() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, false),
            failing_cell("c2", JourneyCategory::Search, false),
            failing_cell("c3", JourneyCategory::RecordingReplay, false),
        ]);

        let gate = ConfidenceGate::standard(); // min 95% pass rate
        let verdict = gate.evaluate(&results);
        // 1/3 = 33% pass rate < 95%
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_standard_invariants_pass() {
        let telemetry = AggregatedSoakTelemetry {
            ops_attempted: 100,
            ops_succeeded: 95,
            ops_failed: 5,
            tasks_spawned: 20,
            tasks_completed: 18,
            tasks_cancelled: 2,
            faults_injected: 10,
            recoveries: 5,
            deadlock_detected_count: 0,
            cells_with_task_accounting_mismatch: 0,
            cells_with_operation_accounting_mismatch: 0,
            max_p95_latency_ms: 100.0,
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        assert_eq!(invariants.len(), 5);
        // All should pass.
        for inv in &invariants {
            assert!(
                inv.passed,
                "Invariant {} failed: {}",
                inv.invariant_id, inv.evidence
            );
        }
    }

    #[test]
    fn test_task_leak_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            tasks_spawned: 20,
            tasks_completed: 15,
            tasks_cancelled: 3, // 2 leaked
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let task_leak = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-01")
            .unwrap();
        assert!(!task_leak.passed);
        assert!(task_leak.mandatory);
    }

    #[test]
    fn test_deadlock_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            deadlock_detected_count: 1,
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let deadlock = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-02")
            .unwrap();
        assert!(!deadlock.passed);
    }

    #[test]
    fn test_message_loss_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            ops_attempted: 100,
            ops_succeeded: 90,
            ops_failed: 5, // 5 lost
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let msg_loss = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-03")
            .unwrap();
        assert!(!msg_loss.passed);
    }

    #[test]
    fn test_latency_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            max_p95_latency_ms: 10000.0,
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let latency = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-04")
            .unwrap();
        assert!(!latency.passed);
    }

    #[test]
    fn test_soak_matrix_standard() {
        let matrix = SoakMatrix::standard();
        assert_eq!(matrix.scenarios.len(), 8); // 8 journey categories
        assert_eq!(matrix.workload_profiles.len(), 4);
        assert_eq!(matrix.injection_profiles.len(), 4);
        assert_eq!(
            matrix.cell_count().expect("valid standard matrix"),
            8 * 4 * 4
        ); // 128
    }

    #[test]
    fn test_soak_matrix_ci_minimal() {
        let matrix = SoakMatrix::ci_minimal();
        // Only critical categories.
        assert_eq!(matrix.scenarios.len(), 4);
        assert_eq!(matrix.workload_profiles.len(), 2);
        assert_eq!(matrix.injection_profiles.len(), 2);
        assert_eq!(matrix.cell_count().expect("valid CI matrix"), 4 * 2 * 2); // 16
    }

    #[test]
    fn test_execution_plan_generation() {
        let matrix = SoakMatrix::ci_minimal();
        let plan = matrix.to_plan().expect("valid CI matrix");
        assert_eq!(plan.total_cells(), 16);
        assert!(!plan.blocking_cells().is_empty());
    }

    #[test]
    fn test_by_category_grouping() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::Watch, true),
            passing_cell("c3", JourneyCategory::Search, false),
        ]);

        let by_cat = results.by_category();
        assert_eq!(by_cat.get(&JourneyCategory::Watch).unwrap().len(), 2);
        assert_eq!(by_cat.get(&JourneyCategory::Search).unwrap().len(), 1);
    }

    #[test]
    fn test_confidence_to_evidence() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::Search, false),
        ]);

        let gate = ConfidenceGate::standard();
        let evidence = gate.to_evidence(&results, "soak-period-1");

        assert_eq!(evidence.period_id, "soak-period-1");
        assert!(evidence.slo_conforming);
        assert_eq!(evidence.incident_count, 0);
        assert!(!evidence.rollback_triggered);
    }

    #[test]
    fn test_verdict_render_summary() {
        let results = sample_results(vec![passing_cell("c1", JourneyCategory::Watch, true)]);
        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        let summary = verdict.render_summary();
        assert!(summary.contains("Confident"));
        assert!(summary.contains("CONF-01"));
    }

    #[test]
    fn test_strict_gate_rejects_any_failure() {
        let mut results = SoakExecutionResult::new(0);
        results.record_cell(passing_cell("c1", JourneyCategory::Watch, true));
        results.record_cell(failing_cell("c2", JourneyCategory::Search, false));
        results.complete(10000).expect("valid completion time");

        let gate = ConfidenceGate::strict(); // 100% pass rate required
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_aggregated_telemetry() {
        let cells = vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            failing_cell("c2", JourneyCategory::Search, false),
        ];

        let agg = AggregatedSoakTelemetry::from_cells(&cells)
            .expect("sample telemetry does not overflow");
        assert_eq!(agg.ops_attempted, 200); // 100 + 100
        assert_eq!(agg.ops_succeeded, 184); // 99 + 85
        assert_eq!(agg.tasks_spawned, 20); // 10 + 10
        assert_eq!(agg.faults_injected, 20); // 0 + 20
        assert!((agg.max_p95_latency_ms - 3000.0).abs() < 0.1);
    }

    #[test]
    fn test_aggregated_telemetry_fails_closed_on_counter_overflow() {
        let mut first = passing_cell("c1", JourneyCategory::Watch, true);
        first.telemetry.ops_attempted = u64::MAX;
        let mut second = passing_cell("c2", JourneyCategory::Watch, true);
        second.telemetry.ops_attempted = 1;
        let error = AggregatedSoakTelemetry::from_cells(&[first, second])
            .expect_err("counter overflow must fail");
        assert_eq!(
            error,
            SoakTelemetryAggregationError::CounterOverflow {
                field: "ops_attempted"
            }
        );
    }

    #[test]
    fn test_aggregated_telemetry_preserves_deadlocks_and_rejects_invalid_latency() {
        let mut deadlocked = passing_cell("c1", JourneyCategory::Watch, true);
        deadlocked.telemetry.deadlock_detected_count = 2;
        let aggregated = AggregatedSoakTelemetry::from_cells(&[deadlocked])
            .expect("finite cell telemetry aggregates");
        assert_eq!(aggregated.deadlock_detected_count, 2);
        assert!(
            !SoakExecutionResult::standard_invariants(&aggregated)
                .iter()
                .find(|invariant| invariant.invariant_id == "SOAK-INV-02")
                .expect("deadlock invariant")
                .passed
        );

        let mut invalid = passing_cell("bad-latency", JourneyCategory::Watch, true);
        invalid.p95_latency_ms = f64::NAN;
        assert_eq!(
            AggregatedSoakTelemetry::from_cells(&[invalid])
                .expect_err("non-finite latency must fail"),
            SoakTelemetryAggregationError::InvalidP95Latency { cell_index: 0 }
        );
    }

    #[test]
    fn test_aggregated_telemetry_does_not_let_cells_cancel_accounting_defects() {
        let mut surplus = passing_cell("surplus", JourneyCategory::Watch, true);
        surplus.telemetry.tasks_spawned = 10;
        surplus.telemetry.tasks_completed = 11;
        surplus.telemetry.ops_attempted = 10;
        surplus.telemetry.ops_succeeded = 11;
        surplus.telemetry.ops_failed = 0;
        let mut deficit = passing_cell("deficit", JourneyCategory::Watch, true);
        deficit.telemetry.tasks_spawned = 10;
        deficit.telemetry.tasks_completed = 9;
        deficit.telemetry.ops_attempted = 10;
        deficit.telemetry.ops_succeeded = 9;
        deficit.telemetry.ops_failed = 0;

        let aggregated = AggregatedSoakTelemetry::from_cells(&[surplus, deficit])
            .expect("bounded counters aggregate");
        assert_eq!(aggregated.tasks_spawned, aggregated.tasks_completed);
        assert_eq!(aggregated.ops_attempted, aggregated.ops_succeeded);
        assert_eq!(aggregated.cells_with_task_accounting_mismatch, 2);
        assert_eq!(aggregated.cells_with_operation_accounting_mismatch, 2);
        let invariants = SoakExecutionResult::standard_invariants(&aggregated);
        assert!(
            !invariants
                .iter()
                .find(|invariant| invariant.invariant_id == "SOAK-INV-01")
                .expect("task invariant")
                .passed
        );
        assert!(
            !invariants
                .iter()
                .find(|invariant| invariant.invariant_id == "SOAK-INV-03")
                .expect("operation invariant")
                .passed
        );
    }

    #[test]
    fn test_standard_invariants_fail_closed_on_counter_overflow() {
        let telemetry = AggregatedSoakTelemetry {
            ops_attempted: 0,
            ops_succeeded: u64::MAX,
            ops_failed: 1,
            tasks_spawned: 0,
            tasks_completed: u64::MAX,
            tasks_cancelled: 1,
            ..Default::default()
        };
        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        assert!(
            !invariants
                .iter()
                .find(|invariant| invariant.invariant_id == "SOAK-INV-01")
                .expect("task invariant")
                .passed
        );
        assert!(
            !invariants
                .iter()
                .find(|invariant| invariant.invariant_id == "SOAK-INV-03")
                .expect("message invariant")
                .passed
        );
    }

    #[test]
    fn test_journey_category_properties() {
        assert!(JourneyCategory::Watch.is_critical());
        assert!(JourneyCategory::RobotOrchestration.is_critical());
        assert!(JourneyCategory::SessionPersistence.is_critical());
        assert!(JourneyCategory::RestartCycle.is_critical());
        assert!(!JourneyCategory::Search.is_critical());
        assert!(!JourneyCategory::MixedBurst.is_critical());
        assert_eq!(JourneyCategory::ALL.len(), 8);
    }

    #[test]
    fn test_empty_results_not_confident() {
        let results = SoakExecutionResult::new(0);
        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        // 0% pass rate < 95% threshold.
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_blocking_scenario_count() {
        let matrix = SoakMatrix::standard();
        // 4 critical categories.
        assert_eq!(matrix.blocking_scenario_count(), 4);
    }

    #[test]
    fn test_serde_roundtrip() {
        let results = sample_results(vec![passing_cell("c1", JourneyCategory::Watch, true)]);
        let json = serde_json::to_string(&results).expect("serialize");
        let restored: SoakExecutionResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.cell_results.len(), 1);
        assert_eq!(restored.invariant_checks.len(), 5);
    }

    #[test]
    fn test_custom_matrix() {
        let scenarios = vec![UserJourneyScenario {
            scenario_id: "custom-1".into(),
            category: JourneyCategory::Watch,
            description: "Custom".into(),
            expected_duration_ms: 5000,
            blocking: true,
            seed: Some(1),
        }];

        let matrix = SoakMatrix::custom(
            scenarios,
            vec![WorkloadProfile::Steady],
            vec![FailureInjectionProfile::None],
        );
        assert_eq!(matrix.cell_count().expect("valid custom matrix"), 1);
        let plan = matrix.to_plan().expect("valid custom matrix");
        assert_eq!(plan.total_cells(), 1);
    }

    #[test]
    fn test_mandatory_invariant_failure_blocks() {
        let mut results = SoakExecutionResult::new(0);
        results.record_cell(passing_cell("c1", JourneyCategory::Watch, true));
        results.record_invariant(SoakInvariantCheck {
            invariant_id: "SOAK-INV-01".into(),
            description: "Task leaks".into(),
            passed: false,
            evidence: "2 tasks leaked".into(),
            mandatory: true,
        });
        results.complete(10000).expect("valid completion time");

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn long_haul_lifecycle_operations_are_idempotent() {
        let acquire = SoakLifecycleOperation {
            operation_id: "setup-acquire-workspace".into(),
            kind: SoakLifecycleKind::AcquireWorkspace,
            resource_id: "workspace".into(),
            parent_id: None,
        };
        let start = SoakLifecycleOperation {
            operation_id: "setup-start-actor".into(),
            kind: SoakLifecycleKind::StartActor,
            resource_id: "actor".into(),
            parent_id: Some("pane".into()),
        };
        let stop = SoakLifecycleOperation {
            operation_id: "teardown-stop-actor".into(),
            kind: SoakLifecycleKind::StopActor,
            resource_id: "actor".into(),
            parent_id: Some("pane".into()),
        };
        let release = SoakLifecycleOperation {
            operation_id: "teardown-release-workspace".into(),
            kind: SoakLifecycleKind::ReleaseWorkspace,
            resource_id: "workspace".into(),
            parent_id: None,
        };

        let mut state = SoakLogicalState::default();
        for operation in [&acquire, &start, &acquire, &start] {
            apply_soak_lifecycle(&mut state, operation);
        }
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.actors.len(), 1);

        for operation in [&stop, &release, &stop, &release] {
            apply_soak_lifecycle(&mut state, operation);
        }
        assert!(state.workspaces.is_empty());
        assert!(state.actors.is_empty());
    }
}
