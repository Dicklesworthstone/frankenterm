//! SnapshotEngine orchestrator for session persistence.
//!
//! Coordinates full mux state capture: layout topology, per-pane state,
//! scrollback references, and agent session metadata. Persists snapshots
//! to SQLite for crash-resilient session restoration.
//!
//! # Architecture
//!
//! ```text
//! SnapshotEngine
//!   ├── WeztermClient::list_panes()  → Vec<PaneInfo>
//!   ├── TopologySnapshot::from_panes()  → layout tree
//!   ├── PaneStateSnapshot::from_pane_info()  → per-pane state
//!   ├── Prepared persisted-state projection → stable `snpd2:` dedup digest
//!   ├── Row-local `snp2:` SHA-256 consistency witness
//!   └── SQLite  → mux_sessions + session_checkpoints + mux_pane_state
//! ```
//!
//! See `wa-29k1` bead for the full design.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_correlator::AgentCorrelator;
use crate::checkpoint_witness::{
    CHECKPOINT_ROLE_SNAPSHOT, CheckpointWitnessError, MAX_CHECKPOINT_METADATA_BYTES,
    MAX_CHECKPOINT_ROLE_BYTES, MAX_CHECKPOINT_SESSION_ID_BYTES, MAX_CHECKPOINT_STATE_HASH_BYTES,
    MAX_PERSISTED_CHECKPOINT_TEXT_BYTES, MAX_PERSISTED_PANE_TEXT_BYTES, PersistedPaneState,
    SNAPSHOT_WITNESS_PREFIX, canonical_json_string, checkpoint_witness,
    persisted_checkpoint_text_bytes, persisted_pane_text_bytes, snapshot_dedup_witness,
};
use crate::config::{SnapshotConfig, SnapshotSchedulingMode};
use crate::outcome::CancelKind;
use crate::patterns::{AgentType, Detection, Severity};
use crate::runtime_async::{LockAcquireError, RwLock, mpsc, watch};
use crate::session_pane_state::{
    AgentMetadata, CapturedEnv, PaneStateSnapshot, SAFE_ENV_VARS, ScrollbackRef,
};
use crate::session_topology::{MAX_TOPOLOGY_PANES, TopologySnapshot, TopologySnapshotError};
use crate::wezterm::PaneInfo;

// =============================================================================
// Telemetry
// =============================================================================

/// Add to a cumulative telemetry counter without allowing a long-running
/// process to wrap at `u64::MAX` and make the reported value move backwards.
fn saturating_telemetry_add(counter: &AtomicU64, delta: u64) {
    if delta == 0 {
        return;
    }
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == u64::MAX {
            return;
        }
        let next = current.saturating_add(delta);
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Operational telemetry counters for the snapshot engine.
///
/// Uses `AtomicU64` because `SnapshotEngine` methods take `&self`. Cumulative
/// counters saturate at `u64::MAX` rather than wrapping backwards.
pub struct SnapshotEngineTelemetry {
    /// Total capture() attempts.
    captures_attempted: AtomicU64,
    /// Captures that completed successfully.
    captures_succeeded: AtomicU64,
    /// Captures skipped due to dedup (NoChanges).
    dedup_skips: AtomicU64,
    /// Captures that failed with an error.
    capture_errors: AtomicU64,
    /// Number of cleanup attempts that entered `cleanup_with_cx`. Automatic
    /// cadence checks that skip a full scan do not increment this counter.
    cleanup_runs: AtomicU64,
    /// Checkpoints removed by cleanup.
    cleanup_removed: AtomicU64,
    /// Total emit_trigger() calls.
    triggers_emitted: AtomicU64,
    /// Triggers accepted (not dropped due to full queue).
    triggers_accepted: AtomicU64,
    /// Total panes captured across all successful snapshots.
    panes_captured: AtomicU64,
    /// Historical terminal/environment/agent pane-state JSON byte estimate
    /// across successful snapshots; not total SQLite or checkpoint bytes.
    bytes_persisted: AtomicU64,
    /// Exact admitted UTF-8 bytes across topology, metadata, and pane text
    /// columns for successful snapshots.
    persisted_text_bytes: AtomicU64,
    /// Pane projections that required deterministic field truncation or
    /// omission before persistence.
    pane_states_truncated: AtomicU64,
}

impl SnapshotEngineTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            captures_attempted: AtomicU64::new(0),
            captures_succeeded: AtomicU64::new(0),
            dedup_skips: AtomicU64::new(0),
            capture_errors: AtomicU64::new(0),
            cleanup_runs: AtomicU64::new(0),
            cleanup_removed: AtomicU64::new(0),
            triggers_emitted: AtomicU64::new(0),
            triggers_accepted: AtomicU64::new(0),
            panes_captured: AtomicU64::new(0),
            bytes_persisted: AtomicU64::new(0),
            persisted_text_bytes: AtomicU64::new(0),
            pane_states_truncated: AtomicU64::new(0),
        }
    }

    /// Snapshot the current counter values.
    ///
    /// Each counter is monotonic and saturating, but the relaxed loads are an
    /// intentionally approximate telemetry read rather than one cross-counter
    /// transaction. A concurrent update can therefore make relationships such
    /// as `triggers_accepted <= triggers_emitted` transiently inconsistent in a
    /// single sample; consumers must compare each counter independently.
    #[must_use]
    pub fn snapshot(&self) -> SnapshotEngineTelemetrySnapshot {
        SnapshotEngineTelemetrySnapshot {
            captures_attempted: self.captures_attempted.load(Ordering::Relaxed),
            captures_succeeded: self.captures_succeeded.load(Ordering::Relaxed),
            dedup_skips: self.dedup_skips.load(Ordering::Relaxed),
            capture_errors: self.capture_errors.load(Ordering::Relaxed),
            cleanup_runs: self.cleanup_runs.load(Ordering::Relaxed),
            cleanup_removed: self.cleanup_removed.load(Ordering::Relaxed),
            triggers_emitted: self.triggers_emitted.load(Ordering::Relaxed),
            triggers_accepted: self.triggers_accepted.load(Ordering::Relaxed),
            panes_captured: self.panes_captured.load(Ordering::Relaxed),
            bytes_persisted: self.bytes_persisted.load(Ordering::Relaxed),
            persisted_text_bytes: self.persisted_text_bytes.load(Ordering::Relaxed),
            pane_states_truncated: self.pane_states_truncated.load(Ordering::Relaxed),
        }
    }
}

impl Default for SnapshotEngineTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SnapshotEngineTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotEngineTelemetry")
            .field(
                "captures_attempted",
                &self.captures_attempted.load(Ordering::Relaxed),
            )
            .field(
                "captures_succeeded",
                &self.captures_succeeded.load(Ordering::Relaxed),
            )
            .field("dedup_skips", &self.dedup_skips.load(Ordering::Relaxed))
            .field(
                "capture_errors",
                &self.capture_errors.load(Ordering::Relaxed),
            )
            .field("cleanup_runs", &self.cleanup_runs.load(Ordering::Relaxed))
            .field(
                "cleanup_removed",
                &self.cleanup_removed.load(Ordering::Relaxed),
            )
            .field(
                "triggers_emitted",
                &self.triggers_emitted.load(Ordering::Relaxed),
            )
            .field(
                "triggers_accepted",
                &self.triggers_accepted.load(Ordering::Relaxed),
            )
            .field(
                "panes_captured",
                &self.panes_captured.load(Ordering::Relaxed),
            )
            .field(
                "bytes_persisted",
                &self.bytes_persisted.load(Ordering::Relaxed),
            )
            .field(
                "persisted_text_bytes",
                &self.persisted_text_bytes.load(Ordering::Relaxed),
            )
            .field(
                "pane_states_truncated",
                &self.pane_states_truncated.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// Serializable snapshot of snapshot engine telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEngineTelemetrySnapshot {
    /// Total capture() attempts.
    pub captures_attempted: u64,
    /// Captures that completed successfully.
    pub captures_succeeded: u64,
    /// Captures skipped due to dedup (NoChanges).
    pub dedup_skips: u64,
    /// Captures that failed with an error.
    pub capture_errors: u64,
    /// Number of cleanup attempts that entered `cleanup_with_cx`; cadence-only
    /// skips are excluded.
    pub cleanup_runs: u64,
    /// Checkpoints removed by cleanup.
    pub cleanup_removed: u64,
    /// Total emit_trigger() calls.
    pub triggers_emitted: u64,
    /// Triggers accepted (not dropped due to full queue).
    pub triggers_accepted: u64,
    /// Total panes captured across all successful snapshots.
    pub panes_captured: u64,
    /// Historical terminal/environment/agent pane-state JSON byte estimate
    /// across successful snapshots; not total SQLite or checkpoint bytes.
    pub bytes_persisted: u64,
    /// Exact admitted UTF-8 bytes across topology, metadata, and pane text
    /// columns for successful snapshots.
    pub persisted_text_bytes: u64,
    /// Pane projections that required deterministic field truncation or
    /// omission before persistence.
    pub pane_states_truncated: u64,
}

// =============================================================================
// Types
// =============================================================================

// Canonical value in TuningConfig::RuntimeTuning (unified — was duplicated in agent_correlator.rs).
const STATE_DETECTION_MAX_AGE: Duration =
    Duration::from_secs(crate::tuning_config::RuntimeTuning::DEFAULT_STATE_DETECTION_MAX_AGE_SECS);

/// What triggered the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotTrigger {
    /// Periodic timer-based capture.
    Periodic,
    /// Reduced-frequency periodic fallback capture in intelligent mode.
    PeriodicFallback,
    /// Manual user-initiated capture.
    Manual,
    /// Pre-restart capture (blocks until complete).
    Shutdown,
    /// Startup capture (initial state after watcher starts).
    Startup,
    /// Event-driven capture (e.g., agent session change).
    Event,
    /// Agent completed significant work.
    WorkCompleted,
    /// Hazard estimate crossed threshold.
    HazardThreshold,
    /// Agent state transition detected.
    StateTransition,
    /// Extended idle period before potential restart.
    IdleWindow,
    /// Memory pressure increased.
    MemoryPressure,
}

impl SnapshotTrigger {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Periodic | Self::PeriodicFallback => "periodic",
            Self::Manual
            | Self::Event
            | Self::WorkCompleted
            | Self::HazardThreshold
            | Self::StateTransition
            | Self::IdleWindow
            | Self::MemoryPressure => "event",
            Self::Shutdown => "shutdown",
            Self::Startup => "startup",
        }
    }
}

// =============================================================================
// Whole-mux recovery claim contract
// =============================================================================

/// Bead that owns the first version of the whole-mux recovery claim contract.
pub const SNAPSHOT_RECOVERY_CONTRACT_OWNER: &str =
    "ft-interactive-swarm-product-convergence-7xqz4.8.14.1.1";

/// Policy targets for periodic recovery publication.
///
/// These are requested objectives, never evidence that a runtime achieved
/// them. Replica, anchor, freshness, scrub, and RTO targets remain unset until
/// the corresponding runtime/configuration beads install an authority for
/// them. That distinction prevents a default value from becoming a product
/// claim merely because it was serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRecoveryPolicy {
    /// Requested interval between local whole-mux capture attempts.
    pub periodic_interval_secs: u64,
    /// Requested maximum age of the newest locally verified generation.
    pub target_local_rpo_secs: u64,
    /// Requested maximum age of the newest independently replicated generation.
    pub target_replica_rpo_secs: Option<u64>,
    /// Requested maximum age of a full anchor generation.
    pub max_full_anchor_age_secs: Option<u64>,
    /// Requested time to make one priority pane safely interactive.
    pub target_interactive_safe_rto_secs: Option<u64>,
    /// Requested time to finish all required whole-mux hydration.
    pub target_complete_rto_secs: Option<u64>,
    /// Requested maximum age of authoritative freshness-witness evidence.
    pub max_freshness_witness_age_secs: Option<u64>,
    /// Requested maximum age of a successful shallow scrub.
    pub max_shallow_scrub_age_secs: Option<u64>,
    /// Requested maximum age of a successful deep scrub.
    pub max_deep_scrub_age_secs: Option<u64>,
    /// Requested maximum age of a successful clean-host disaster drill.
    pub max_disaster_drill_age_secs: Option<u64>,
}

impl SnapshotRecoveryPolicy {
    /// Conservative local-only policy derived from the existing periodic
    /// snapshot interval. It makes no replica, RTO, freshness, scrub, or drill
    /// promise until those values are explicitly configured.
    #[must_use]
    pub const fn local_only(periodic_interval_secs: u64) -> Self {
        Self {
            periodic_interval_secs,
            target_local_rpo_secs: periodic_interval_secs,
            target_replica_rpo_secs: None,
            max_full_anchor_age_secs: None,
            target_interactive_safe_rto_secs: None,
            target_complete_rto_secs: None,
            max_freshness_witness_age_secs: None,
            max_shallow_scrub_age_secs: None,
            max_deep_scrub_age_secs: None,
            max_disaster_drill_age_secs: None,
        }
    }

    /// Validate ordering and nonzero constraints without promoting an unset
    /// objective into an achieved fact.
    pub fn validate(self) -> Result<Self, SnapshotRecoveryPolicyError> {
        if self.periodic_interval_secs == 0 || self.target_local_rpo_secs == 0 {
            return Err(SnapshotRecoveryPolicyError::ZeroLocalObjective);
        }
        if self.target_local_rpo_secs < self.periodic_interval_secs {
            return Err(SnapshotRecoveryPolicyError::LocalRpoBelowInterval);
        }
        if self
            .target_replica_rpo_secs
            .is_some_and(|replica| replica < self.target_local_rpo_secs)
        {
            return Err(SnapshotRecoveryPolicyError::ReplicaRpoBelowLocalRpo);
        }
        if self
            .target_complete_rto_secs
            .zip(self.target_interactive_safe_rto_secs)
            .is_some_and(|(complete, interactive)| complete < interactive)
        {
            return Err(SnapshotRecoveryPolicyError::CompleteRtoBelowInteractiveRto);
        }
        for value in [
            self.target_replica_rpo_secs,
            self.max_full_anchor_age_secs,
            self.target_interactive_safe_rto_secs,
            self.target_complete_rto_secs,
            self.max_freshness_witness_age_secs,
            self.max_shallow_scrub_age_secs,
            self.max_deep_scrub_age_secs,
            self.max_disaster_drill_age_secs,
        ] {
            if value == Some(0) {
                return Err(SnapshotRecoveryPolicyError::ZeroOptionalObjective);
            }
        }
        Ok(self)
    }
}

impl Default for SnapshotRecoveryPolicy {
    fn default() -> Self {
        Self::local_only(SnapshotConfig::default().interval_seconds)
    }
}

/// Invalid recovery-objective policy. Variants are deliberately content-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotRecoveryPolicyError {
    #[error("local recovery interval and RPO must be nonzero")]
    ZeroLocalObjective,
    #[error("local RPO cannot be lower than the periodic capture interval")]
    LocalRpoBelowInterval,
    #[error("replica RPO cannot be lower than local RPO")]
    ReplicaRpoBelowLocalRpo,
    #[error("complete RTO cannot be lower than interactive-safe RTO")]
    CompleteRtoBelowInteractiveRto,
    #[error("an explicitly configured recovery objective cannot be zero")]
    ZeroOptionalObjective,
}

/// Failure and disaster classes covered by the normative recovery matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryFailureClass {
    GracefulMuxRestart,
    MuxCrash,
    GuardianCrash,
    ClientCrash,
    FullHostPowerLoss,
    FilesystemWritebackLoss,
    TornOrCorruptArtifact,
    DiskFull,
    InodeFull,
    RemoteDomainOutage,
    BuildOrCodecChange,
    KeyLoss,
    OperatorSelectedRollback,
    LocalMediaLoss,
    CompleteHostLoss,
    LocalCredentialStoreLoss,
    ReplicaDomainLoss,
    CorrelatedSiteLoss,
    ValidButStaleReplica,
    OmittedLatestRoot,
    ForkOrSplitView,
    NoFreshnessWitness,
    NoBootstrapRoute,
    CleanHostRecovery,
    ShallowScrubOverdue,
    DeepScrubOverdue,
    ScrubFailure,
    CorruptRepairSource,
    ClientViewStateLoss,
    ClientViewStateConflict,
}

impl SnapshotRecoveryFailureClass {
    /// Stable exhaustive order used by contract and proof-manifest tests.
    pub const ALL: [Self; 30] = [
        Self::GracefulMuxRestart,
        Self::MuxCrash,
        Self::GuardianCrash,
        Self::ClientCrash,
        Self::FullHostPowerLoss,
        Self::FilesystemWritebackLoss,
        Self::TornOrCorruptArtifact,
        Self::DiskFull,
        Self::InodeFull,
        Self::RemoteDomainOutage,
        Self::BuildOrCodecChange,
        Self::KeyLoss,
        Self::OperatorSelectedRollback,
        Self::LocalMediaLoss,
        Self::CompleteHostLoss,
        Self::LocalCredentialStoreLoss,
        Self::ReplicaDomainLoss,
        Self::CorrelatedSiteLoss,
        Self::ValidButStaleReplica,
        Self::OmittedLatestRoot,
        Self::ForkOrSplitView,
        Self::NoFreshnessWitness,
        Self::NoBootstrapRoute,
        Self::CleanHostRecovery,
        Self::ShallowScrubOverdue,
        Self::DeepScrubOverdue,
        Self::ScrubFailure,
        Self::CorruptRepairSource,
        Self::ClientViewStateLoss,
        Self::ClientViewStateConflict,
    ];
}

/// Five capabilities that must never be conflated by a recovery surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryCapability {
    GuardianLiveProcessReattachment,
    ExactTerminalParserRenderReconstruction,
    TopologyLayoutRecreation,
    PolicyGatedProcessReplacement,
    ForensicContentExport,
}

impl SnapshotRecoveryCapability {
    /// Stable exhaustive order used by cross-product tests.
    pub const ALL: [Self; 5] = [
        Self::GuardianLiveProcessReattachment,
        Self::ExactTerminalParserRenderReconstruction,
        Self::TopologyLayoutRecreation,
        Self::PolicyGatedProcessReplacement,
        Self::ForensicContentExport,
    ];
}

/// Recovery phase whose name may be exposed to a user or API consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryReadiness {
    /// Verified bytes that have not acquired mutation authority.
    Candidate,
    /// One priority pane has singular writer/input authority and can be used.
    InteractiveSafe,
    /// Every required whole-mux object and semantic invariant is present.
    Complete,
}

/// Receipt-backed durability grade. `Unverified` is a first-class result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryDurabilityGrade {
    Unverified,
    LocalVerified,
    ReplicatedVerified,
    OffsiteVerified,
}

/// Independent verdict used for validity and authority checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryVerdict {
    Unknown,
    Verified,
    Rejected,
    Conflict,
}

/// Semantic content proven by the selected artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoverySemantics {
    ForensicPartial,
    ForensicComplete,
    TerminalStateComplete,
    WholeMuxComplete,
}

/// Whether RaptorQ or another repair operation contributed bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryRepairStatus {
    NotRepaired,
    RepairedUnverified,
    RepairedAndReverified,
}

/// Artifact family supplying the evidence profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryArtifactKind {
    LiveMuxForensicDump,
    CheckpointScrollbackExport,
    WholeMuxRecoveryImage,
}

/// Freshness is independent from validity and durability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryFreshness {
    Unknown,
    Verified,
    Stale,
    Conflict,
}

/// Scrub coverage is independent from current artifact validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryScrubCoverage {
    Unknown,
    Current,
    Overdue,
    Failed,
}

/// Disaster-drill currency is independent from a single successful restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryDrillCurrency {
    Unknown,
    Current,
    Overdue,
    Failed,
}

/// Per-client view-state disposition, separate from authoritative mux truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryClientStateDisposition {
    Unknown,
    PreservedAndVerified,
    SafelyReset,
    Lost,
    Conflict,
}

/// Complete evidence profile consumed by capability and readiness guards.
///
/// Keeping each verdict as a separate field is intentional. Constructors and
/// validators must not infer freshness from validity, durability from repair,
/// whole-mux semantics from a forensic export, or client-state preservation
/// from server recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRecoveryEvidence {
    pub artifact_kind: SnapshotRecoveryArtifactKind,
    pub artifact_validity: SnapshotRecoveryVerdict,
    pub repair_status: SnapshotRecoveryRepairStatus,
    pub semantics: SnapshotRecoverySemantics,
    pub compatibility: SnapshotRecoveryVerdict,
    pub topology_authority: SnapshotRecoveryVerdict,
    pub guardian_census: SnapshotRecoveryVerdict,
    pub lease_replay_input_authority: SnapshotRecoveryVerdict,
    pub process_replacement_approval: SnapshotRecoveryVerdict,
    pub durability: SnapshotRecoveryDurabilityGrade,
    pub freshness: SnapshotRecoveryFreshness,
    pub scrub_coverage: SnapshotRecoveryScrubCoverage,
    pub drill_currency: SnapshotRecoveryDrillCurrency,
    pub client_state: SnapshotRecoveryClientStateDisposition,
}

impl SnapshotRecoveryEvidence {
    /// Evidence profile for an independently verified live mux content dump.
    #[must_use]
    pub const fn verified_mux_forensic_dump(complete: bool) -> Self {
        Self::verified_forensic(
            SnapshotRecoveryArtifactKind::LiveMuxForensicDump,
            complete,
        )
    }

    /// Evidence profile for an independently verified checkpoint/scrollback export.
    #[must_use]
    pub const fn verified_checkpoint_scrollback_export(complete: bool) -> Self {
        Self::verified_forensic(
            SnapshotRecoveryArtifactKind::CheckpointScrollbackExport,
            complete,
        )
    }

    const fn verified_forensic(kind: SnapshotRecoveryArtifactKind, complete: bool) -> Self {
        Self {
            artifact_kind: kind,
            artifact_validity: SnapshotRecoveryVerdict::Verified,
            repair_status: SnapshotRecoveryRepairStatus::NotRepaired,
            semantics: if complete {
                SnapshotRecoverySemantics::ForensicComplete
            } else {
                SnapshotRecoverySemantics::ForensicPartial
            },
            compatibility: SnapshotRecoveryVerdict::Unknown,
            topology_authority: SnapshotRecoveryVerdict::Unknown,
            guardian_census: SnapshotRecoveryVerdict::Unknown,
            lease_replay_input_authority: SnapshotRecoveryVerdict::Unknown,
            process_replacement_approval: SnapshotRecoveryVerdict::Unknown,
            durability: SnapshotRecoveryDurabilityGrade::Unverified,
            freshness: SnapshotRecoveryFreshness::Unknown,
            scrub_coverage: SnapshotRecoveryScrubCoverage::Unknown,
            drill_currency: SnapshotRecoveryDrillCurrency::Unknown,
            client_state: SnapshotRecoveryClientStateDisposition::Unknown,
        }
    }

    /// Validate one bounded capability/readiness claim.
    pub fn validate_claim(
        self,
        capability: SnapshotRecoveryCapability,
        readiness: SnapshotRecoveryReadiness,
    ) -> Result<SnapshotRecoveryClaimReceipt, SnapshotRecoveryClaimError> {
        if self.artifact_validity != SnapshotRecoveryVerdict::Verified {
            return Err(SnapshotRecoveryClaimError::ArtifactNotVerified);
        }
        if self.repair_status == SnapshotRecoveryRepairStatus::RepairedUnverified {
            return Err(SnapshotRecoveryClaimError::RepairNotReverified);
        }

        if capability == SnapshotRecoveryCapability::ForensicContentExport {
            if self.artifact_kind == SnapshotRecoveryArtifactKind::WholeMuxRecoveryImage
                || !matches!(
                    self.semantics,
                    SnapshotRecoverySemantics::ForensicPartial
                        | SnapshotRecoverySemantics::ForensicComplete
                )
            {
                return Err(SnapshotRecoveryClaimError::ArtifactCapabilityMismatch);
            }
            if readiness != SnapshotRecoveryReadiness::Candidate {
                return Err(SnapshotRecoveryClaimError::ForensicPromotionForbidden);
            }
            return Ok(SnapshotRecoveryClaimReceipt {
                capability,
                readiness,
                mutation_permitted: false,
            });
        }

        if self.artifact_kind != SnapshotRecoveryArtifactKind::WholeMuxRecoveryImage {
            return Err(SnapshotRecoveryClaimError::ForensicPromotionForbidden);
        }
        if self.compatibility != SnapshotRecoveryVerdict::Verified {
            return Err(SnapshotRecoveryClaimError::CompatibilityNotVerified);
        }

        match capability {
            SnapshotRecoveryCapability::GuardianLiveProcessReattachment => {
                if self.guardian_census != SnapshotRecoveryVerdict::Verified {
                    return Err(SnapshotRecoveryClaimError::GuardianCensusNotVerified);
                }
                if self.lease_replay_input_authority != SnapshotRecoveryVerdict::Verified {
                    return Err(SnapshotRecoveryClaimError::MutationAuthorityNotVerified);
                }
            }
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction => {
                if !matches!(
                    self.semantics,
                    SnapshotRecoverySemantics::TerminalStateComplete
                        | SnapshotRecoverySemantics::WholeMuxComplete
                ) {
                    return Err(SnapshotRecoveryClaimError::TerminalSemanticsIncomplete);
                }
            }
            SnapshotRecoveryCapability::TopologyLayoutRecreation => {
                if self.semantics != SnapshotRecoverySemantics::WholeMuxComplete
                    || self.topology_authority != SnapshotRecoveryVerdict::Verified
                {
                    return Err(SnapshotRecoveryClaimError::TopologyAuthorityNotVerified);
                }
            }
            SnapshotRecoveryCapability::PolicyGatedProcessReplacement => {
                if self.semantics != SnapshotRecoverySemantics::WholeMuxComplete
                    || self.topology_authority != SnapshotRecoveryVerdict::Verified
                {
                    return Err(SnapshotRecoveryClaimError::TopologyAuthorityNotVerified);
                }
                if self.process_replacement_approval != SnapshotRecoveryVerdict::Verified {
                    return Err(SnapshotRecoveryClaimError::ProcessReplacementNotApproved);
                }
            }
            SnapshotRecoveryCapability::ForensicContentExport => unreachable!(),
        }

        if readiness != SnapshotRecoveryReadiness::Candidate {
            if self.durability < SnapshotRecoveryDurabilityGrade::LocalVerified {
                return Err(SnapshotRecoveryClaimError::DurabilityNotVerified);
            }
            if self.freshness != SnapshotRecoveryFreshness::Verified {
                return Err(SnapshotRecoveryClaimError::FreshnessNotVerified);
            }
        }
        if readiness == SnapshotRecoveryReadiness::Complete
            && (self.semantics != SnapshotRecoverySemantics::WholeMuxComplete
                || self.topology_authority != SnapshotRecoveryVerdict::Verified)
        {
            return Err(SnapshotRecoveryClaimError::WholeMuxSemanticsIncomplete);
        }

        Ok(SnapshotRecoveryClaimReceipt {
            capability,
            readiness,
            mutation_permitted: readiness != SnapshotRecoveryReadiness::Candidate,
        })
    }

    /// Stronger release-language gate. Scrub and drill currency stay separate
    /// facts, but both must be current before a release may claim this complete
    /// recovery capability.
    pub fn validate_release_claim(
        self,
        capability: SnapshotRecoveryCapability,
    ) -> Result<SnapshotRecoveryClaimReceipt, SnapshotRecoveryClaimError> {
        let receipt = self.validate_claim(capability, SnapshotRecoveryReadiness::Complete)?;
        if self.scrub_coverage != SnapshotRecoveryScrubCoverage::Current {
            return Err(SnapshotRecoveryClaimError::ScrubCoverageNotCurrent);
        }
        if self.drill_currency != SnapshotRecoveryDrillCurrency::Current {
            return Err(SnapshotRecoveryClaimError::DisasterDrillNotCurrent);
        }
        Ok(receipt)
    }
}

/// Nonconstructive claim receipt: only the guard can establish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SnapshotRecoveryClaimReceipt {
    capability: SnapshotRecoveryCapability,
    readiness: SnapshotRecoveryReadiness,
    mutation_permitted: bool,
}

impl SnapshotRecoveryClaimReceipt {
    #[must_use]
    pub const fn capability(self) -> SnapshotRecoveryCapability {
        self.capability
    }

    #[must_use]
    pub const fn readiness(self) -> SnapshotRecoveryReadiness {
        self.readiness
    }

    #[must_use]
    pub const fn mutation_permitted(self) -> bool {
        self.mutation_permitted
    }
}

/// Finite, content-free reason why a recovery claim was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotRecoveryClaimError {
    #[error("artifact validity is not verified")]
    ArtifactNotVerified,
    #[error("repaired bytes have not been independently reverified")]
    RepairNotReverified,
    #[error("artifact semantics do not match the requested capability")]
    ArtifactCapabilityMismatch,
    #[error("a forensic artifact cannot be promoted to executable recovery")]
    ForensicPromotionForbidden,
    #[error("build and codec compatibility are not verified")]
    CompatibilityNotVerified,
    #[error("guardian census authority is not verified")]
    GuardianCensusNotVerified,
    #[error("lease, replay, and input authority are not verified")]
    MutationAuthorityNotVerified,
    #[error("terminal parser and render semantics are incomplete")]
    TerminalSemanticsIncomplete,
    #[error("topology authority is not verified")]
    TopologyAuthorityNotVerified,
    #[error("process replacement is not policy-approved")]
    ProcessReplacementNotApproved,
    #[error("durability grade is not locally verified")]
    DurabilityNotVerified,
    #[error("freshness is not verified or is in conflict")]
    FreshnessNotVerified,
    #[error("whole-mux semantics are incomplete")]
    WholeMuxSemanticsIncomplete,
    #[error("scrub coverage is not current")]
    ScrubCoverageNotCurrent,
    #[error("disaster drill is not current")]
    DisasterDrillNotCurrent,
}

/// Whether a failure/capability cell can ever graduate after exact checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRecoveryCapabilityAvailability {
    CandidateAfterExactGuards,
    RequiresIndependentDurability,
    RequiresExternalProcessDurability,
    Forbidden,
}

/// One row in the normative failure matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SnapshotRecoveryFailureContract {
    pub failure: SnapshotRecoveryFailureClass,
    pub recoverable_point: &'static str,
    pub rpo_scope: &'static str,
    pub rto_scope: &'static str,
    pub automation: &'static str,
    pub mutation: &'static str,
    pub operator_acknowledgement: bool,
    pub required_evidence: &'static [&'static str],
    pub terminal_outcome: &'static str,
    pub nonclaim: &'static str,
}

/// Cross-product cell joining one failure row to one distinct capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SnapshotRecoveryContractCell {
    pub failure: SnapshotRecoveryFailureClass,
    pub capability: SnapshotRecoveryCapability,
    pub availability: SnapshotRecoveryCapabilityAvailability,
    pub nonclaim: &'static str,
}

const EVIDENCE_LOCAL_ROOT: &[&str] = &[
    "cryptographic_validity",
    "semantic_completeness",
    "platform_durability_receipt",
];
const EVIDENCE_GUARDIAN: &[&str] = &[
    "authenticated_guardian_census",
    "lease_generation",
    "replay_watermark",
    "input_effect_disposition",
];
const EVIDENCE_REPLICA: &[&str] = &[
    "independent_replica_receipt",
    "recovery_key_wrapper",
    "freshness_witness",
];
const EVIDENCE_FRESHNESS: &[&str] = &[
    "independent_freshness_witness",
    "root_chain_continuity",
    "operator_selected_root",
];
const EVIDENCE_SCRUB: &[&str] = &[
    "scrub_coverage_receipt",
    "source_symbol_identity",
    "post_repair_reverification",
];
const EVIDENCE_CLIENT: &[&str] = &[
    "exact_mux_root_identity",
    "domain_and_pane_identity",
    "client_state_disposition",
];

const fn failure_contract(
    failure: SnapshotRecoveryFailureClass,
    recoverable_point: &'static str,
    rpo_scope: &'static str,
    rto_scope: &'static str,
    automation: &'static str,
    mutation: &'static str,
    operator_acknowledgement: bool,
    required_evidence: &'static [&'static str],
    terminal_outcome: &'static str,
    nonclaim: &'static str,
) -> SnapshotRecoveryFailureContract {
    SnapshotRecoveryFailureContract {
        failure,
        recoverable_point,
        rpo_scope,
        rto_scope,
        automation,
        mutation,
        operator_acknowledgement,
        required_evidence,
        terminal_outcome,
        nonclaim,
    }
}

/// Return the exhaustive normative row for one failure class.
#[must_use]
pub const fn snapshot_recovery_failure_contract(
    failure: SnapshotRecoveryFailureClass,
) -> SnapshotRecoveryFailureContract {
    use SnapshotRecoveryFailureClass as F;
    match failure {
        F::GracefulMuxRestart => failure_contract(failure, "newest verified local root plus guardian replay suffix", "local publication RPO", "interactive-safe then complete RTO", "automatic after exact preflight", "activate only after singular writer authority", false, EVIDENCE_GUARDIAN, "fallback to predecessor or unavailable", "a graceful restart is not proof of crash or power-loss recovery"),
        F::MuxCrash => failure_contract(failure, "newest verified root plus guardian durable replay prefix", "local publication RPO", "interactive-safe then complete RTO", "automatic read-only plan; guarded activation", "no mutation before lease and replay reconciliation", false, EVIDENCE_GUARDIAN, "quarantine ambiguous panes", "SIGKILL proves only mux-process loss, not host power loss"),
        F::GuardianCrash => failure_contract(failure, "newest verified serialized terminal and topology state", "local publication RPO", "complete reconstruction RTO", "automatic verification only", "replacement requires policy approval", true, EVIDENCE_LOCAL_ROOT, "live reattachment unavailable", "guardian loss does not preserve guardian-owned PTYs or child execution"),
        F::ClientCrash => failure_contract(failure, "current live mux truth", "not applicable to mux state", "client reconnect RTO", "automatic after mux identity validation", "client view state may apply only after exact pane validation", false, EVIDENCE_CLIENT, "reset client state and reconnect", "client state is not mux state or pending input authority"),
        F::FullHostPowerLoss => failure_contract(failure, "newest power-loss-verified serialized generation", "local or replica RPO by surviving receipt", "complete reconstruction RTO", "automatic verification; policy-gated activation", "never claim live process reattachment", true, EVIDENCE_LOCAL_ROOT, "fallback or unavailable", "powered-off processes do not execute and process memory is not serialized"),
        F::FilesystemWritebackLoss => failure_contract(failure, "newest generation whose file, root, parent, and device flush contract verifies", "local publication RPO", "complete reconstruction RTO", "automatic predecessor fallback", "mutate only after selected root verifies", false, EVIDENCE_LOCAL_ROOT, "quarantine torn suffix", "rename or fsync alone is not universal power-loss proof"),
        F::TornOrCorruptArtifact => failure_contract(failure, "newest independently verified or repairable generation", "local or replica RPO", "repair plus complete RTO", "bounded repair then reverify", "repaired bytes remain candidates", false, EVIDENCE_SCRUB, "quarantine on insufficient rank or authentication failure", "RaptorQ rank does not imply integrity, authenticity, or authority"),
        F::DiskFull => failure_contract(failure, "last fully published predecessor", "last successful local publication", "operator remediation plus recovery RTO", "skip overlapping publication and preserve predecessors", "no retention mutation without capacity authority", true, EVIDENCE_LOCAL_ROOT, "degraded with explicit stale RPO", "failed publication never refreshes RPO"),
        F::InodeFull => failure_contract(failure, "last fully published predecessor", "last successful local publication", "operator remediation plus recovery RTO", "skip publication and preserve predecessors", "no partial generation publication", true, EVIDENCE_LOCAL_ROOT, "degraded with explicit stale RPO", "free bytes do not prove inode capacity"),
        F::RemoteDomainOutage => failure_contract(failure, "verified local root and already durable remote receipts", "separate local and replica RPO", "local recovery RTO", "continue bounded local capture; defer replication", "no remote authority invention", false, EVIDENCE_REPLICA, "replication degraded", "local success is not replicated durability"),
        F::BuildOrCodecChange => failure_contract(failure, "newest migration-compatible verified root", "selected root RPO", "migration plus recovery RTO", "automatic read-only compatibility plan", "activation requires exact migration receipt", true, EVIDENCE_LOCAL_ROOT, "quarantine incompatible objects", "parse success is not semantic compatibility"),
        F::KeyLoss => failure_contract(failure, "generation decryptable through an independent wrapper", "surviving-wrapper RPO", "key recovery plus complete RTO", "automatic discovery only", "no plaintext guessing or unauthenticated import", true, EVIDENCE_REPLICA, "encrypted state unavailable", "repair symbols cannot replace a lost decryption key"),
        F::OperatorSelectedRollback => failure_contract(failure, "exact acknowledged older verified root", "operator-selected historical point", "complete reconstruction RTO", "never automatic", "activate only the pinned root after freshness warning", true, EVIDENCE_FRESHNESS, "remain on current root or unavailable", "an acknowledged rollback is not latest-state recovery"),
        F::LocalMediaLoss => failure_contract(failure, "newest independently replicated verified generation", "replica RPO", "replica download plus recovery RTO", "automatic read-only discovery", "activation requires independent key and freshness", true, EVIDENCE_REPLICA, "unavailable without replica", "same-device repair symbols do not survive device loss"),
        F::CompleteHostLoss => failure_contract(failure, "newest independent-domain generation and key wrapper", "replica or offsite RPO", "clean-host RTO", "bootstrap discovery then read-only plan", "rotate lost-host credentials before publication", true, EVIDENCE_REPLICA, "unavailable without bootstrap, replica, and wrapper", "host loss cannot preserve local processes, media, credentials, or cache"),
        F::LocalCredentialStoreLoss => failure_contract(failure, "generation unlocked by an independent recovery wrapper", "surviving-wrapper RPO", "credential recovery plus complete RTO", "approved wrapper acquisition", "rotate lost credentials before write authority", true, EVIDENCE_REPLICA, "encrypted state unavailable", "artifact possession is not decryption or mutation authority"),
        F::ReplicaDomainLoss => failure_contract(failure, "newest verified surviving local or independent replica root", "surviving-domain RPO", "re-replication RTO", "continue local capture and rebuild redundancy", "never delete sole surviving root", false, EVIDENCE_REPLICA, "durability grade downgraded", "one replica receipt is not offsite durability"),
        F::CorrelatedSiteLoss => failure_contract(failure, "newest offsite-verified generation and wrapper", "offsite RPO", "clean-site RTO", "approved offsite bootstrap", "new credentials and authority required", true, EVIDENCE_REPLICA, "unavailable without independent site", "local plus same-site replicas are one failure domain"),
        F::ValidButStaleReplica => failure_contract(failure, "newest root confirmed by independent witness", "witness-confirmed RPO", "freshness reconciliation RTO", "verification and comparison only", "no automatic activation", true, EVIDENCE_FRESHNESS, "quarantine stale root", "cryptographic validity does not prove freshness"),
        F::OmittedLatestRoot => failure_contract(failure, "newest independently witnessed root", "witness-confirmed RPO", "freshness reconciliation RTO", "query independent witnesses", "no automatic fallback presented as latest", true, EVIDENCE_FRESHNESS, "unknown freshness", "a store listing is not proof that no newer root exists"),
        F::ForkOrSplitView => failure_contract(failure, "operator-selected branch after witness reconciliation", "branch-specific RPO", "conflict-resolution RTO", "read-only branch comparison", "automatic recovery forbidden", true, EVIDENCE_FRESHNESS, "quarantine all conflicting heads", "individually valid branches do not establish one authority"),
        F::NoFreshnessWitness => failure_contract(failure, "verified root with unknown freshness", "unknown freshness; artifact age only", "operator-decision RTO", "verification only", "automatic activation forbidden", true, EVIDENCE_FRESHNESS, "candidate or unavailable", "validity and timestamp do not prove latestness"),
        F::NoBootstrapRoute => failure_contract(failure, "locally discoverable state only", "surviving local RPO", "operator provisioning RTO", "local verification only", "no remote authority discovery by guessing", true, EVIDENCE_REPLICA, "clean-host recovery unavailable", "a replica cannot help a fresh host that cannot discover or authenticate it"),
        F::CleanHostRecovery => failure_contract(failure, "newest independently discovered verified root and wrapper", "replica or offsite RPO", "interactive-safe then complete clean-host RTO", "approved bootstrap then read-only plan", "rotate credentials before durable publication", true, EVIDENCE_REPLICA, "unavailable on any missing authority", "copied local cache is not a clean-host drill"),
        F::ShallowScrubOverdue => failure_contract(failure, "last verified root with overdue coverage", "last publication RPO plus coverage age", "scrub catch-up RTO", "bounded priority scrub", "do not upgrade durability grade", false, EVIDENCE_SCRUB, "degraded coverage", "recent publication does not imply recent verification"),
        F::DeepScrubOverdue => failure_contract(failure, "last verified root with overdue deep coverage", "last publication RPO plus coverage age", "deep-scrub catch-up RTO", "bounded scheduled deep scrub", "do not reclaim unverified dependencies", false, EVIDENCE_SCRUB, "degraded coverage", "shallow metadata checks do not prove object decodability"),
        F::ScrubFailure => failure_contract(failure, "last independently verified unaffected root", "unaffected-root RPO", "repair or re-replication RTO", "quarantine then create-new heal", "never overwrite the damaged object in place", false, EVIDENCE_SCRUB, "quarantine on failed heal", "detection is not successful repair"),
        F::CorruptRepairSource => failure_contract(failure, "generation recoverable from independently authenticated symbols", "surviving-source RPO", "repair plus reverify RTO", "exclude corrupt source and recompute rank", "never trust decoder output before authentication", false, EVIDENCE_SCRUB, "insufficient-rank quarantine", "a decoder result is not a verified snapshot"),
        F::ClientViewStateLoss => failure_contract(failure, "verified mux truth with safe client reset", "mux RPO; client state unavailable", "mux recovery plus client reset RTO", "automatic safe reset", "discard transient input, IME, clipboard, credentials, and handles", false, EVIDENCE_CLIENT, "server usable with reset client", "server recovery does not imply client-view preservation"),
        F::ClientViewStateConflict => failure_contract(failure, "verified mux truth with conflicting client envelope quarantined", "mux RPO; client state conflicted", "mux recovery plus operator/client reset RTO", "automatic server recovery; client reset", "never apply conflicting selection, zoom, or viewport state", false, EVIDENCE_CLIENT, "server usable with reset client", "client envelope validity does not override root, domain, or pane identity"),
    }
}

/// Return one cell from the exhaustive failure/capability cross-product.
#[must_use]
pub const fn snapshot_recovery_contract_cell(
    failure: SnapshotRecoveryFailureClass,
    capability: SnapshotRecoveryCapability,
) -> SnapshotRecoveryContractCell {
    use SnapshotRecoveryCapability as C;
    use SnapshotRecoveryCapabilityAvailability as A;
    use SnapshotRecoveryFailureClass as F;

    let (availability, nonclaim) = match capability {
        C::GuardianLiveProcessReattachment => match failure {
            F::GuardianCrash | F::FullHostPowerLoss | F::CompleteHostLoss | F::CorrelatedSiteLoss => (
                A::Forbidden,
                "live process execution cannot be reconstructed from serialized bytes",
            ),
            _ => (
                A::CandidateAfterExactGuards,
                "reattachment requires a live authenticated guardian plus exact lease, replay, and input authority",
            ),
        },
        C::ExactTerminalParserRenderReconstruction => match failure {
            F::KeyLoss | F::LocalCredentialStoreLoss | F::LocalMediaLoss | F::CompleteHostLoss | F::CorrelatedSiteLoss => (
                A::RequiresIndependentDurability,
                "reconstruction requires a decryptable independently retained semantically complete image",
            ),
            _ => (
                A::CandidateAfterExactGuards,
                "forensic text or topology metadata is not terminal parser/render state",
            ),
        },
        C::TopologyLayoutRecreation => match failure {
            F::KeyLoss | F::LocalCredentialStoreLoss | F::LocalMediaLoss | F::CompleteHostLoss | F::CorrelatedSiteLoss => (
                A::RequiresIndependentDurability,
                "topology recreation requires an independently retained authoritative topology object",
            ),
            _ => (
                A::CandidateAfterExactGuards,
                "pane labels and best-effort projections are not authoritative topology",
            ),
        },
        C::PolicyGatedProcessReplacement => (
            A::CandidateAfterExactGuards,
            "replacement creates new processes and never claims continuity of process memory or external resources",
        ),
        C::ForensicContentExport => (
            A::CandidateAfterExactGuards,
            "forensic content is read-only evidence and never executable recovery authority",
        ),
    };
    SnapshotRecoveryContractCell {
        failure,
        capability,
        availability,
        nonclaim,
    }
}

/// One machine-validated proof-manifest entry for this contract bead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SnapshotRecoveryContractProofEntry {
    pub invariant_id: &'static str,
    pub owner_bead: &'static str,
    pub fixture_or_oracle: &'static str,
    pub assertion: &'static str,
    pub package_or_script: &'static str,
    pub exact_filter_or_scenario: &'static str,
    pub test_layer: &'static str,
    pub platform: &'static str,
    pub required_artifacts: &'static str,
    pub causal_fault_or_mutation: &'static str,
}

/// Executable registration for local invariants and the downstream real e2e
/// consumer owned by the disaster-journey bead.
pub const SNAPSHOT_RECOVERY_CONTRACT_PROOF_MANIFEST: &[SnapshotRecoveryContractProofEntry] = &[
    SnapshotRecoveryContractProofEntry {
        invariant_id: "snapshot.contract.failure_capability_cross_product",
        owner_bead: SNAPSHOT_RECOVERY_CONTRACT_OWNER,
        fixture_or_oracle: "SnapshotRecoveryFailureClass::ALL x SnapshotRecoveryCapability::ALL",
        assertion: "every cell is total and carries a nonclaim",
        package_or_script: "frankenterm-core",
        exact_filter_or_scenario: "snapshot_recovery_contract_matrix_is_total_and_nonclaiming",
        test_layer: "unit",
        platform: "all",
        required_artifacts: "source identity plus nonzero exact test selection",
        causal_fault_or_mutation: "remove one failure/capability row or clear one nonclaim",
    },
    SnapshotRecoveryContractProofEntry {
        invariant_id: "snapshot.contract.independent_verdicts",
        owner_bead: SNAPSHOT_RECOVERY_CONTRACT_OWNER,
        fixture_or_oracle: "fully verified whole-mux evidence profile",
        assertion: "validity, semantics, durability, freshness, scrub, drill, and client disposition do not imply one another",
        package_or_script: "frankenterm-core",
        exact_filter_or_scenario: "snapshot_recovery_claim_guard_rejects_each_missing_independent_fact",
        test_layer: "mutation",
        platform: "all",
        required_artifacts: "bounded content-free assertion log",
        causal_fault_or_mutation: "replace exactly one independent verdict with unknown, stale, overdue, failed, or conflict",
    },
    SnapshotRecoveryContractProofEntry {
        invariant_id: "snapshot.contract.forensic_nonpromotion",
        owner_bead: SNAPSHOT_RECOVERY_CONTRACT_OWNER,
        fixture_or_oracle: "verified mux dump and checkpoint-scrollback export",
        assertion: "forensic export is candidate-only and never mutation-capable",
        package_or_script: "frankenterm-core and frankenterm",
        exact_filter_or_scenario: "snapshot_recovery_forensic_artifacts_cannot_be_promoted",
        test_layer: "integration",
        platform: "all",
        required_artifacts: "independent verifier receipt",
        causal_fault_or_mutation: "request executable capability or interactive-safe/complete readiness",
    },
    SnapshotRecoveryContractProofEntry {
        invariant_id: "snapshot.contract.power_loss_nonclaim",
        owner_bead: SNAPSHOT_RECOVERY_CONTRACT_OWNER,
        fixture_or_oracle: "mux SIGKILL and isolated-host power-cut scenarios",
        assertion: "process crash evidence cannot satisfy host-power-loss evidence",
        package_or_script: "tests/e2e snapshot disaster harness",
        exact_filter_or_scenario: "snapshot_contract_host_power_loss_nonclaim",
        test_layer: "e2e",
        platform: "linux-and-macos",
        required_artifacts: "source, binary, filesystem capability, fault, root, and replay identities",
        causal_fault_or_mutation: "label SIGKILL-only evidence as host power loss",
    },
    SnapshotRecoveryContractProofEntry {
        invariant_id: "snapshot.contract.clean_host_progressive_recovery",
        owner_bead: SNAPSHOT_RECOVERY_CONTRACT_OWNER,
        fixture_or_oracle: "ft-interactive-swarm-product-convergence-7xqz4.8.14.4.3",
        assertion: "clean host reaches interactive-safe before complete without authority invention",
        package_or_script: "tests/e2e snapshot disaster harness",
        exact_filter_or_scenario: "snapshot_contract_clean_host_progressive_recovery",
        test_layer: "e2e",
        platform: "linux-and-macos",
        required_artifacts: "bootstrap, key-wrapper, replica, freshness, lease, replay, topology, and client-disposition receipts",
        causal_fault_or_mutation: "remove one authority, inject stale/split view, or promote interactive-safe to complete",
    },
];

/// Result of a successful snapshot capture.
#[derive(Clone)]
pub struct SnapshotResult {
    /// Time-ordered `sess-<timestamp>-<random>` session ID (UUID-v7-like).
    pub session_id: String,
    /// Checkpoint row ID in SQLite.
    pub checkpoint_id: i64,
    /// Authoritative checkpoint timestamp (epoch milliseconds).
    pub checkpoint_at: u64,
    /// Exact persisted v2 witness bound to this checkpoint row.
    pub state_hash: String,
    /// Number of panes captured.
    pub pane_count: usize,
    /// Historical serialized terminal/environment/agent pane-state JSON byte
    /// estimate. This excludes topology, cwd, command, and SQLite overhead.
    pub total_bytes: usize,
    /// Exact admitted UTF-8 bytes across topology, metadata, and every pane
    /// text column. This excludes SQLite record/index overhead.
    pub persisted_text_bytes: usize,
    /// Number of pane projections whose optional text was shortened or
    /// omitted to satisfy the durable row budget.
    pub truncated_pane_count: usize,
    /// What triggered this snapshot.
    pub trigger: SnapshotTrigger,
}

impl std::fmt::Debug for SnapshotResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotResult")
            .field("checkpoint_id", &self.checkpoint_id)
            .field("checkpoint_at", &self.checkpoint_at)
            .field("pane_count", &self.pane_count)
            .field("total_bytes", &self.total_bytes)
            .field("persisted_text_bytes", &self.persisted_text_bytes)
            .field("truncated_pane_count", &self.truncated_pane_count)
            .field("trigger", &self.trigger)
            .finish_non_exhaustive()
    }
}

/// Role scope used when resolving a deletion target inside its transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotCheckpointRoleScope {
    /// Resolve only restorable snapshot rows.
    Snapshot,
    /// Resolve snapshots or restore receipts.
    Any,
}

/// Immutable checkpoint identity used to protect an interactive confirmation
/// from SQLite ROWID reuse between preview and deletion.
#[derive(Clone, PartialEq, Eq)]
pub struct SnapshotCheckpointIdentity {
    pub checkpoint_id: i64,
    pub session_id: String,
    pub checkpoint_at: u64,
    pub checkpoint_role: String,
    pub state_hash: String,
}

impl std::fmt::Debug for SnapshotCheckpointIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotCheckpointIdentity")
            .field("checkpoint_id", &self.checkpoint_id)
            .field("checkpoint_at", &self.checkpoint_at)
            .finish_non_exhaustive()
    }
}

/// In-memory periodic-dedup hint bound to the exact durable row that made the
/// hint safe. The database identity is revalidated before every skip because
/// another engine or process may prune that row without access to this cache.
#[derive(Clone, PartialEq, Eq)]
struct LastDedupCheckpoint {
    dedup_hash: String,
    identity: SnapshotCheckpointIdentity,
}

impl std::fmt::Debug for LastDedupCheckpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LastDedupCheckpoint")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Transaction-local checkpoint deletion selector.
#[derive(Clone, PartialEq, Eq)]
pub enum SnapshotDeleteTarget {
    /// Delete the row currently carrying this numeric ID.
    Id(i64),
    /// Delete only if every immutable identity field still matches.
    Exact(SnapshotCheckpointIdentity),
    /// Resolve and delete the deterministic latest row atomically.
    Latest(SnapshotCheckpointRoleScope),
}

impl std::fmt::Debug for SnapshotDeleteTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Id(checkpoint_id) => formatter.debug_tuple("Id").field(checkpoint_id).finish(),
            Self::Exact(identity) => formatter.debug_tuple("Exact").field(identity).finish(),
            Self::Latest(scope) => formatter.debug_tuple("Latest").field(scope).finish(),
        }
    }
}

/// Receipt for one authority-serialized checkpoint deletion.
#[derive(Clone, PartialEq, Eq)]
pub struct SnapshotDeleteResult {
    /// Exact identity of the row that was deleted.
    pub identity: SnapshotCheckpointIdentity,
    /// Historical payload-byte estimate recorded on the checkpoint row. This
    /// is not an exact SQLite file-size reduction.
    pub recorded_payload_bytes: u64,
    /// Whether deleting this row invalidated the session's exact clean-state
    /// receipt and therefore forced the session back to unclean.
    pub invalidated_clean_state: bool,
}

impl std::fmt::Debug for SnapshotDeleteResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotDeleteResult")
            .field("identity", &self.identity)
            .field("recorded_payload_bytes", &self.recorded_payload_bytes)
            .field("invalidated_clean_state", &self.invalidated_clean_state)
            .finish()
    }
}

/// Per-capture options for call sites that need snapshot behavior beyond the
/// engine's normal periodic/event defaults.
#[derive(Default)]
pub struct SnapshotCaptureOptions {
    /// Link the checkpoint to already-captured scrollback segments.
    pub include_scrollback: bool,
    /// Optional operator/event metadata persisted atomically with the
    /// checkpoint and covered by its v2 witness.
    pub metadata: Option<Value>,
}

/// Destroy an owned JSON tree without recursive `Value::drop` calls. Public
/// capture options accept programmatically constructed values, so their depth
/// can exceed serde_json's parser recursion limit by an arbitrary amount.
fn drop_json_value_iteratively(value: Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(mut children) => pending.append(&mut children),
            Value::Object(fields) => pending.extend(fields.into_values()),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
}

impl Drop for SnapshotCaptureOptions {
    fn drop(&mut self) {
        if let Some(metadata) = self.metadata.take() {
            drop_json_value_iteratively(metadata);
        }
    }
}

/// Safe owner for the blocking handoff. If the executor suppresses and drops
/// the closure before it starts, or the caller cancels while it is queued, the
/// captured JSON still receives iterative destruction.
struct IterativelyDroppedJsonValue(Option<Value>);

impl IterativelyDroppedJsonValue {
    fn new(value: Value) -> Self {
        Self(Some(value))
    }

    fn value(&self) -> &Value {
        self.0
            .as_ref()
            .expect("iterative JSON owner retains its value until drop")
    }
}

impl Drop for IterativelyDroppedJsonValue {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            drop_json_value_iteratively(value);
        }
    }
}

impl std::fmt::Debug for SnapshotCaptureOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotCaptureOptions")
            .field("include_scrollback", &self.include_scrollback)
            .field("has_metadata", &self.metadata.is_some())
            .finish_non_exhaustive()
    }
}

/// Finite identity for a durable snapshot-authority mutation.
///
/// These labels intentionally contain no session IDs, paths, or pane content,
/// so an indeterminate outcome can be reported without leaking authority data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotAuthorityOperation {
    /// Atomically create/update a session and persist one checkpoint.
    CheckpointCommit,
    /// Delete checkpoints under the configured retention policy.
    CheckpointCleanup,
    /// Delete sessions and their dependent state under session retention.
    SessionRetentionCleanup,
    /// Mark the current session as cleanly shut down.
    ShutdownMark,
    /// Delete one exact checkpoint and reconcile its session summary.
    CheckpointDelete,
    /// Persist an unclean restore receipt after layout reconstruction.
    RestoreReceiptCommit,
    /// Bind an exact restore receipt after process relaunch settles.
    RestoreCleanMark,
    /// Delete one operator-selected session and its dependent state.
    SessionDelete,
    /// Persist a restore attempt intent before the first external mux effect.
    RestoreIntentCommit,
}

impl SnapshotAuthorityOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CheckpointCommit => "checkpoint_commit",
            Self::CheckpointCleanup => "checkpoint_cleanup",
            Self::SessionRetentionCleanup => "session_retention_cleanup",
            Self::ShutdownMark => "shutdown_mark",
            Self::CheckpointDelete => "checkpoint_delete",
            Self::RestoreReceiptCommit => "restore_receipt_commit",
            Self::RestoreCleanMark => "restore_clean_mark",
            Self::SessionDelete => "session_delete",
            Self::RestoreIntentCommit => "restore_intent_commit",
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::CheckpointCommit => 1,
            Self::CheckpointCleanup => 2,
            Self::SessionRetentionCleanup => 3,
            Self::ShutdownMark => 4,
            Self::CheckpointDelete => 5,
            Self::RestoreReceiptCommit => 6,
            Self::RestoreCleanMark => 7,
            Self::SessionDelete => 8,
            Self::RestoreIntentCommit => 9,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::CheckpointCommit),
            2 => Some(Self::CheckpointCleanup),
            3 => Some(Self::SessionRetentionCleanup),
            4 => Some(Self::ShutdownMark),
            5 => Some(Self::CheckpointDelete),
            6 => Some(Self::RestoreReceiptCommit),
            7 => Some(Self::RestoreCleanMark),
            8 => Some(Self::SessionDelete),
            9 => Some(Self::RestoreIntentCommit),
            _ => None,
        }
    }
}

impl std::fmt::Display for SnapshotAuthorityOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Finite, content-free resource classes for live snapshot projection limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotProjectionResource {
    /// Canonical operator/event metadata JSON.
    MetadataBytes,
    /// Maximum nesting depth of operator/event metadata.
    MetadataDepth,
    /// Total scalar/container nodes in operator/event metadata.
    MetadataNodes,
    /// Canonical text-bearing columns for one pane row.
    PaneTextBytes,
    /// Aggregate canonical text admitted for one checkpoint.
    CheckpointTextBytes,
}

impl SnapshotProjectionResource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MetadataBytes => "metadata_bytes",
            Self::MetadataDepth => "metadata_depth",
            Self::MetadataNodes => "metadata_nodes",
            Self::PaneTextBytes => "pane_text_bytes",
            Self::CheckpointTextBytes => "checkpoint_text_bytes",
        }
    }
}

impl std::fmt::Display for SnapshotProjectionResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Error returned when a snapshot-engine operation cannot complete safely.
#[derive(thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot already in progress")]
    InProgress,
    #[error("snapshot capture admission is closed for shutdown")]
    ShuttingDown,
    #[error("snapshot scheduler already running for this engine")]
    SchedulerInProgress,
    #[error("snapshot intelligent scheduler trigger receiver is unavailable")]
    TriggerReceiverUnavailable,
    #[error("no panes found")]
    NoPanes,
    #[error("no changes since last snapshot")]
    NoChanges,
    #[error("pane listing failed")]
    PaneList(String),
    #[error("snapshot database operation failed")]
    Database(String),
    #[error("snapshot serialization failed")]
    Serialization(String),
    #[error("topology admission failed")]
    Topology(#[from] TopologySnapshotError),
    #[error(
        "snapshot projection resource admission failed for {resource}: {observed} exceeds {limit}"
    )]
    ProjectionResourceLimit {
        /// Finite resource class; cannot contain pane/session content.
        resource: SnapshotProjectionResource,
        /// Quantity observed when admission stopped. A streaming byte counter
        /// can stop at the first over-limit chunk, so this need not be the
        /// hypothetical fully serialized size of rejected input.
        observed: usize,
        /// Configured hard admission limit.
        limit: usize,
    },
    #[error(
        "snapshot topology/pane-state identity sets disagree (topology panes: {topology_panes}, pane states: {pane_states})"
    )]
    PaneIdentitySetMismatch {
        /// Pane identities declared by the admitted topology.
        topology_panes: usize,
        /// Pane identities supplied as persisted pane rows.
        pane_states: usize,
    },
    /// The blocking closure was admitted, but its authoritative result was
    /// lost. The closure may still be running or may already have committed.
    #[error(
        "snapshot authority outcome is indeterminate after {operation} handoff; reconcile durable state before retrying"
    )]
    IndeterminateAuthorityMutation {
        /// Finite operation identity; contains no session data.
        operation: SnapshotAuthorityOperation,
    },
    /// A prior mutation lost its authoritative result. All later durable
    /// mutations fail closed until an external authority reconciles the durable
    /// state. Merely constructing a fresh engine does not prove reconciliation.
    #[error(
        "snapshot authority reconciliation is required before {operation}; first indeterminate operation: {first_indeterminate_operation:?}; durable mutation suppressed"
    )]
    AuthorityReconciliationRequired {
        /// Mutation that was suppressed by the sticky latch.
        operation: SnapshotAuthorityOperation,
        /// First operation that latched this same-process database authority.
        first_indeterminate_operation: Option<SnapshotAuthorityOperation>,
    },
    /// Another durable mutation currently owns the engine's exclusive
    /// authority admission. No work for this attempted mutation was handed to
    /// the blocking pool, so the caller may retry after the owner settles.
    #[error("snapshot authority mutation already in progress during {operation}")]
    AuthorityMutationInProgress {
        /// Mutation that could not acquire exclusive authority.
        operation: SnapshotAuthorityOperation,
    },
    /// The caller's capability context (`Cx`) cancelled before a durable
    /// blocking mutation was admitted, or during non-mutating preflight.
    /// Cancellation after mutation admission is indeterminate instead.
    #[error("snapshot capture cancelled via capability context")]
    Cancelled,
    #[error("snapshot capability deadline exceeded")]
    DeadlineExceeded,
    #[error("snapshot capability poll quota exhausted")]
    PollQuotaExhausted,
    #[error("snapshot capability cost budget exhausted")]
    CostBudgetExhausted,
    #[error("snapshot capability context failed")]
    ContextFailure,
    /// The blocking executor failed before the guarded closure began. The
    /// start/suppress handshake proves no durable mutation ran, so this is not
    /// an indeterminate authority outcome and may be retried.
    #[error("snapshot blocking runtime failed before authority mutation started")]
    BlockingRuntimeFailure,
    /// The explicit shutdown deadline elapsed before both the final-checkpoint
    /// and clean-mark receipts were observed. No fresh mutation is launched
    /// after this boundary, so the method never silently exceeds its timeout.
    #[error("shutdown checkpoint did not settle within {timeout_ms}ms")]
    ShutdownTimedOut { timeout_ms: u64 },
    /// A final checkpoint settled, but the clean-shutdown mark failed. Preserve
    /// the committed checkpoint receipt so callers can report partial durable
    /// progress without claiming a clean session.
    #[error("clean-shutdown mark failed after final checkpoint settlement")]
    ShutdownMarkFailed {
        checkpoint: Box<SnapshotResult>,
        #[source]
        source: Box<SnapshotError>,
    },
    #[error("snapshot lock acquisition timed out at {deadline_nanos}ns")]
    LockTimedOut { deadline_nanos: u64 },
    #[error("snapshot lock is poisoned")]
    LockPoisoned,
    #[error("snapshot lock acquisition future polled after completion")]
    LockPolledAfterCompletion,
}

fn topology_snapshot_error_class(error: &TopologySnapshotError) -> &'static str {
    match error {
        TopologySnapshotError::TooLarge { .. } => "topology_too_large",
        TopologySnapshotError::TooDeep { .. } => "topology_too_deep",
        TopologySnapshotError::ResourceLimit { .. } => "topology_resource_limit",
        TopologySnapshotError::InvalidStructure { .. } => "topology_invalid_structure",
        TopologySnapshotError::Json(_) => "topology_json_invalid",
    }
}

impl std::fmt::Debug for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InProgress => formatter.write_str("InProgress"),
            Self::ShuttingDown => formatter.write_str("ShuttingDown"),
            Self::SchedulerInProgress => formatter.write_str("SchedulerInProgress"),
            Self::TriggerReceiverUnavailable => formatter.write_str("TriggerReceiverUnavailable"),
            Self::NoPanes => formatter.write_str("NoPanes"),
            Self::NoChanges => formatter.write_str("NoChanges"),
            Self::PaneList(_) => formatter.write_str("PaneList"),
            Self::Database(_) => formatter.write_str("Database"),
            Self::Serialization(_) => formatter.write_str("Serialization"),
            Self::Topology(error) => formatter
                .debug_struct("Topology")
                .field("classification", &topology_snapshot_error_class(error))
                .finish_non_exhaustive(),
            Self::ProjectionResourceLimit {
                resource,
                observed,
                limit,
            } => formatter
                .debug_struct("ProjectionResourceLimit")
                .field("resource", resource)
                .field("observed", observed)
                .field("limit", limit)
                .finish(),
            Self::PaneIdentitySetMismatch {
                topology_panes,
                pane_states,
            } => formatter
                .debug_struct("PaneIdentitySetMismatch")
                .field("topology_panes", topology_panes)
                .field("pane_states", pane_states)
                .finish(),
            Self::IndeterminateAuthorityMutation { operation } => formatter
                .debug_struct("IndeterminateAuthorityMutation")
                .field("operation", operation)
                .finish(),
            Self::AuthorityReconciliationRequired {
                operation,
                first_indeterminate_operation,
            } => formatter
                .debug_struct("AuthorityReconciliationRequired")
                .field("operation", operation)
                .field(
                    "first_indeterminate_operation",
                    first_indeterminate_operation,
                )
                .finish(),
            Self::AuthorityMutationInProgress { operation } => formatter
                .debug_struct("AuthorityMutationInProgress")
                .field("operation", operation)
                .finish(),
            Self::Cancelled => formatter.write_str("Cancelled"),
            Self::DeadlineExceeded => formatter.write_str("DeadlineExceeded"),
            Self::PollQuotaExhausted => formatter.write_str("PollQuotaExhausted"),
            Self::CostBudgetExhausted => formatter.write_str("CostBudgetExhausted"),
            Self::ContextFailure => formatter.write_str("ContextFailure"),
            Self::BlockingRuntimeFailure => formatter.write_str("BlockingRuntimeFailure"),
            Self::ShutdownTimedOut { timeout_ms } => formatter
                .debug_struct("ShutdownTimedOut")
                .field("timeout_ms", timeout_ms)
                .finish(),
            Self::ShutdownMarkFailed { checkpoint, source } => formatter
                .debug_struct("ShutdownMarkFailed")
                .field("checkpoint_id", &checkpoint.checkpoint_id)
                .field("source_class", &source.diagnostic_class())
                .finish_non_exhaustive(),
            Self::LockTimedOut { deadline_nanos } => formatter
                .debug_struct("LockTimedOut")
                .field("deadline_nanos", deadline_nanos)
                .finish(),
            Self::LockPoisoned => formatter.write_str("LockPoisoned"),
            Self::LockPolledAfterCompletion => formatter.write_str("LockPolledAfterCompletion"),
        }
    }
}

impl SnapshotError {
    /// Finite class suitable for telemetry and logs. Unlike `Display`, this is
    /// deliberately machine-stable and never incorporates source strings.
    fn diagnostic_class(&self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::ShuttingDown => "shutting_down",
            Self::SchedulerInProgress => "scheduler_in_progress",
            Self::TriggerReceiverUnavailable => "trigger_receiver_unavailable",
            Self::NoPanes => "no_panes",
            Self::NoChanges => "no_changes",
            Self::PaneList(_) => "pane_list",
            Self::Database(_) => "database",
            Self::Serialization(_) => "serialization",
            Self::Topology(error) => topology_snapshot_error_class(error),
            Self::ProjectionResourceLimit { .. } => "projection_resource_limit",
            Self::PaneIdentitySetMismatch { .. } => "pane_identity_set_mismatch",
            Self::IndeterminateAuthorityMutation { .. } => "indeterminate_authority_mutation",
            Self::AuthorityReconciliationRequired { .. } => "authority_reconciliation_required",
            Self::AuthorityMutationInProgress { .. } => "authority_mutation_in_progress",
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::PollQuotaExhausted => "poll_quota_exhausted",
            Self::CostBudgetExhausted => "cost_budget_exhausted",
            Self::ContextFailure => "context_failure",
            Self::BlockingRuntimeFailure => "blocking_runtime_failure",
            Self::ShutdownTimedOut { .. } => "shutdown_timed_out",
            Self::ShutdownMarkFailed { .. } => "shutdown_mark_failed",
            Self::LockTimedOut { .. } => "lock_timed_out",
            Self::LockPoisoned => "lock_poisoned",
            Self::LockPolledAfterCompletion => "lock_polled_after_completion",
        }
    }

    /// Whether the engine must reconcile durable state before another snapshot
    /// authority mutation can be attempted safely.
    #[must_use]
    pub fn requires_reconciliation(&self) -> bool {
        match self {
            Self::IndeterminateAuthorityMutation { .. }
            | Self::AuthorityReconciliationRequired { .. } => true,
            Self::ShutdownMarkFailed { source, .. } => source.requires_reconciliation(),
            _ => false,
        }
    }

    /// Final checkpoint committed before a later clean-mark failure, if any.
    #[must_use]
    pub fn committed_shutdown_checkpoint(&self) -> Option<&SnapshotResult> {
        match self {
            Self::ShutdownMarkFailed { checkpoint, .. } => Some(checkpoint.as_ref()),
            _ => None,
        }
    }

    /// Whether an automatic scheduler can retry this failure without risking a
    /// duplicate or conflicting durable mutation. This is deliberately narrower
    /// than general recoverability: cancellation/budget failures terminate the
    /// owning capability, while indeterminate outcomes remain fail-closed.
    fn is_retry_safe_scheduler_failure(&self) -> bool {
        matches!(
            self,
            Self::PaneList(_)
                | Self::Database(_)
                | Self::BlockingRuntimeFailure
                | Self::LockTimedOut { .. }
        )
    }

    /// Deterministic live-projection capacity failures retain scheduler demand
    /// with bounded backoff. They are distinct from transient retry-safe errors
    /// and from ordinary serialization/integrity failures.
    fn is_capacity_admission_failure(&self) -> bool {
        matches!(
            self,
            Self::Topology(
                TopologySnapshotError::TooLarge { .. }
                    | TopologySnapshotError::TooDeep { .. }
                    | TopologySnapshotError::ResourceLimit { .. }
                    | TopologySnapshotError::InvalidStructure { .. }
            ) | Self::ProjectionResourceLimit { .. }
        )
    }
}

fn snapshot_lock_error(error: LockAcquireError) -> SnapshotError {
    match error {
        LockAcquireError::Cancelled => SnapshotError::Cancelled,
        LockAcquireError::DeadlineExceeded => SnapshotError::DeadlineExceeded,
        LockAcquireError::PollQuotaExhausted => SnapshotError::PollQuotaExhausted,
        LockAcquireError::CostBudgetExhausted => SnapshotError::CostBudgetExhausted,
        LockAcquireError::ContextFailure => SnapshotError::ContextFailure,
        LockAcquireError::TimedOut { deadline_nanos } => {
            SnapshotError::LockTimedOut { deadline_nanos }
        }
        LockAcquireError::Poisoned => SnapshotError::LockPoisoned,
        LockAcquireError::PolledAfterCompletion => SnapshotError::LockPolledAfterCompletion,
    }
}

fn snapshot_context_error(cx: &crate::cx::Cx) -> SnapshotError {
    match cx.root_cancel_cause().map(|reason| reason.kind) {
        Some(CancelKind::Deadline | CancelKind::Timeout) => SnapshotError::DeadlineExceeded,
        Some(CancelKind::PollQuota) => SnapshotError::PollQuotaExhausted,
        Some(CancelKind::CostBudget) => SnapshotError::CostBudgetExhausted,
        Some(_) => SnapshotError::Cancelled,
        None => SnapshotError::ContextFailure,
    }
}

fn snapshot_cx_checkpoint(cx: &crate::cx::Cx) -> std::result::Result<(), SnapshotError> {
    cx.checkpoint().map_err(|_| snapshot_context_error(cx))
}

fn classify_shutdown_timeout(cx: &crate::cx::Cx, timeout: Duration) -> SnapshotError {
    if cx.root_cancel_cause().is_some() || cx.is_cancel_requested() {
        snapshot_context_error(cx)
    } else {
        SnapshotError::ShutdownTimedOut {
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

// =============================================================================
// SnapshotEngine
// =============================================================================

/// Retry-safe automatic session-cleanup failures and admission contention are
/// retried soon enough to recover without waiting for the normal hours-long
/// cadence, but not on every 250 ms intelligent-scheduler poll.
const SESSION_CLEANUP_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Automatic per-checkpoint pruning is a full authority-table scan. Bound it
/// away from the hot capture path while still guaranteeing that an active
/// scheduler revisits retention promptly. The configured snapshot interval is
/// clamped into this range: high-frequency intelligent captures therefore do
/// not manufacture high-frequency scans, while very sparse configurations do
/// not defer pruning for hours.
const CHECKPOINT_CLEANUP_MIN_INTERVAL: Duration = Duration::from_secs(30);
const CHECKPOINT_CLEANUP_MAX_INTERVAL: Duration = Duration::from_mins(5);
/// Admission contention and retry-safe failure must not turn into a tight
/// cleanup loop, but also must not postpone the next attempt until the normal
/// cadence elapses again.
const CHECKPOINT_CLEANUP_RETRY_DELAY: Duration = Duration::from_secs(30);
/// A scheduler capture that was provably not admitted must retry promptly
/// without spinning or pretending the cadence completed. This is short enough
/// for hazard/memory-pressure triggers while still bounding contention load.
const SCHEDULER_URGENT_CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(250);
const SCHEDULER_BACKGROUND_CAPTURE_RETRY_DELAY: Duration = Duration::from_secs(1);
const SCHEDULER_RETRY_SAFE_CAPTURE_MIN_DELAY: Duration = Duration::from_secs(1);
const SCHEDULER_RETRY_SAFE_CAPTURE_MAX_DELAY: Duration = Duration::from_secs(30);
const CAPTURE_LIFECYCLE_OPEN_IDLE: u8 = 0;
const CAPTURE_LIFECYCLE_OPEN_ACTIVE: u8 = 1;
/// A shutdown owner has fenced new ordinary captures and is waiting for the
/// ordinary capture that was already active when the intent was published.
const CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED: u8 = 2;
/// The pending shutdown owner disappeared; the active ordinary capture still
/// owns the lane, but a later shutdown caller may adopt the sticky intent.
const CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_RETRYABLE: u8 = 3;
/// One shutdown caller owns the terminal checkpoint reservation.
const CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED: u8 = 4;
/// The owned terminal checkpoint is currently being prepared/persisted.
const CAPTURE_LIFECYCLE_SHUTDOWN_ACTIVE: u8 = 5;
/// Shutdown intent remains fenced after a retry-safe owner failure/timeout.
const CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE: u8 = 6;
/// A terminal checkpoint and its clean-mark receipt both settled.
const CAPTURE_LIFECYCLE_SHUTDOWN_COMPLETE: u8 = 7;

/// Typed scheduler result for one pane-provider capture attempt.
///
/// `Unchanged` is an authoritative dedup decision. `Deferred` proves no
/// snapshot settled (no panes were available, or an in-process owner held the
/// capture/authority lane), so callers must preserve the trigger and cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulerCaptureOutcome {
    Captured,
    Unchanged,
    Deferred(SchedulerCaptureDeferredReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulerCaptureDeferredReason {
    Busy,
    NoPanes,
    /// Deterministic admission rejected the current pane projection. The live
    /// pane set may later shrink or become internally consistent, so retain
    /// demand with bounded backoff instead of terminating the scheduler.
    CapacityAdmission,
    /// The attempted work settled without an indeterminate durable effect, but
    /// a transient database, pane-list, blocking-runtime, or lock-timeout
    /// failure prevented a checkpoint receipt.
    RetrySafeFailure,
}

impl SchedulerCaptureOutcome {
    const fn settled(self) -> bool {
        matches!(self, Self::Captured | Self::Unchanged)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SchedulerCaptureRetryState {
    consecutive_backoff_deferrals: u32,
}

impl SchedulerCaptureRetryState {
    fn record_settled(&mut self) {
        self.consecutive_backoff_deferrals = 0;
    }

    fn retry_deadline(
        &mut self,
        now: Instant,
        trigger: SnapshotTrigger,
        reason: SchedulerCaptureDeferredReason,
    ) -> Instant {
        if matches!(
            reason,
            SchedulerCaptureDeferredReason::RetrySafeFailure
                | SchedulerCaptureDeferredReason::CapacityAdmission
        ) {
            self.consecutive_backoff_deferrals =
                self.consecutive_backoff_deferrals.saturating_add(1);
        }
        scheduler_capture_retry_deadline(now, trigger, reason, self.consecutive_backoff_deferrals)
    }
}

fn scheduler_capture_retry_delay(
    trigger: SnapshotTrigger,
    reason: SchedulerCaptureDeferredReason,
    consecutive_backoff_deferrals: u32,
) -> Duration {
    match reason {
        SchedulerCaptureDeferredReason::RetrySafeFailure
        | SchedulerCaptureDeferredReason::CapacityAdmission => {
            let exponent = consecutive_backoff_deferrals.saturating_sub(1).min(5);
            let delay = SCHEDULER_RETRY_SAFE_CAPTURE_MIN_DELAY.saturating_mul(1_u32 << exponent);
            delay.min(SCHEDULER_RETRY_SAFE_CAPTURE_MAX_DELAY)
        }
        SchedulerCaptureDeferredReason::Busy if scheduler_trigger_priority(trigger) == 2 => {
            SCHEDULER_URGENT_CAPTURE_RETRY_DELAY
        }
        SchedulerCaptureDeferredReason::Busy | SchedulerCaptureDeferredReason::NoPanes => {
            SCHEDULER_BACKGROUND_CAPTURE_RETRY_DELAY
        }
    }
}

fn scheduler_capture_retry_deadline(
    now: Instant,
    trigger: SnapshotTrigger,
    reason: SchedulerCaptureDeferredReason,
    consecutive_backoff_deferrals: u32,
) -> Instant {
    // Both fixed delays are tiny relative to every representable production
    // `Instant`. Falling back to `now` at the numeric ceiling preserves
    // liveness instead of silently dropping a deferred trigger.
    now.checked_add(scheduler_capture_retry_delay(
        trigger,
        reason,
        consecutive_backoff_deferrals,
    ))
    .unwrap_or(now)
}

const fn scheduler_trigger_priority(trigger: SnapshotTrigger) -> u8 {
    match trigger {
        SnapshotTrigger::HazardThreshold | SnapshotTrigger::MemoryPressure => 2,
        SnapshotTrigger::Event
        | SnapshotTrigger::WorkCompleted
        | SnapshotTrigger::StateTransition
        | SnapshotTrigger::IdleWindow => 1,
        SnapshotTrigger::Periodic
        | SnapshotTrigger::PeriodicFallback
        | SnapshotTrigger::Manual
        | SnapshotTrigger::Shutdown
        | SnapshotTrigger::Startup => 0,
    }
}

const fn should_upgrade_pending_scheduler_trigger(
    pending: SnapshotTrigger,
    candidate: SnapshotTrigger,
    candidate_is_due: bool,
) -> bool {
    candidate_is_due && scheduler_trigger_priority(candidate) > scheduler_trigger_priority(pending)
}

fn intelligent_scheduler_poll_wait(
    fallback_wait: Duration,
    capture_retry_wait: Option<Duration>,
) -> Duration {
    capture_retry_wait.unwrap_or(fallback_wait)
}

fn due_intelligent_scheduler_retry(
    pending_trigger: Option<SnapshotTrigger>,
    capture_retry_at: Option<Instant>,
    now: Instant,
) -> Option<SnapshotTrigger> {
    pending_trigger
        .zip(capture_retry_at)
        .and_then(|(trigger, retry_at)| (now >= retry_at).then_some(trigger))
}

/// Per-scheduler automatic session-cleanup cadence state.
///
/// `last_authoritative_success` advances only after a typed cleanup receipt
/// confirms that every enabled recovery-usability reconciliation phase was
/// complete. `retry_deferred_at` rate-limits bounded reconciliation progress,
/// admission contention, and failures that are explicitly safe to retry.
/// Indeterminate outcomes are governed by the engine-owned sticky
/// reconciliation latch instead of this schedule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SessionCleanupSchedule {
    last_authoritative_success: Option<Instant>,
    retry_deferred_at: Option<Instant>,
}

impl SessionCleanupSchedule {
    fn defer_retry(&mut self, now: Instant) {
        self.retry_deferred_at = Some(now);
    }

    fn record_authoritative_success(&mut self, now: Instant) {
        self.last_authoritative_success = Some(now);
        self.retry_deferred_at = None;
    }
}

/// Process-local, database-keyed checkpoint-pruning cadence.
///
/// This lives inside [`SnapshotAuthorityState`] rather than one scheduler so
/// independently constructed engines that target the same SQLite object share
/// both the last successful scan and one in-flight admission bit. `Instant`
/// makes every decision immune to wall-clock jumps.
#[derive(Debug, Clone, Copy)]
struct CheckpointCleanupCadence {
    last_authoritative_success: Option<Instant>,
    retry_deferred_at: Option<Instant>,
    in_progress: bool,
}

impl CheckpointCleanupCadence {
    fn new() -> Self {
        Self {
            last_authoritative_success: None,
            retry_deferred_at: None,
            in_progress: false,
        }
    }
}

/// Shared automatic-cleanup admission. Dropping an unfinished attempt always
/// releases admission and publishes a bounded retry deadline. If another
/// explicit cleanup completed after this attempt was claimed, that newer
/// authoritative success wins and no redundant retry is scheduled.
struct CheckpointCleanupAttemptGuard {
    authority: Arc<SnapshotAuthorityState>,
    claimed_at: Instant,
}

impl Drop for CheckpointCleanupAttemptGuard {
    fn drop(&mut self) {
        let mut cadence = self
            .authority
            .checkpoint_cleanup_cadence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cadence.in_progress = false;
        if cadence
            .last_authoritative_success
            .is_none_or(|completed_at| completed_at < self.claimed_at)
        {
            cadence.retry_deferred_at = Some(Instant::now());
        }
    }
}

/// Scheduler-local exclusion for one automatic session-cleanup attempt.
/// Durable unknown-effect classification belongs exclusively to the shared
/// start/suppress authority guard below; this guard only releases the scheduler
/// flag and must never turn a provably suppressed queued task into a latch.
struct SessionCleanupAttemptGuard<'a> {
    in_progress: &'a AtomicBool,
}

impl Drop for SessionCleanupAttemptGuard<'_> {
    fn drop(&mut self) {
        self.in_progress.store(false, Ordering::Release);
    }
}

/// Exclusive ownership of the transition from ordinary capture service to
/// terminal shutdown. Reservation is never reopened on drop: once shutdown is
/// attempted, later ordinary captures must remain fenced until a new engine
/// reconstructs authority from durable state.
struct CaptureShutdownReservation<'a> {
    lifecycle: &'a AtomicU8,
}

impl CaptureShutdownReservation<'_> {
    fn begin_final_capture(&self) -> std::result::Result<(), SnapshotError> {
        self.lifecycle
            .compare_exchange(
                CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED,
                CAPTURE_LIFECYCLE_SHUTDOWN_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| SnapshotError::ShuttingDown)
    }

    fn complete(&self) -> std::result::Result<(), SnapshotError> {
        self.lifecycle
            .compare_exchange(
                CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED,
                CAPTURE_LIFECYCLE_SHUTDOWN_COMPLETE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| SnapshotError::ContextFailure)
    }
}

impl Drop for CaptureShutdownReservation<'_> {
    fn drop(&mut self) {
        // Dropping an owner never reopens ordinary admission. Publish the
        // precise retryable state that a later shutdown caller can adopt.
        loop {
            let current = self.lifecycle.load(Ordering::Acquire);
            let retryable = match current {
                CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED => {
                    CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_RETRYABLE
                }
                CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED | CAPTURE_LIFECYCLE_SHUTDOWN_ACTIVE => {
                    CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE
                }
                _ => return,
            };
            if self
                .lifecycle
                .compare_exchange(current, retryable, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }
}

const AUTHORITY_HANDOFF_PENDING: u8 = 0;
const AUTHORITY_HANDOFF_STARTED: u8 = 1;
const AUTHORITY_HANDOFF_SUPPRESSED: u8 = 2;
const AUTHORITY_HANDOFF_COMPLETED: u8 = 3;

/// Same-process authority state shared by every engine that resolves to the
/// same SQLite file identity. This does not replace the durable cross-process
/// intent protocol, but it prevents ad-hoc engines in one process from
/// bypassing each other's admission or sticky reconciliation state.
#[derive(Debug)]
struct SnapshotAuthorityState {
    /// Every stable spelling currently known to identify this database. The
    /// registry lock is always acquired before this lock, so discovering a
    /// hard-link alias and retaining a latch cannot deadlock each other.
    registry_identities: StdMutex<Vec<String>>,
    /// Canonical connection locator chosen by the first engine for this
    /// database object. Hard-link aliases must reuse it so SQLite WAL/journal
    /// sidecars and locks cannot split by pathname even though the inode is the
    /// same.
    connection_locator: Option<String>,
    in_progress: AtomicBool,
    reconciliation_required: AtomicBool,
    session_cleanup_reconciliation_required: AtomicBool,
    first_latched_operation: AtomicU8,
    /// Shared monotonic checkpoint-retention schedule. This prevents multiple
    /// engines for one database from each running the same full scan after a
    /// capture.
    checkpoint_cleanup_cadence: StdMutex<CheckpointCleanupCadence>,
}

impl SnapshotAuthorityState {
    fn new(registry_identity: Option<String>) -> Self {
        Self::new_with_registry_identities(registry_identity.into_iter().collect(), None)
    }

    fn new_with_registry_identities(
        registry_identities: Vec<String>,
        connection_locator: Option<String>,
    ) -> Self {
        Self {
            registry_identities: StdMutex::new(registry_identities),
            connection_locator,
            in_progress: AtomicBool::new(false),
            reconciliation_required: AtomicBool::new(false),
            session_cleanup_reconciliation_required: AtomicBool::new(false),
            first_latched_operation: AtomicU8::new(0),
            checkpoint_cleanup_cadence: StdMutex::new(CheckpointCleanupCadence::new()),
        }
    }

    fn reconciliation_is_required(&self) -> bool {
        self.reconciliation_required.load(Ordering::Acquire)
            || self
                .session_cleanup_reconciliation_required
                .load(Ordering::Acquire)
    }

    fn latch_reconciliation(self: &Arc<Self>, operation: SnapshotAuthorityOperation) {
        let _ = self.first_latched_operation.compare_exchange(
            0,
            operation.code(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.reconciliation_required.store(true, Ordering::Release);
        self.session_cleanup_reconciliation_required
            .store(true, Ordering::Release);
        retain_latched_snapshot_authority(self);
    }

    fn first_latched_operation(&self) -> Option<SnapshotAuthorityOperation> {
        SnapshotAuthorityOperation::from_code(self.first_latched_operation.load(Ordering::Acquire))
    }
}

enum SnapshotAuthorityRegistryEntry {
    Live(Weak<SnapshotAuthorityState>),
    Latched(Arc<SnapshotAuthorityState>),
}

const SNAPSHOT_AUTHORITY_OBJECT_IDENTITY_PREFIX: &str = "sqlite-file-object-unix:";

/// Finite, content-free reason that a filesystem-backed authority can no
/// longer prove it still names the SQLite object originally admitted.
#[derive(Debug, thiserror::Error)]
enum SnapshotAuthorityIdentityError {
    #[error("snapshot database has more than one registered filesystem object identity")]
    MultipleRegisteredObjects,
    #[error("snapshot database resolved to more than one filesystem object identity")]
    MultipleObservedObjects,
    #[error("snapshot database filesystem object disappeared after authority was established")]
    EstablishedObjectMissing,
    #[error("snapshot database filesystem object changed after authority was established")]
    EstablishedObjectReplaced,
    #[error("snapshot database filesystem object is already owned by another authority state")]
    SplitAuthority,
}

fn snapshot_authority_object_identities(identities: &[String]) -> Vec<&str> {
    identities
        .iter()
        .filter_map(|identity| {
            identity
                .starts_with(SNAPSHOT_AUTHORITY_OBJECT_IDENTITY_PREFIX)
                .then_some(identity.as_str())
        })
        .collect()
}

fn snapshot_authority_registry()
-> &'static StdMutex<HashMap<String, SnapshotAuthorityRegistryEntry>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<String, SnapshotAuthorityRegistryEntry>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn retain_latched_snapshot_authority(state: &Arc<SnapshotAuthorityState>) {
    let mut entries = snapshot_authority_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let identities = state
        .registry_identities
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for identity in identities.iter() {
        entries.insert(
            identity.clone(),
            SnapshotAuthorityRegistryEntry::Latched(Arc::clone(state)),
        );
    }
}

/// Publish filesystem object identity after a formerly-missing database has
/// been created. Without this refresh, an engine registered by path before the
/// first commit and a later hard-link spelling could acquire independent
/// same-process authority states for one SQLite inode.
fn refresh_snapshot_authority_file_identities(
    db_path: &str,
    state: &Arc<SnapshotAuthorityState>,
    operation: SnapshotAuthorityOperation,
) -> std::result::Result<(), SnapshotAuthorityIdentityError> {
    let identities = snapshot_authority_file_identities(db_path);
    if identities.is_empty() {
        return Ok(());
    }

    let mut conflicts: Vec<Arc<SnapshotAuthorityState>> = Vec::new();
    let identity_error;
    {
        // Lock order is registry then per-state identities, matching
        // shared_snapshot_authority_state and the type-level invariant.
        let mut entries = snapshot_authority_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for identity in &identities {
            let existing = match entries.get(identity) {
                Some(SnapshotAuthorityRegistryEntry::Live(existing)) => existing.upgrade(),
                Some(SnapshotAuthorityRegistryEntry::Latched(existing)) => {
                    Some(Arc::clone(existing))
                }
                None => None,
            };
            if let Some(existing) = existing
                && !Arc::ptr_eq(&existing, state)
                && !conflicts.iter().any(|known| Arc::ptr_eq(known, &existing))
            {
                conflicts.push(existing);
            }
        }

        let mut registered = state
            .registry_identities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let registered_objects = snapshot_authority_object_identities(&registered);
        let observed_objects = snapshot_authority_object_identities(&identities);
        let mut observed_error = if registered_objects.len() > 1 {
            Some(SnapshotAuthorityIdentityError::MultipleRegisteredObjects)
        } else if observed_objects.len() > 1 {
            Some(SnapshotAuthorityIdentityError::MultipleObservedObjects)
        } else {
            match (registered_objects.first(), observed_objects.first()) {
                (Some(_), None) => Some(SnapshotAuthorityIdentityError::EstablishedObjectMissing),
                (Some(registered), Some(observed)) if registered != observed => {
                    Some(SnapshotAuthorityIdentityError::EstablishedObjectReplaced)
                }
                _ => None,
            }
        };
        if observed_error.is_none() && !conflicts.is_empty() {
            observed_error = Some(SnapshotAuthorityIdentityError::SplitAuthority);
        }

        if observed_error.is_none() {
            for identity in identities {
                if !registered.contains(&identity) {
                    registered.push(identity.clone());
                }
                let entry = if state.reconciliation_is_required() {
                    SnapshotAuthorityRegistryEntry::Latched(Arc::clone(state))
                } else {
                    SnapshotAuthorityRegistryEntry::Live(Arc::downgrade(state))
                };
                entries.insert(identity, entry);
            }
            return Ok(());
        }
        identity_error = observed_error;
    }

    // A split, disappearance, or replacement was observed. Do not guess which
    // state owns the object or whether an alias still references an older
    // inode: latch every participant after releasing registry locks so every
    // future authority observation and mutation fails closed.
    let error = identity_error.unwrap_or(SnapshotAuthorityIdentityError::SplitAuthority);
    tracing::error!(
        reason = %error,
        conflict_count = conflicts.len(),
        "snapshot filesystem authority identity could not be proven"
    );
    state.latch_reconciliation(operation);
    for conflict in conflicts {
        conflict.latch_reconciliation(operation);
    }
    Err(error)
}

fn freeze_filesystem_path_from_base(path: &Path, base: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    std::fs::canonicalize(&absolute)
        .or_else(|_| {
            let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
            let file_name = absolute.file_name().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "snapshot database path has no file name",
                )
            })?;
            std::fs::canonicalize(parent).map(|canonical_parent| canonical_parent.join(file_name))
        })
        .unwrap_or(absolute)
}

fn freeze_filesystem_path(path: &Path) -> PathBuf {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    freeze_filesystem_path_from_base(path, &base)
}

fn filesystem_snapshot_authority_identity(path: &Path) -> String {
    let identity = freeze_filesystem_path(path);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        format!(
            "sqlite-file-unix:{}",
            sqlite_uri_identity_bytes(identity.as_os_str().as_bytes())
        )
    }
    #[cfg(not(unix))]
    {
        format!("sqlite-file:{}", identity.to_string_lossy())
    }
}

fn decode_sqlite_uri_component(raw: &str) -> Vec<u8> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let decoded_byte = bytes
                .get(index + 1..index + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(decoded_byte) = decoded_byte {
                // Bundled SQLite's default URI parser terminates the current
                // component at `%00`. A build with
                // SQLITE_ENABLE_URI_00_ERROR would reject the open instead,
                // for which sharing no authority is moot.
                if decoded_byte == 0 {
                    break;
                }
                decoded.push(decoded_byte);
                index += 3;
                continue;
            }
        }
        // An incomplete or non-hex escape remains a literal `%` in SQLite;
        // only consume that byte so a later valid escape is still decoded.
        decoded.push(bytes[index]);
        index += 1;
    }
    decoded
}

fn sqlite_uri_identity_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(unix)]
fn filesystem_snapshot_authority_identity_from_uri_bytes(bytes: &[u8]) -> String {
    use std::os::unix::ffi::OsStringExt;

    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()));
    filesystem_snapshot_authority_identity(&path)
}

#[cfg(windows)]
fn filesystem_snapshot_authority_identity_from_uri_bytes(bytes: &[u8]) -> String {
    match String::from_utf8(bytes.to_vec()) {
        Ok(path) => {
            // SQLite maps an absolute URI path of `/C:/...` to the ordinary
            // Windows drive path `C:/...`. Feed the same path to Rust so URI
            // and ordinary spellings cannot acquire independent authority.
            let path = sqlite_windows_uri_drive_path(&path);
            filesystem_snapshot_authority_identity(Path::new(path))
        }
        Err(_) => format!("sqlite-uri-file-bytes:{}", sqlite_uri_identity_bytes(bytes)),
    }
}

#[cfg(windows)]
fn sqlite_windows_uri_drive_path(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        &path[1..]
    } else {
        path
    }
}

#[cfg(all(not(unix), not(windows)))]
fn filesystem_snapshot_authority_identity_from_uri_bytes(bytes: &[u8]) -> String {
    match String::from_utf8(bytes.to_vec()) {
        Ok(path) => filesystem_snapshot_authority_identity(Path::new(&path)),
        Err(_) => format!("sqlite-uri-file-bytes:{}", sqlite_uri_identity_bytes(bytes)),
    }
}

fn sqlite_uri_vfs_is_project_default(vfs: Option<&[u8]>) -> bool {
    match vfs {
        None => true,
        Some(vfs) => (cfg!(unix) && vfs == b"unix") || (cfg!(windows) && vfs == b"win32"),
    }
}

fn sqlite_uri_raw_file_path(raw_path: &str) -> Option<&str> {
    let Some(authority_and_path) = raw_path.strip_prefix("//") else {
        return Some(raw_path);
    };
    let (authority, path) =
        authority_and_path
            .find('/')
            .map_or((authority_and_path, ""), |separator| {
                (
                    &authority_and_path[..separator],
                    &authority_and_path[separator..],
                )
            });
    // SQLite validates the literal raw authority before percent-decoding the
    // filename. Encoded spellings of localhost are therefore invalid.
    if authority.is_empty() || authority == "localhost" {
        Some(path)
    } else {
        None
    }
}

fn sqlite_locator_filesystem_path(db_path: &str) -> Option<PathBuf> {
    let Some(uri) = db_path.strip_prefix("file:") else {
        return Some(PathBuf::from(db_path));
    };
    let uri = uri.split_once('#').map_or(uri, |(before, _)| before);
    let raw_path = uri.split_once('?').map_or(uri, |(path, _)| path);
    let decoded = decode_sqlite_uri_component(sqlite_uri_raw_file_path(raw_path)?);

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
    }
    #[cfg(windows)]
    {
        let path = String::from_utf8(decoded).ok()?;
        Some(PathBuf::from(sqlite_windows_uri_drive_path(&path)))
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        String::from_utf8(decoded).ok().map(PathBuf::from)
    }
}

fn is_filesystem_snapshot_authority_identity(identity: &str) -> bool {
    identity.starts_with("sqlite-file-unix:") || identity.starts_with("sqlite-file:")
}

#[cfg(unix)]
fn filesystem_object_snapshot_authority_identity(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some(format!(
        "sqlite-file-object-unix:{}:{}",
        metadata.dev(),
        metadata.ino()
    ))
}

#[cfg(not(unix))]
fn filesystem_object_snapshot_authority_identity(_path: &Path) -> Option<String> {
    None
}

fn snapshot_authority_file_identities(db_path: &str) -> Vec<String> {
    let Some(primary) = snapshot_authority_file_identity(db_path) else {
        return Vec::new();
    };
    let mut identities = vec![primary.clone()];
    if is_filesystem_snapshot_authority_identity(&primary)
        && let Some(path) = sqlite_locator_filesystem_path(db_path)
        && let Some(object_identity) =
            filesystem_object_snapshot_authority_identity(&freeze_filesystem_path(&path))
    {
        identities.push(object_identity);
    }
    identities
}

fn sqlite_uri_encode_path(path: &Path) -> Option<String> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;

        path.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let bytes = path.to_str()?.as_bytes();

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(3));
    for &byte in bytes {
        encoded.push('%');
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Some(encoded)
}

fn freeze_snapshot_db_locator_from_base(db_path: &str, base: &Path) -> String {
    let Some(identity) = snapshot_authority_file_identity(db_path) else {
        return db_path.to_owned();
    };
    if !is_filesystem_snapshot_authority_identity(&identity) {
        return db_path.to_owned();
    }
    let Some(path) = sqlite_locator_filesystem_path(db_path) else {
        return db_path.to_owned();
    };
    let frozen_path = freeze_filesystem_path_from_base(&path, base);
    let Some(encoded_path) = sqlite_uri_encode_path(&frozen_path) else {
        return db_path.to_owned();
    };
    let query = db_path
        .strip_prefix("file:")
        .and_then(|uri| {
            uri.split_once('#')
                .map_or(uri, |(before, _)| before)
                .split_once('?')
        })
        .map_or("", |(_, query)| query);
    if query.is_empty() {
        format!("file:{encoded_path}")
    } else {
        format!("file:{encoded_path}?{query}")
    }
}

fn freeze_snapshot_db_locator(db_path: &str) -> String {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    freeze_snapshot_db_locator_from_base(db_path, &base)
}

fn snapshot_authority_file_identity(db_path: &str) -> Option<String> {
    if db_path == ":memory:" || db_path.is_empty() {
        // Each ordinary :memory: or empty-name temporary connection is a
        // distinct database, so sharing authority would create false coupling.
        return None;
    }

    if let Some(uri) = db_path.strip_prefix("file:") {
        // SQLite ignores a raw URI fragment. Split it before recognizing the
        // query so escaped `%23` bytes remain filename/query data.
        let uri = uri.split_once('#').map_or(uri, |(before, _)| before);
        let (raw_path, query) = uri.split_once('?').unwrap_or((uri, ""));
        let raw_file_path = sqlite_uri_raw_file_path(raw_path)?;
        let mut mode: Option<Vec<u8>> = None;
        let mut cache: Option<Vec<u8>> = None;
        let mut vfs: Option<Vec<u8>> = None;
        for parameter in query.split('&').filter(|value| !value.is_empty()) {
            let (raw_name, raw_value) = parameter.split_once('=').unwrap_or((parameter, ""));
            let name = decode_sqlite_uri_component(raw_name);
            let value = decode_sqlite_uri_component(raw_value);
            // SQLite's URI option names and recognized values are exact,
            // case-sensitive UTF-8 strings. Parse raw separators first and
            // decode each component second so `%26`/`%3d` stay data.
            if name == b"mode" {
                mode = Some(value);
            } else if name == b"cache" {
                cache = Some(value);
            } else if name == b"vfs" {
                vfs = Some(value);
            }
        }
        if mode
            .as_deref()
            .is_some_and(|value| !matches!(value, b"ro" | b"rw" | b"rwc" | b"memory"))
            || cache
                .as_deref()
                .is_some_and(|value| !matches!(value, b"shared" | b"private"))
        {
            // sqlite3ParseUri rejects invalid recognized option values before
            // VFS open. Such an input cannot share durable authority with a
            // valid filename, even if its decoded path happens to match.
            return None;
        }
        if vfs.as_deref() == Some(b"") {
            // The bundled sqlite3ParseUri implementation resolves an explicit
            // empty vfs name with sqlite3_vfs_find("") and rejects it. Do not
            // couple a filename that cannot open to a valid default-VFS owner.
            return None;
        }
        let decoded_path = decode_sqlite_uri_component(raw_file_path);
        let uri_path = decoded_path.as_slice();
        if uri_path.is_empty() {
            // Empty URI filenames are private temporary databases, including
            // `mode=memory&cache=shared`; no name exists to share.
            return None;
        }
        let memory =
            uri_path == b":memory:" || mode.as_deref().is_some_and(|value| value == b"memory");
        if vfs.as_deref() == Some(b"memdb") {
            // Bundled SQLite's memdb VFS chooses its backing-store identity
            // solely from the decoded filename: names longer than one byte
            // beginning with `/` or `\` share one MemStore regardless of
            // mode/cache. Core `cache=shared` also shares a BtShared for the
            // same filename/VFS before a second xOpen. Both mechanisms resolve
            // to one authority key; every other memdb spelling is private.
            let shared = cache.as_deref() == Some(b"shared")
                || (uri_path.len() > 1 && matches!(uri_path.first(), Some(b'/' | b'\\')));
            return shared
                .then(|| format!("sqlite-memdb-vfs:{}", sqlite_uri_identity_bytes(uri_path)));
        }
        if memory {
            if cache.as_deref().is_some_and(|value| value == b"shared") {
                // SQLite shares a named URI memory database only within the
                // process and only when cache=shared. Shared-cache matching
                // compares the resolved VFS pointer, so omitted VFS and the
                // platform's bundled default name are the same authority while
                // genuinely alternate VFS names remain distinct.
                let vfs_identity = if sqlite_uri_vfs_is_project_default(vfs.as_deref()) {
                    &b"default"[..]
                } else {
                    vfs.as_deref().unwrap_or_default()
                };
                return Some(format!(
                    "sqlite-shared-memory:{}:vfs={}",
                    sqlite_uri_identity_bytes(uri_path),
                    sqlite_uri_identity_bytes(vfs_identity)
                ));
            }
            return None;
        }

        return Some(filesystem_snapshot_authority_identity_from_uri_bytes(
            uri_path,
        ));
    }

    Some(filesystem_snapshot_authority_identity(Path::new(db_path)))
}

fn shared_snapshot_authority_state(db_path: &str) -> Arc<SnapshotAuthorityState> {
    let identities = snapshot_authority_file_identities(db_path);
    if identities.is_empty() {
        return Arc::new(SnapshotAuthorityState::new(None));
    }
    let state = {
        let mut entries = snapshot_authority_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.retain(|_, entry| match entry {
            SnapshotAuthorityRegistryEntry::Live(state) => state.strong_count() > 0,
            SnapshotAuthorityRegistryEntry::Latched(_) => true,
        });

        let mut matched_states = Vec::new();
        for identity in &identities {
            let matched = match entries.get(identity) {
                Some(SnapshotAuthorityRegistryEntry::Live(state)) => state.upgrade(),
                Some(SnapshotAuthorityRegistryEntry::Latched(state)) => Some(Arc::clone(state)),
                None => None,
            };
            if let Some(matched) = matched
                && !matched_states
                    .iter()
                    .any(|known| Arc::ptr_eq(known, &matched))
            {
                matched_states.push(matched);
            }
        }

        matched_states.into_iter().next().unwrap_or_else(|| {
            Arc::new(SnapshotAuthorityState::new_with_registry_identities(
                Vec::new(),
                Some(db_path.to_owned()),
            ))
        })
    };

    // Publish every spelling through the same invariant checker. If two
    // identities already resolve to distinct live states, or this pathname
    // now resolves to a replacement inode, the checker latches all known
    // participants instead of silently overwriting one registry entry.
    let _ = refresh_snapshot_authority_file_identities(
        db_path,
        &state,
        SnapshotAuthorityOperation::CheckpointCommit,
    );
    state
}

/// Result transported across the blocking boundary. `Suppressed` means the
/// async owner disappeared (or the executor failed) before the closure began,
/// and the closure therefore proved that it performed no durable work.
enum AuthorityBlockingOutcome<T, E> {
    Suppressed,
    Executed(std::result::Result<T, E>),
    IdentityRefreshFailed(SnapshotAuthorityIdentityError),
}

fn run_authority_work_if_started<T, E, F>(
    handoff_state: &AtomicU8,
    work: F,
) -> AuthorityBlockingOutcome<T, E>
where
    F: FnOnce() -> std::result::Result<T, E>,
{
    if handoff_state
        .compare_exchange(
            AUTHORITY_HANDOFF_PENDING,
            AUTHORITY_HANDOFF_STARTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return AuthorityBlockingOutcome::Suppressed;
    }

    let result = work();
    handoff_state.store(AUTHORITY_HANDOFF_COMPLETED, Ordering::Release);
    AuthorityBlockingOutcome::Executed(result)
}

/// Classifies an error returned by a blocking authority closure. Errors after
/// an ambiguous commit or a multi-transaction cleanup phase require the same
/// sticky reconciliation latch as a lost blocking-task result. Errors proven
/// to precede mutation, or followed by an acknowledged rollback, remain safe
/// to report and retry.
pub(crate) trait SnapshotAuthorityWorkFailure: std::fmt::Display {
    fn requires_reconciliation(&self) -> bool;
}

/// Latches both durable-authority and retention-scheduler reconciliation if a
/// mutation closure started but its typed settlement was not published.
#[derive(Debug)]
struct SnapshotAuthorityAttemptGuard {
    authority: Arc<SnapshotAuthorityState>,
    operation: SnapshotAuthorityOperation,
    handoff_state: Arc<AtomicU8>,
    settled: bool,
}

/// Exclusive same-process admission for a read-only authority observation.
/// Losing this guard never latches reconciliation because the guarded closure
/// cannot mutate durable state; SQLite remains the cross-process serializer.
struct SnapshotAuthorityReadGuard {
    authority: Arc<SnapshotAuthorityState>,
}

impl Drop for SnapshotAuthorityReadGuard {
    fn drop(&mut self) {
        self.authority.in_progress.store(false, Ordering::Release);
    }
}

/// Temporarily lends the engine-owned intelligent-scheduler receiver to one
/// admitted scheduler invocation and restores it on every exit path. The slot
/// uses a standard mutex because it is held only while moving the receiver,
/// never while awaiting channel input.
struct SnapshotTriggerReceiverLease<'a> {
    slot: &'a StdMutex<Option<mpsc::Receiver<SnapshotTrigger>>>,
    receiver: Option<mpsc::Receiver<SnapshotTrigger>>,
}

impl<'a> SnapshotTriggerReceiverLease<'a> {
    fn take(slot: &'a StdMutex<Option<mpsc::Receiver<SnapshotTrigger>>>) -> Option<Self> {
        let receiver = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()?;
        Some(Self {
            slot,
            receiver: Some(receiver),
        })
    }

    fn receiver_mut(
        &mut self,
    ) -> std::result::Result<&mut mpsc::Receiver<SnapshotTrigger>, SnapshotError> {
        self.receiver
            .as_mut()
            .ok_or(SnapshotError::TriggerReceiverUnavailable)
    }
}

impl Drop for SnapshotTriggerReceiverLease<'_> {
    fn drop(&mut self) {
        let Some(receiver) = self.receiver.take() else {
            return;
        };
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(receiver);
        } else {
            tracing::error!(
                "snapshot trigger receiver slot was unexpectedly occupied during lease return"
            );
        }
    }
}

impl SnapshotAuthorityAttemptGuard {
    fn handoff_state(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.handoff_state)
    }

    /// Prevent queued work from starting. Returns `true` only when the closure
    /// had not begun and this caller therefore proves that no mutation ran.
    fn suppress_pending_handoff(&self) -> bool {
        self.handoff_state
            .compare_exchange(
                AUTHORITY_HANDOFF_PENDING,
                AUTHORITY_HANDOFF_SUPPRESSED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn settle(mut self) {
        self.settled = true;
    }

    fn latch_and_settle(mut self) {
        self.authority.latch_reconciliation(self.operation);
        self.settled = true;
    }
}

impl Drop for SnapshotAuthorityAttemptGuard {
    fn drop(&mut self) {
        if !self.settled && !self.suppress_pending_handoff() {
            let state = self.handoff_state.load(Ordering::Acquire);
            if matches!(
                state,
                AUTHORITY_HANDOFF_STARTED | AUTHORITY_HANDOFF_COMPLETED
            ) {
                self.authority.latch_reconciliation(self.operation);
            }
        }
        self.authority.in_progress.store(false, Ordering::Release);
    }
}

fn classify_snapshot_authority_blocking_failure(
    operation: SnapshotAuthorityOperation,
    error: crate::runtime_async::SpawnBlockingWithCxError,
    mutation_started: bool,
) -> SnapshotError {
    if mutation_started {
        return SnapshotError::IndeterminateAuthorityMutation { operation };
    }
    classify_snapshot_pure_blocking_failure(error)
}

/// Map failure of pure CPU/read preparation work. No snapshot authority
/// mutation has been admitted at this point, so even mid-flight cancellation
/// may discard the result without latching durable reconciliation.
fn classify_snapshot_pure_blocking_failure(
    error: crate::runtime_async::SpawnBlockingWithCxError,
) -> SnapshotError {
    match error {
        crate::runtime_async::SpawnBlockingWithCxError::CancelledBeforeSpawn { .. }
        | crate::runtime_async::SpawnBlockingWithCxError::CancelledMidFlight { .. } => {
            SnapshotError::Cancelled
        }
        crate::runtime_async::SpawnBlockingWithCxError::RuntimeFailure => {
            SnapshotError::BlockingRuntimeFailure
        }
        crate::runtime_async::SpawnBlockingWithCxError::CancellationWatcherTimerFailure => {
            SnapshotError::ContextFailure
        }
    }
}

/// Central orchestrator for mux session state capture.
///
/// Thread-safe: one atomic lifecycle word serializes ordinary captures and the
/// terminal shutdown reservation without a precheck-to-claim race.
/// The engine opens its own SQLite connection and moves blocking work off the
/// async workers. SQLite still serializes its write transactions against the
/// high-frequency ingest writer.
pub struct SnapshotEngine {
    /// Path to the SQLite database.
    db_path: Arc<String>,
    /// Snapshot configuration.
    config: SnapshotConfig,
    /// Current session ID (set on first capture).
    session_id: RwLock<Option<String>>,
    /// Stable host-boot and process-incarnation fence captured once. Missing
    /// platform authority leaves sessions fail-closed as legacy/unknown.
    owner_identity: Option<crate::session_retention::SessionOwnerIdentity>,
    /// Stable semantic-state SHA-256 digest used for periodic deduplication.
    last_dedup_hash: RwLock<Option<LastDedupCheckpoint>>,
    /// Atomic combined capture/shutdown lifecycle. One CAS both admits an
    /// ordinary capture and proves shutdown has not reserved the lane, closing
    /// the precheck-to-claim race that separate booleans cannot avoid.
    capture_lifecycle: AtomicU8,
    /// Exactly one scheduler invocation owns this engine at a time. Cadence
    /// state is scheduler-local, so admitting two periodic loops would let a
    /// contender repeat retention cleanup after the first loop succeeds.
    scheduler_in_progress: AtomicBool,
    /// Exclusive admission flag for automatic session-retention cleanup.
    session_cleanup_in_progress: AtomicBool,
    /// Same-process, database-keyed admission and sticky reconciliation state
    /// shared by every engine targeting the same SQLite file.
    snapshot_authority: Arc<SnapshotAuthorityState>,
    /// External trigger ingress sender for intelligent scheduling mode.
    trigger_tx: mpsc::Sender<SnapshotTrigger>,
    /// Runtime-owned receiver, leased by one admitted `run_periodic` call and
    /// restored when that scheduler invocation exits.
    trigger_rx: StdMutex<Option<mpsc::Receiver<SnapshotTrigger>>>,
    /// Operational telemetry counters.
    telemetry: SnapshotEngineTelemetry,
    /// White-box proof seam for metadata-before-auxiliary-read ordering.
    #[cfg(test)]
    auxiliary_projection_read_attempts: AtomicU64,
}

impl SnapshotEngine {
    /// Create a new snapshot engine.
    pub fn new(db_path: Arc<String>, config: SnapshotConfig) -> Self {
        let (trigger_tx, trigger_rx) = mpsc::channel(512);
        // Resolve filesystem-backed locators exactly once. Reusing this stable
        // URI prevents a later process-wide cwd or symlink change from sending
        // durable mutations to a database other than the one whose authority
        // state this engine acquired. Logical in-memory locators remain exact.
        let frozen_db_path = freeze_snapshot_db_locator(db_path.as_str());
        let snapshot_authority = shared_snapshot_authority_state(&frozen_db_path);
        let db_path = Arc::new(
            snapshot_authority
                .connection_locator
                .clone()
                .unwrap_or(frozen_db_path),
        );
        Self {
            db_path,
            config,
            session_id: RwLock::new(None),
            owner_identity: crate::session_retention::current_session_owner_identity(),
            last_dedup_hash: RwLock::new(None),
            capture_lifecycle: AtomicU8::new(CAPTURE_LIFECYCLE_OPEN_IDLE),
            scheduler_in_progress: AtomicBool::new(false),
            session_cleanup_in_progress: AtomicBool::new(false),
            snapshot_authority,
            trigger_tx,
            trigger_rx: StdMutex::new(Some(trigger_rx)),
            telemetry: SnapshotEngineTelemetry::new(),
            #[cfg(test)]
            auxiliary_projection_read_attempts: AtomicU64::new(0),
        }
    }

    /// Emit an event-driven snapshot trigger.
    ///
    /// Returns `false` when the trigger queue is full or no receiver is active.
    #[must_use]
    pub fn emit_trigger(&self, trigger: SnapshotTrigger) -> bool {
        saturating_telemetry_add(&self.telemetry.triggers_emitted, 1);
        let accepted = self.trigger_tx.try_send(trigger).is_ok();
        if accepted {
            saturating_telemetry_add(&self.telemetry.triggers_accepted, 1);
        }
        accepted
    }

    fn authority_reconciliation_is_required(&self) -> bool {
        self.snapshot_authority.reconciliation_is_required()
    }

    fn authority_reconciliation_error(
        &self,
        operation: SnapshotAuthorityOperation,
    ) -> SnapshotError {
        SnapshotError::AuthorityReconciliationRequired {
            operation,
            first_indeterminate_operation: self.snapshot_authority.first_latched_operation(),
        }
    }

    /// Atomically reserve the capture lane for shutdown. If an ordinary capture
    /// already owns it, publish sticky shutdown intent before waiting for that
    /// owner to hand the lane directly to this reservation. A cancelled owner
    /// leaves an adoptable retry state; ordinary admission never reopens.
    async fn reserve_capture_lifecycle(
        &self,
        cx: &crate::cx::Cx,
    ) -> std::result::Result<CaptureShutdownReservation<'_>, SnapshotError> {
        snapshot_cx_checkpoint(cx)?;
        loop {
            let current = self.capture_lifecycle.load(Ordering::Acquire);
            match current {
                CAPTURE_LIFECYCLE_OPEN_IDLE => {
                    if self
                        .capture_lifecycle
                        .compare_exchange(
                            CAPTURE_LIFECYCLE_OPEN_IDLE,
                            CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    return Ok(CaptureShutdownReservation {
                        lifecycle: &self.capture_lifecycle,
                    });
                }
                CAPTURE_LIFECYCLE_OPEN_ACTIVE => {
                    if self
                        .capture_lifecycle
                        .compare_exchange(
                            CAPTURE_LIFECYCLE_OPEN_ACTIVE,
                            CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    let reservation = CaptureShutdownReservation {
                        lifecycle: &self.capture_lifecycle,
                    };
                    loop {
                        match self.capture_lifecycle.load(Ordering::Acquire) {
                            CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED => {
                                return Ok(reservation);
                            }
                            CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED => {
                                crate::runtime_async::sleep_with_cx(cx, Duration::from_millis(1))
                                    .await
                                    .map_err(|_| snapshot_context_error(cx))?;
                            }
                            _ => return Err(SnapshotError::ContextFailure),
                        }
                    }
                }
                CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_RETRYABLE => {
                    if self
                        .capture_lifecycle
                        .compare_exchange(
                            CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_RETRYABLE,
                            CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    let reservation = CaptureShutdownReservation {
                        lifecycle: &self.capture_lifecycle,
                    };
                    loop {
                        match self.capture_lifecycle.load(Ordering::Acquire) {
                            CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED => {
                                return Ok(reservation);
                            }
                            CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED => {
                                crate::runtime_async::sleep_with_cx(cx, Duration::from_millis(1))
                                    .await
                                    .map_err(|_| snapshot_context_error(cx))?;
                            }
                            _ => return Err(SnapshotError::ContextFailure),
                        }
                    }
                }
                CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE => {
                    if self
                        .capture_lifecycle
                        .compare_exchange(
                            CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE,
                            CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    return Ok(CaptureShutdownReservation {
                        lifecycle: &self.capture_lifecycle,
                    });
                }
                CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED
                | CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED
                | CAPTURE_LIFECYCLE_SHUTDOWN_ACTIVE
                | CAPTURE_LIFECYCLE_SHUTDOWN_COMPLETE => {
                    return Err(SnapshotError::ShuttingDown);
                }
                _ => return Err(SnapshotError::ContextFailure),
            }
        }
    }

    /// Try to acquire exclusive admission for one durable authority mutation.
    ///
    /// The second latch check pairs with the predecessor guard's release of
    /// `snapshot_authority_in_progress`: an abandoned admitted handoff stores
    /// both sticky latches before making the admission flag available again.
    fn try_begin_snapshot_authority(
        &self,
        operation: SnapshotAuthorityOperation,
    ) -> std::result::Result<SnapshotAuthorityAttemptGuard, SnapshotError> {
        if self.authority_reconciliation_is_required() {
            return Err(self.authority_reconciliation_error(operation));
        }
        if refresh_snapshot_authority_file_identities(
            self.db_path.as_str(),
            &self.snapshot_authority,
            operation,
        )
        .is_err()
        {
            return Err(self.authority_reconciliation_error(operation));
        }

        if self
            .snapshot_authority
            .in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            if self.authority_reconciliation_is_required() {
                return Err(self.authority_reconciliation_error(operation));
            }
            return Err(SnapshotError::AuthorityMutationInProgress { operation });
        }

        if self.authority_reconciliation_is_required() {
            self.snapshot_authority
                .in_progress
                .store(false, Ordering::Release);
            return Err(self.authority_reconciliation_error(operation));
        }

        Ok(SnapshotAuthorityAttemptGuard {
            authority: Arc::clone(&self.snapshot_authority),
            operation,
            handoff_state: Arc::new(AtomicU8::new(AUTHORITY_HANDOFF_PENDING)),
            settled: false,
        })
    }

    /// Acquire the database-keyed lane for a read-only authority observation.
    /// This is deliberately separate from mutation admission: cancellation or
    /// executor failure cannot make a read indeterminate and therefore must
    /// never poison the durable-mutation lane.
    fn try_begin_snapshot_authority_read(
        &self,
        operation: SnapshotAuthorityOperation,
    ) -> std::result::Result<SnapshotAuthorityReadGuard, SnapshotError> {
        if self.authority_reconciliation_is_required() {
            return Err(self.authority_reconciliation_error(operation));
        }
        if refresh_snapshot_authority_file_identities(
            self.db_path.as_str(),
            &self.snapshot_authority,
            operation,
        )
        .is_err()
        {
            return Err(self.authority_reconciliation_error(operation));
        }
        if self
            .snapshot_authority
            .in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            if self.authority_reconciliation_is_required() {
                return Err(self.authority_reconciliation_error(operation));
            }
            return Err(SnapshotError::AuthorityMutationInProgress { operation });
        }
        if self.authority_reconciliation_is_required() {
            self.snapshot_authority
                .in_progress
                .store(false, Ordering::Release);
            return Err(self.authority_reconciliation_error(operation));
        }
        Ok(SnapshotAuthorityReadGuard {
            authority: Arc::clone(&self.snapshot_authority),
        })
    }

    /// Execute one exclusively admitted durable mutation and preserve the
    /// distinction between cancellation before admission and observation loss
    /// afterwards.
    ///
    /// The attempt guard marks its handoff boundary only after the explicit
    /// pre-handoff checkpoint. If this future is then dropped while the blocking
    /// closure is outstanding, the guard latches reconciliation before it
    /// releases exclusive admission. Every subsequent authority mutation
    /// therefore fails closed rather than racing a closure that may still commit.
    async fn spawn_blocking_authority_with_cx<T, E, F>(
        &self,
        cx: &crate::cx::Cx,
        operation: SnapshotAuthorityOperation,
        work: F,
    ) -> std::result::Result<T, SnapshotError>
    where
        T: Send + 'static,
        E: SnapshotAuthorityWorkFailure + Send + 'static,
        F: FnOnce() -> std::result::Result<T, E> + Send + 'static,
    {
        let attempt = self.try_begin_snapshot_authority(operation)?;
        snapshot_cx_checkpoint(cx)?;

        let handoff_state = attempt.handoff_state();
        let authority_lifetime = Arc::clone(&attempt.authority);
        let db_path_for_identity_refresh = Arc::clone(&self.db_path);
        let outcome = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            // Keep the database-keyed authority alive until the queued closure
            // has either suppressed itself or reached terminal return. A new
            // engine must not prune the Weak registry entry while old work can
            // still commit.
            let outcome = run_authority_work_if_started(&handoff_state, work);
            match outcome {
                AuthorityBlockingOutcome::Executed(result) => {
                    match refresh_snapshot_authority_file_identities(
                        db_path_for_identity_refresh.as_str(),
                        &authority_lifetime,
                        operation,
                    ) {
                        Ok(()) => AuthorityBlockingOutcome::Executed(result),
                        Err(error) => AuthorityBlockingOutcome::IdentityRefreshFailed(error),
                    }
                }
                other => other,
            }
        })
        .await;

        match outcome {
            Ok(AuthorityBlockingOutcome::Executed(Ok(result))) => {
                attempt.settle();
                Ok(result)
            }
            Ok(AuthorityBlockingOutcome::Executed(Err(error))) => {
                if error.requires_reconciliation() {
                    tracing::warn!(
                        %operation,
                        error_class = "indeterminate_database_authority",
                        "snapshot authority work returned an indeterminate database outcome"
                    );
                    attempt.latch_and_settle();
                    Err(SnapshotError::IndeterminateAuthorityMutation { operation })
                } else {
                    attempt.settle();
                    Err(SnapshotError::Database(error.to_string()))
                }
            }
            Ok(AuthorityBlockingOutcome::Suppressed) => {
                attempt.settle();
                Err(SnapshotError::Cancelled)
            }
            Ok(AuthorityBlockingOutcome::IdentityRefreshFailed(error)) => {
                tracing::warn!(
                    %operation,
                    error = %error,
                    "snapshot authority work settled but filesystem identity publication failed"
                );
                attempt.latch_and_settle();
                Err(SnapshotError::IndeterminateAuthorityMutation { operation })
            }
            Err(error) => {
                let no_mutation_started = attempt.suppress_pending_handoff();
                let mutation_started = !no_mutation_started;
                if mutation_started {
                    attempt.latch_and_settle();
                } else {
                    attempt.settle();
                }
                Err(classify_snapshot_authority_blocking_failure(
                    operation,
                    error,
                    mutation_started,
                ))
            }
        }
    }

    /// Capture a full mux state snapshot from the given pane list.
    ///
    /// This is the core method. It takes a pre-fetched pane list to
    /// decouple the engine from `WeztermClient` (easier to test).
    pub async fn capture(
        &self,
        panes: &[PaneInfo],
        trigger: SnapshotTrigger,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.capture_with_cx(&cx, panes, trigger).await
    }

    /// Capture a full mux state snapshot with explicit per-call options.
    pub async fn capture_with_options(
        &self,
        panes: &[PaneInfo],
        trigger: SnapshotTrigger,
        options: SnapshotCaptureOptions,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.capture_with_options_with_cx(&cx, panes, trigger, options)
            .await
    }

    /// Capture a full mux state snapshot bound to the caller's asupersync
    /// capability context (ft-xbnl0.2.3 Cx-first entry point).
    ///
    /// Mirrors [`capture`](Self::capture) with two Cx-first changes:
    ///
    ///   * The internal `last_dedup_hash` RwLock uses `read_with_cx(cx)` /
    ///     `write_with_cx(cx)` so caller cancellation propagates through
    ///     the dedup cache lookups.
    ///
    ///   * First-session creation and the first checkpoint share one SQLite
    ///     transaction. The `session_id` lock keeps that durable commit and
    ///     subsequent in-memory publication in one ordered operation.
    ///
    /// Pre-flight: if `cx` is already cancelled on entry, the
    /// `captures_attempted` counter is still incremented (parity with
    /// the legacy observability surface), but the capture lifecycle is
    /// never claimed and the method returns `SnapshotError::Cancelled`
    /// without touching panes, topology, or storage.
    ///
    /// The legacy [`capture`](Self::capture) entry point is preserved
    /// for non-migrated callers; this is strictly additive.
    pub async fn capture_with_cx(
        &self,
        cx: &crate::cx::Cx,
        panes: &[PaneInfo],
        trigger: SnapshotTrigger,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        self.capture_with_options_with_cx(cx, panes, trigger, SnapshotCaptureOptions::default())
            .await
    }

    /// Capture a full mux state snapshot bound to the caller's Cx and explicit
    /// per-call options.
    pub async fn capture_with_options_with_cx(
        &self,
        cx: &crate::cx::Cx,
        panes: &[PaneInfo],
        trigger: SnapshotTrigger,
        options: SnapshotCaptureOptions,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        self.capture_with_options_and_shutdown_admission(cx, panes, trigger, options, None)
            .await
    }

    async fn capture_with_options_and_shutdown_admission(
        &self,
        cx: &crate::cx::Cx,
        panes: &[PaneInfo],
        trigger: SnapshotTrigger,
        options: SnapshotCaptureOptions,
        shutdown_reservation: Option<&CaptureShutdownReservation<'_>>,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        saturating_telemetry_add(&self.telemetry.captures_attempted, 1);

        if let Err(error) = snapshot_cx_checkpoint(cx) {
            saturating_telemetry_add(&self.telemetry.capture_errors, 1);
            return Err(error);
        }
        if self.authority_reconciliation_is_required() {
            saturating_telemetry_add(&self.telemetry.capture_errors, 1);
            return Err(
                self.authority_reconciliation_error(SnapshotAuthorityOperation::CheckpointCommit)
            );
        }
        if trigger == SnapshotTrigger::Shutdown && shutdown_reservation.is_none() {
            // A caller cannot smuggle a terminal trigger through ordinary
            // admission. Terminal captures must own the sticky shutdown fence.
            saturating_telemetry_add(&self.telemetry.capture_errors, 1);
            return Err(SnapshotError::ShuttingDown);
        }

        // 1. Atomically admit either an ordinary capture or the one final
        // capture owned by an exclusive shutdown reservation.
        let shutdown_capture = if let Some(reservation) = shutdown_reservation {
            if let Err(error) = reservation.begin_final_capture() {
                saturating_telemetry_add(&self.telemetry.capture_errors, 1);
                return Err(error);
            }
            true
        } else {
            match self.capture_lifecycle.compare_exchange(
                CAPTURE_LIFECYCLE_OPEN_IDLE,
                CAPTURE_LIFECYCLE_OPEN_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => false,
                Err(CAPTURE_LIFECYCLE_OPEN_ACTIVE) => {
                    saturating_telemetry_add(&self.telemetry.capture_errors, 1);
                    return Err(SnapshotError::InProgress);
                }
                Err(_) => {
                    saturating_telemetry_add(&self.telemetry.capture_errors, 1);
                    return Err(SnapshotError::ShuttingDown);
                }
            }
        };
        struct InProgressGuard<'a> {
            lifecycle: &'a AtomicU8,
            shutdown_capture: bool,
            capture_errors: &'a AtomicU64,
            completed_without_error: bool,
        }
        impl InProgressGuard<'_> {
            fn complete_without_error(&mut self) {
                self.completed_without_error = true;
            }
        }
        impl Drop for InProgressGuard<'_> {
            fn drop(&mut self) {
                if !self.completed_without_error {
                    saturating_telemetry_add(self.capture_errors, 1);
                }
                if self.shutdown_capture {
                    let _ = self.lifecycle.compare_exchange(
                        CAPTURE_LIFECYCLE_SHUTDOWN_ACTIVE,
                        CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return;
                }

                // The ordinary owner either reopens ordinary admission, or
                // hands the lane directly to the shutdown intent that fenced
                // later captures while this one was active.
                loop {
                    let current = self.lifecycle.load(Ordering::Acquire);
                    let next = match current {
                        CAPTURE_LIFECYCLE_OPEN_ACTIVE => CAPTURE_LIFECYCLE_OPEN_IDLE,
                        CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED => {
                            CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED
                        }
                        CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_RETRYABLE => {
                            CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE
                        }
                        _ => return,
                    };
                    if self
                        .lifecycle
                        .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return;
                    }
                }
            }
        }
        let mut capture_guard = InProgressGuard {
            lifecycle: &self.capture_lifecycle,
            shutdown_capture,
            capture_errors: &self.telemetry.capture_errors,
            completed_without_error: false,
        };

        if panes.is_empty() && trigger != SnapshotTrigger::Shutdown {
            return Err(SnapshotError::NoPanes);
        }
        if panes.len() > MAX_TOPOLOGY_PANES {
            return Err(SnapshotError::Topology(
                TopologySnapshotError::ResourceLimit {
                    resource: "panes",
                    count: panes.len(),
                    limit: MAX_TOPOLOGY_PANES,
                },
            ));
        }

        let mut options = options;
        let include_scrollback = options.include_scrollback;
        // Operator/event metadata is part of the persisted v2 witness but not
        // the topology/pane dedup digest. A metadata-bearing capture must
        // therefore reach durable storage even when its panes are unchanged.
        let periodic_dedup_allowed = options.metadata.is_none();
        let metadata = options.metadata.take();
        // Admit caller-supplied metadata before any auxiliary database read.
        // The iterative shape walk, bounded byte counter, and canonicalization
        // are CPU/allocation work, so keep them off the async worker while
        // moving (not cloning) the caller-owned Value into the blocking pool.
        let metadata_json = if let Some(metadata) = metadata {
            let metadata = IterativelyDroppedJsonValue::new(metadata);
            Some(
                crate::runtime_async::spawn_blocking_with_cx(cx, move || {
                    canonical_checkpoint_metadata(metadata.value())
                })
                .await
                .map_err(classify_snapshot_pure_blocking_failure)?
                .map_err(SnapshotError::from)?,
            )
        } else {
            None
        };

        let now_ms = epoch_ms();

        // 2. Load auxiliary persisted observations without blocking an async
        // worker on SQLite.
        #[cfg(test)]
        self.auxiliary_projection_read_attempts
            .fetch_add(1, Ordering::Relaxed);
        let pane_ids: Vec<u64> = panes.iter().map(|p| p.pane_id).collect();
        let detection_pane_ids = pane_ids.clone();
        let db_path_for_detections = Arc::clone(&self.db_path);
        let detection_max_age_ms =
            u64::try_from(STATE_DETECTION_MAX_AGE.as_millis()).unwrap_or(u64::MAX);
        let cutoff_ms: i64 =
            i64::try_from(now_ms.saturating_sub(detection_max_age_ms)).unwrap_or(i64::MAX);

        let detections_by_pane = match crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            load_latest_detections_by_pane_sync(
                db_path_for_detections.as_str(),
                &detection_pane_ids,
                cutoff_ms,
            )
        })
        .await
        .map_err(classify_snapshot_pure_blocking_failure)?
        {
            Ok(detections) => detections,
            Err(_) => {
                tracing::warn!(
                    error_class = "detection_projection_database",
                    "best-effort snapshot detection projection failed; continuing without detections"
                );
                HashMap::new()
            }
        };

        let scrollback_refs = if include_scrollback {
            let db_path_for_scrollback = Arc::clone(&self.db_path);
            let scrollback_pane_ids = pane_ids.clone();
            crate::runtime_async::spawn_blocking_with_cx(cx, move || {
                load_latest_scrollback_refs_sync(
                    db_path_for_scrollback.as_str(),
                    &scrollback_pane_ids,
                )
            })
            .await
            .map_err(classify_snapshot_pure_blocking_failure)?
            .map_err(SnapshotError::Database)?
        } else {
            std::collections::HashMap::new()
        };

        // 3. Copy only the bounded fields consumed by topology, pane-state, and
        // agent correlation. In particular, do not clone arbitrary backend
        // extras or let raw oversized cwd/title/workspace strings survive in a
        // second topology copy. The remaining JSON construction and hashing is
        // CPU/allocation heavy and runs in the bounded blocking pool.
        let pane_projection = project_snapshot_panes(panes)?;
        let prepared = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let SnapshotPaneProjection {
                panes: owned_panes,
                topology_workspace_id,
                truncated_pane_ids,
            } = pane_projection;
            let (mut topology, _report) = TopologySnapshot::from_panes(&owned_panes, now_ms);
            topology.workspace_id = topology_workspace_id;
            let mut correlator = AgentCorrelator::new();
            for (pane_id, detection) in detections_by_pane {
                correlator.ingest_detections(pane_id, std::slice::from_ref(&detection));
            }
            for pane in &owned_panes {
                correlator.update_from_pane_info(pane);
            }
            let pane_states: Vec<PaneStateSnapshot> = owned_panes
                .iter()
                .map(|pane| {
                    let mut snapshot = PaneStateSnapshot::from_pane_info(pane, now_ms, false);
                    if let Some(scrollback_ref) = scrollback_refs.get(&pane.pane_id) {
                        snapshot = snapshot.with_scrollback(scrollback_ref.clone());
                    }
                    if let Some(agent) = correlator.get_metadata(pane.pane_id) {
                        snapshot = snapshot.with_agent(agent);
                    }
                    snapshot
                })
                .collect();
            prepare_snapshot_persistence_with_canonical_metadata(
                &topology,
                &pane_states,
                metadata_json,
                &truncated_pane_ids,
            )
        })
        .await
        .map_err(classify_snapshot_pure_blocking_failure)?
        .map_err(SnapshotError::from)?;
        let dedup_hash = prepared.dedup_hash.clone();

        // 4. Skip if periodic-like and unchanged — Cx-bound read
        if periodic_dedup_allowed
            && matches!(
                trigger,
                SnapshotTrigger::Periodic | SnapshotTrigger::PeriodicFallback
            )
        {
            let cached_checkpoint = {
                let last = self
                    .last_dedup_hash
                    .read_with_cx(cx)
                    .await
                    .map_err(snapshot_lock_error)?;
                last.as_ref()
                    .filter(|cached| cached.dedup_hash.as_str() == dedup_hash.as_str())
                    .cloned()
            };
            if self.authority_reconciliation_is_required() {
                return Err(self
                    .authority_reconciliation_error(SnapshotAuthorityOperation::CheckpointCommit));
            }
            if let Some(cached) = cached_checkpoint {
                // A cache hit is only a hint. Another SnapshotEngine or process
                // can delete/prune its row, so every skip revalidates the exact
                // immutable commit receipt. Same-process mutations are held
                // behind this read admission until the skip decision settles.
                let authority_read = self.try_begin_snapshot_authority_read(
                    SnapshotAuthorityOperation::CheckpointCommit,
                )?;
                let db_path = Arc::clone(&self.db_path);
                let identity = cached.identity;
                let still_durable = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
                    exact_snapshot_checkpoint_exists_sync(db_path.as_str(), &identity)
                })
                .await
                .map_err(classify_snapshot_pure_blocking_failure)?
                .map_err(|error| SnapshotError::Database(error.to_string()))?;
                if still_durable {
                    saturating_telemetry_add(&self.telemetry.dedup_skips, 1);
                    capture_guard.complete_without_error();
                    return Err(SnapshotError::NoChanges);
                }
                drop(authority_read);
            }
        }

        // 5. Acquire the session publication guard before the durable write.
        // A first capture proposes an ID here but does not publish it in memory
        // or SQLite until the checkpoint transaction succeeds.
        let mut session_id_guard = self
            .session_id
            .write_with_cx(cx)
            .await
            .map_err(snapshot_lock_error)?;
        let creates_session = session_id_guard.is_none();
        let session_id = session_id_guard.clone().unwrap_or_else(generate_session_id);
        let new_session = creates_session.then(|| NewSessionMetadata {
            ft_version: crate::VERSION.to_string(),
            host_id: self
                .owner_identity
                .as_ref()
                .map(|identity| identity.host_id.clone()),
        });
        let owner_identity = self.owner_identity.clone();

        // 6. Acquire the final in-memory publication guard before the durable
        // write. Post-handoff cancellation is indeterminate, never an ordinary
        // failure; a typed success can therefore publish the dedup witness
        // immediately while both in-memory authority guards remain held.
        let mut last_dedup_hash = self
            .last_dedup_hash
            .write_with_cx(cx)
            .await
            .map_err(snapshot_lock_error)?;

        // 7. Persist checkpoint + pane states in a transaction.
        let checkpoint_type = trigger.as_db_str().to_string();
        let pane_count = prepared.pane_count;

        let db_path = Arc::clone(&self.db_path);

        let result = self
            .spawn_blocking_authority_with_cx(
                cx,
                SnapshotAuthorityOperation::CheckpointCommit,
                move || {
                    save_checkpoint_authoritatively_sync(
                        db_path.as_str(),
                        &session_id,
                        now_ms,
                        &checkpoint_type,
                        &prepared,
                        new_session.as_ref(),
                        owner_identity.as_ref(),
                    )
                },
            )
            .await?;

        // 8. Publish both in-memory authorities only after the typed durable
        // receipt. An indeterminate handoff leaves the sticky latch set and a
        // failed first transaction leaves `session_id` as `None`.
        if creates_session {
            *session_id_guard = Some(result.session_id.clone());
        }
        *last_dedup_hash = Some(LastDedupCheckpoint {
            dedup_hash,
            identity: SnapshotCheckpointIdentity {
                checkpoint_id: result.checkpoint_id,
                session_id: result.session_id.clone(),
                checkpoint_at: now_ms,
                checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
                state_hash: result.state_hash.clone(),
            },
        });

        // 9. Record success telemetry
        saturating_telemetry_add(&self.telemetry.captures_succeeded, 1);
        saturating_telemetry_add(
            &self.telemetry.panes_captured,
            u64::try_from(pane_count).unwrap_or(u64::MAX),
        );
        saturating_telemetry_add(
            &self.telemetry.bytes_persisted,
            u64::try_from(result.total_bytes).unwrap_or(u64::MAX),
        );
        saturating_telemetry_add(
            &self.telemetry.persisted_text_bytes,
            u64::try_from(result.persisted_text_bytes).unwrap_or(u64::MAX),
        );
        saturating_telemetry_add(
            &self.telemetry.pane_states_truncated,
            u64::try_from(result.truncated_pane_count).unwrap_or(u64::MAX),
        );
        if result.truncated_pane_count > 0 {
            tracing::warn!(
                checkpoint_id = result.checkpoint_id,
                truncated_pane_count = result.truncated_pane_count,
                pane_count,
                pane_text_limit = MAX_PERSISTED_PANE_TEXT_BYTES,
                "persisted snapshot after bounding oversized pane observations"
            );
        }
        capture_guard.complete_without_error();

        Ok(SnapshotResult {
            session_id: result.session_id,
            checkpoint_id: result.checkpoint_id,
            checkpoint_at: now_ms,
            state_hash: result.state_hash,
            pane_count,
            total_bytes: result.total_bytes,
            persisted_text_bytes: result.persisted_text_bytes,
            truncated_pane_count: result.truncated_pane_count,
            trigger,
        })
    }

    /// Run retention cleanup: remove old checkpoints exceeding limits.
    pub async fn cleanup(&self) -> std::result::Result<usize, SnapshotError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.cleanup_with_cx(&cx).await
    }

    /// Run retention cleanup bound to the caller's asupersync capability
    /// context (ft-xbnl0.2.x Cx-first entry point).
    ///
    /// A `cx.checkpoint()` before the blocking handoff lets a cancelled caller
    /// skip the DB work entirely. Cancellation or executor failure after
    /// admission is explicitly indeterminate: the engine latches reconciliation
    /// and suppresses every later durable mutation rather than claiming the
    /// cleanup is retry-safe.
    ///
    /// Pre-flight: if `cx` is already cancelled on entry, the
    /// `cleanup_runs` counter is still incremented (to preserve
    /// observability parity with `cleanup`), but the spawn_blocking is
    /// skipped and the method returns [`SnapshotError::Cancelled`] without
    /// touching the database.
    ///
    /// The legacy [`cleanup`](Self::cleanup) entry point is preserved
    /// for non-migrated callers; this is strictly additive.
    pub async fn cleanup_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> std::result::Result<usize, SnapshotError> {
        saturating_telemetry_add(&self.telemetry.cleanup_runs, 1);

        if cx.checkpoint().is_err() {
            tracing::debug!(
                "cleanup_with_cx: Cx cancelled before spawn_blocking; skipping cleanup"
            );
            return Err(SnapshotError::Cancelled);
        }

        let db_path = Arc::clone(&self.db_path);
        let retention_count = self.config.retention_count;
        let retention_days = self.config.retention_days;

        // Serialize cleanup against capture's durable-commit publication. A
        // checkpoint commit releases the database authority lane before it
        // publishes `last_dedup_hash`; taking this guard first prevents a
        // cleanup from deleting that checkpoint and then having the stale
        // digest republished after cleanup returns.
        let mut last_dedup_hash = self
            .last_dedup_hash
            .write_with_cx(cx)
            .await
            .map_err(snapshot_lock_error)?;

        let removed = self
            .spawn_blocking_authority_with_cx(
                cx,
                SnapshotAuthorityOperation::CheckpointCleanup,
                move || cleanup_authoritatively_sync(&db_path, retention_count, retention_days),
            )
            .await?;
        if removed != 0 {
            // The digest is an in-memory optimization, not durable authority,
            // and is not keyed by checkpoint identity. Conservatively clear
            // it after any actual pruning so an unchanged periodic capture
            // cannot be skipped after its only restorable row was removed.
            *last_dedup_hash = None;
        }
        saturating_telemetry_add(
            &self.telemetry.cleanup_removed,
            u64::try_from(removed).unwrap_or(u64::MAX),
        );
        self.record_checkpoint_cleanup_success(Instant::now());

        Ok(removed)
    }

    fn checkpoint_cleanup_interval(&self) -> Duration {
        checkpoint_cleanup_interval(self.config.interval_seconds)
    }

    fn record_checkpoint_cleanup_success(&self, completed_at: Instant) {
        let mut cadence = self
            .snapshot_authority
            .checkpoint_cleanup_cadence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cadence.last_authoritative_success = Some(completed_at);
        cadence.retry_deferred_at = None;
    }

    fn automatic_checkpoint_cleanup_wait(&self, now: Instant) -> Option<Duration> {
        if self.authority_reconciliation_is_required() {
            return None;
        }
        let cadence = self
            .snapshot_authority
            .checkpoint_cleanup_cadence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(checkpoint_cleanup_wait_duration(
            &cadence,
            self.checkpoint_cleanup_interval(),
            now,
        ))
    }

    fn try_begin_automatic_checkpoint_cleanup(
        &self,
        now: Instant,
    ) -> Option<CheckpointCleanupAttemptGuard> {
        if self.authority_reconciliation_is_required() {
            return None;
        }
        let mut cadence = self
            .snapshot_authority
            .checkpoint_cleanup_cadence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !checkpoint_cleanup_due(&cadence, self.checkpoint_cleanup_interval(), now) {
            return None;
        }
        cadence.in_progress = true;
        drop(cadence);

        let attempt = CheckpointCleanupAttemptGuard {
            authority: Arc::clone(&self.snapshot_authority),
            claimed_at: now,
        };
        if self.authority_reconciliation_is_required() {
            drop(attempt);
            return None;
        }
        Some(attempt)
    }

    /// Run automatic checkpoint pruning only when the shared database cadence
    /// is due. Retry-safe failures are logged and deferred; indeterminate
    /// outcomes retain the existing fail-closed scheduler contract.
    async fn maybe_run_checkpoint_cleanup_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> std::result::Result<(), SnapshotError> {
        if self.authority_reconciliation_is_required() {
            return Err(
                self.authority_reconciliation_error(SnapshotAuthorityOperation::CheckpointCleanup)
            );
        }
        let Some(_attempt) = self.try_begin_automatic_checkpoint_cleanup(Instant::now()) else {
            return Ok(());
        };

        match self.cleanup_with_cx(cx).await {
            Ok(removed) => {
                tracing::debug!(removed, "automatic snapshot retention cleanup completed");
                Ok(())
            }
            Err(error) if error.requires_reconciliation() => {
                tracing::warn!(
                    error_class = error.diagnostic_class(),
                    "automatic snapshot retention cleanup requires durable-state reconciliation"
                );
                Err(error)
            }
            Err(error) if error.is_retry_safe_scheduler_failure() => {
                tracing::warn!(
                    error_class = error.diagnostic_class(),
                    retry_delay_seconds = CHECKPOINT_CLEANUP_RETRY_DELAY.as_secs(),
                    "automatic snapshot retention cleanup failed; retry deferred"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Delete one checkpoint through the database-keyed durable-authority
    /// lane and reconcile the owning session's latest/clean summaries in the
    /// same SQLite transaction. Missing IDs are an acknowledged no-op.
    pub async fn delete_checkpoint(
        &self,
        target: SnapshotDeleteTarget,
    ) -> std::result::Result<Option<SnapshotDeleteResult>, SnapshotError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.delete_checkpoint_with_cx(&cx, target).await
    }

    /// Cx-first sibling of [`delete_checkpoint`](Self::delete_checkpoint).
    /// Cancellation after blocking-pool admission is indeterminate and latches
    /// all later mutations until durable state is reconciled.
    pub async fn delete_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        target: SnapshotDeleteTarget,
    ) -> std::result::Result<Option<SnapshotDeleteResult>, SnapshotError> {
        cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
        // See cleanup_with_cx: hold the publication guard across the durable
        // mutation so a concurrent capture cannot publish a digest for a row
        // that this deletion has already removed.
        let mut last_dedup_hash = self
            .last_dedup_hash
            .write_with_cx(cx)
            .await
            .map_err(snapshot_lock_error)?;
        let db_path = Arc::clone(&self.db_path);
        let deleted = self
            .spawn_blocking_authority_with_cx(
                cx,
                SnapshotAuthorityOperation::CheckpointDelete,
                move || delete_checkpoint_authoritatively_sync(&db_path, &target),
            )
            .await?;
        if deleted.is_some() {
            *last_dedup_hash = None;
        }
        Ok(deleted)
    }

    /// Configured value contribution for a trigger type.
    fn trigger_value(&self, trigger: SnapshotTrigger) -> f64 {
        let s = &self.config.scheduling;
        match trigger {
            SnapshotTrigger::WorkCompleted => s.work_completed_value,
            SnapshotTrigger::StateTransition => s.state_transition_value,
            SnapshotTrigger::IdleWindow => s.idle_window_value,
            SnapshotTrigger::MemoryPressure => s.memory_pressure_value,
            SnapshotTrigger::HazardThreshold => s.hazard_trigger_value,
            SnapshotTrigger::Event => s.work_completed_value,
            SnapshotTrigger::Periodic
            | SnapshotTrigger::PeriodicFallback
            | SnapshotTrigger::Manual
            | SnapshotTrigger::Shutdown
            | SnapshotTrigger::Startup => 0.0,
        }
    }

    /// Whether this trigger should bypass threshold accumulation and fire immediately.
    #[allow(clippy::unused_self)]
    fn is_immediate_trigger(&self, trigger: SnapshotTrigger) -> bool {
        matches!(
            trigger,
            SnapshotTrigger::HazardThreshold | SnapshotTrigger::MemoryPressure
        )
    }

    /// Attempt a capture via the pane provider, with standard logging.
    /// Returns `true` if a new checkpoint was persisted.
    ///
    /// Retained as a legacy ambient path alongside the cx-first sibling
    /// `capture_from_provider_with_cx` used by `run_periodic_with_cx`
    /// and the current periodic scheduler (ticks 111/112).
    #[allow(dead_code)]
    async fn capture_from_provider<F, Fut>(
        &self,
        pane_provider: &F,
        trigger: SnapshotTrigger,
    ) -> bool
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
    {
        match pane_provider().await {
            Ok(panes) => match self
                .capture_with_options(
                    &panes,
                    trigger,
                    SnapshotCaptureOptions {
                        include_scrollback: true,
                        metadata: None,
                    },
                )
                .await
            {
                Ok(result) => {
                    tracing::info!(
                        trigger = ?trigger,
                        pane_count = result.pane_count,
                        total_bytes = result.total_bytes,
                        checkpoint_id = result.checkpoint_id,
                        "snapshot captured"
                    );
                    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                    if let Err(error) = self.maybe_run_checkpoint_cleanup_with_cx(&cx).await {
                        tracing::warn!(
                            error_class = error.diagnostic_class(),
                            "automatic snapshot retention cleanup failed after capture"
                        );
                    }
                    true
                }
                Err(SnapshotError::NoChanges) => {
                    tracing::debug!(trigger = ?trigger, "snapshot skipped: no changes");
                    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                    if let Err(error) = self.maybe_run_checkpoint_cleanup_with_cx(&cx).await {
                        tracing::warn!(
                            error_class = error.diagnostic_class(),
                            "automatic snapshot retention cleanup failed after dedup"
                        );
                    }
                    false
                }
                Err(SnapshotError::InProgress) => {
                    tracing::debug!(trigger = ?trigger, "snapshot skipped: capture in progress");
                    false
                }
                Err(SnapshotError::AuthorityMutationInProgress { operation }) => {
                    tracing::debug!(
                        trigger = ?trigger,
                        %operation,
                        "snapshot skipped: durable authority mutation in progress"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(
                        trigger = ?trigger,
                        error_class = e.diagnostic_class(),
                        "snapshot capture failed"
                    );
                    false
                }
            },
            Err(error) => {
                tracing::warn!(
                    trigger = ?trigger,
                    error_class = error.diagnostic_class(),
                    "snapshot pane listing failed"
                );
                false
            }
        }
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`capture_from_provider`].
    ///
    /// Routes both capture and cadence-bounded checkpoint cleanup through
    /// their cx-first siblings so the scheduler's main work loop honours
    /// caller cancellation across every write seam.
    /// The pane provider is awaited directly — it's a caller-supplied
    /// future that doesn't inherently know about cx; if a caller
    /// needs cx threaded there they can capture one in the closure.
    async fn capture_from_provider_with_cx<F, Fut>(
        &self,
        cx: &crate::cx::Cx,
        pane_provider: &F,
        trigger: SnapshotTrigger,
    ) -> std::result::Result<SchedulerCaptureOutcome, SnapshotError>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
    {
        if self.authority_reconciliation_is_required() {
            return Err(
                self.authority_reconciliation_error(SnapshotAuthorityOperation::CheckpointCommit)
            );
        }
        match self.capture_lifecycle.load(Ordering::Acquire) {
            CAPTURE_LIFECYCLE_OPEN_IDLE => {}
            CAPTURE_LIFECYCLE_OPEN_ACTIVE => {
                tracing::debug!(
                    trigger = ?trigger,
                    "snapshot deferred before pane discovery: capture in progress"
                );
                return Ok(SchedulerCaptureOutcome::Deferred(
                    SchedulerCaptureDeferredReason::Busy,
                ));
            }
            CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED
            | CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_RETRYABLE
            | CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED
            | CAPTURE_LIFECYCLE_SHUTDOWN_ACTIVE
            | CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE
            | CAPTURE_LIFECYCLE_SHUTDOWN_COMPLETE => return Err(SnapshotError::ShuttingDown),
            _ => return Err(SnapshotError::ContextFailure),
        }
        if self.snapshot_authority.in_progress.load(Ordering::Acquire) {
            tracing::debug!(
                trigger = ?trigger,
                "snapshot deferred before pane discovery: durable authority mutation in progress"
            );
            return Ok(SchedulerCaptureOutcome::Deferred(
                SchedulerCaptureDeferredReason::Busy,
            ));
        }

        match pane_provider().await {
            Ok(panes) => match self
                .capture_with_options_with_cx(
                    cx,
                    &panes,
                    trigger,
                    SnapshotCaptureOptions {
                        include_scrollback: true,
                        metadata: None,
                    },
                )
                .await
            {
                Ok(result) => {
                    tracing::info!(
                        trigger = ?trigger,
                        pane_count = result.pane_count,
                        total_bytes = result.total_bytes,
                        checkpoint_id = result.checkpoint_id,
                        "snapshot captured (cx path)"
                    );
                    self.maybe_run_checkpoint_cleanup_with_cx(cx).await?;
                    Ok(SchedulerCaptureOutcome::Captured)
                }
                Err(SnapshotError::NoChanges) => {
                    tracing::debug!(trigger = ?trigger, "snapshot skipped: no changes");
                    self.maybe_run_checkpoint_cleanup_with_cx(cx).await?;
                    Ok(SchedulerCaptureOutcome::Unchanged)
                }
                Err(SnapshotError::InProgress) => {
                    tracing::debug!(trigger = ?trigger, "snapshot skipped: capture in progress");
                    Ok(SchedulerCaptureOutcome::Deferred(
                        SchedulerCaptureDeferredReason::Busy,
                    ))
                }
                Err(SnapshotError::NoPanes) => {
                    tracing::debug!(trigger = ?trigger, "snapshot deferred: no panes available");
                    Ok(SchedulerCaptureOutcome::Deferred(
                        SchedulerCaptureDeferredReason::NoPanes,
                    ))
                }
                Err(SnapshotError::AuthorityMutationInProgress { operation }) => {
                    tracing::debug!(
                        trigger = ?trigger,
                        %operation,
                        "snapshot skipped: durable authority mutation in progress"
                    );
                    Ok(SchedulerCaptureOutcome::Deferred(
                        SchedulerCaptureDeferredReason::Busy,
                    ))
                }
                Err(error) if error.is_capacity_admission_failure() => {
                    tracing::warn!(
                        trigger = ?trigger,
                        error_class = error.diagnostic_class(),
                        "snapshot projection exceeded deterministic admission; scheduler will retain demand with bounded backoff"
                    );
                    Ok(SchedulerCaptureOutcome::Deferred(
                        SchedulerCaptureDeferredReason::CapacityAdmission,
                    ))
                }
                Err(error) if error.is_retry_safe_scheduler_failure() => {
                    tracing::warn!(
                        trigger = ?trigger,
                        error_class = error.diagnostic_class(),
                        "snapshot capture failed retry-safely; scheduler will retain demand with bounded backoff"
                    );
                    Ok(SchedulerCaptureOutcome::Deferred(
                        SchedulerCaptureDeferredReason::RetrySafeFailure,
                    ))
                }
                Err(e) => {
                    tracing::warn!(
                        trigger = ?trigger,
                        error_class = e.diagnostic_class(),
                        "snapshot capture failed"
                    );
                    Err(e)
                }
            },
            Err(error) if error.is_retry_safe_scheduler_failure() => {
                tracing::warn!(
                    trigger = ?trigger,
                    error_class = error.diagnostic_class(),
                    "snapshot pane provider failed retry-safely; scheduler will use bounded backoff"
                );
                Ok(SchedulerCaptureOutcome::Deferred(
                    SchedulerCaptureDeferredReason::RetrySafeFailure,
                ))
            }
            Err(error) => Err(error),
        }
    }

    /// Run the snapshot scheduling loop.
    ///
    /// In `Periodic` mode: captures at fixed intervals.
    /// In `Intelligent` mode: accumulates trigger values and captures when
    /// the threshold is reached, with a periodic fallback for liveness.
    ///
    /// `pane_provider` is called each time to fetch the current pane list.
    /// This decouples the engine from `WeztermClient` for testability.
    ///
    /// # Errors
    ///
    /// Returns a typed snapshot error when capture, synchronization, or caller
    /// cancellation prevents the scheduler from completing normally.
    pub async fn run_periodic<F, Fut>(
        &self,
        shutdown: watch::Receiver<bool>,
        pane_provider: F,
    ) -> std::result::Result<(), SnapshotError>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
    {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_periodic_with_cx(&cx, shutdown, pane_provider)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run_periodic`].
    ///
    /// Tick 112 upgrade: threads the caller's cx through the
    /// scheduler body so shutdown-watcher polls
    /// (`shutdown.changed(&cx)`) and intelligent-scheduler
    /// trigger polls (`trigger_rx.recv(&cx)`) honor caller
    /// cancellation. A cancelled caller cx cuts BOTH poll sites
    /// at their next waker boundary, replacing tick 106's
    /// pre-flight-only gating.
    ///
    /// Both entry points now share a single
    /// `scheduler_body(&cx, ...)` helper (via internal
    /// refactor); the legacy path passes `cx::for_request()` to
    /// preserve its prior semantics.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Cancelled`] when `cx` is cancelled, or
    /// propagates capture/synchronization failures from scheduler work.
    pub async fn run_periodic_with_cx<F, Fut>(
        &self,
        cx: &crate::cx::Cx,
        shutdown: watch::Receiver<bool>,
        pane_provider: F,
    ) -> std::result::Result<(), SnapshotError>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
    {
        if cx.checkpoint().is_err() {
            tracing::info!("snapshot engine run_periodic cancelled at entry");
            return Err(SnapshotError::Cancelled);
        }
        self.scheduler_body(cx, shutdown, pane_provider).await
    }

    /// Cx-aware scheduler body shared by `run_periodic` and
    /// `run_periodic_with_cx`. The caller's cx is threaded into
    /// both `shutdown.changed(&cx)` and `trigger_rx.recv(&cx)`
    /// so cancellation propagates into both poll sites.
    async fn scheduler_body<F, Fut>(
        &self,
        cx: &crate::cx::Cx,
        mut shutdown: watch::Receiver<bool>,
        pane_provider: F,
    ) -> std::result::Result<(), SnapshotError>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
    {
        if self
            .scheduler_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SnapshotError::SchedulerInProgress);
        }
        struct SchedulerGuard<'a>(&'a AtomicBool);
        impl Drop for SchedulerGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _scheduler_guard = SchedulerGuard(&self.scheduler_in_progress);

        // ft-0yuxe: drive `[snapshots.session_retention]` cleanup from the live
        // snapshot scheduler. Normal cadence advances only after an
        // authoritative success; retry-safe failures and admission contention
        // use a bounded retry delay shared by both scheduling modes.
        let mut session_cleanup_schedule = SessionCleanupSchedule::default();
        match self.config.scheduling.mode {
            SnapshotSchedulingMode::Periodic => {
                let interval_secs = self.config.interval_seconds.max(30);
                let interval = Duration::from_secs(interval_secs);
                let mut last_snapshot_completion = None;
                let mut snapshot_retry_not_before = None;
                let mut snapshot_retry_state = SchedulerCaptureRetryState::default();

                loop {
                    let now = Instant::now();
                    let mut next_wait = periodic_scheduler_wait_duration(
                        &session_cleanup_schedule,
                        self.config.session_retention.cleanup_interval_hours,
                        self.authority_reconciliation_is_required(),
                        last_snapshot_completion,
                        snapshot_retry_not_before,
                        interval,
                        now,
                    );
                    if let Some(checkpoint_cleanup_wait) =
                        self.automatic_checkpoint_cleanup_wait(now)
                    {
                        next_wait = next_wait.min(checkpoint_cleanup_wait);
                    }

                    if !next_wait.is_zero() {
                        // ft-xbnl0.2.3 tick 296: cx-first timeout wrapping
                        // shutdown.changed(cx) — both the outer interval-timeout
                        // AND the inner shutdown wait now honor cx-cancel.
                        let shutdown_fut = shutdown.changed(cx);
                        let shutdown_wait =
                            crate::runtime_async::timeout_with_cx(cx, next_wait, shutdown_fut)
                                .await;
                        if cx.is_cancel_requested() {
                            return Err(SnapshotError::Cancelled);
                        }
                        if shutdown_wait.is_ok() {
                            tracing::info!("snapshot engine shutting down");
                            break;
                        }
                        // The independent cleanup and snapshot deadlines can
                        // wake this loop for different reasons. Recompute both
                        // after every timer wake rather than coupling cleanup
                        // cadence to a snapshot capture.
                        continue;
                    }

                    if cx.checkpoint().is_err() {
                        tracing::info!("snapshot engine run_periodic: cx cancelled, exiting");
                        return Err(SnapshotError::Cancelled);
                    }
                    if *shutdown.borrow() {
                        tracing::info!("snapshot engine shutting down before cleanup");
                        break;
                    }

                    self.maybe_run_checkpoint_cleanup_with_cx(cx).await?;
                    snapshot_cx_checkpoint(cx)?;
                    self.maybe_run_session_cleanup(cx, &mut session_cleanup_schedule)
                        .await;
                    snapshot_cx_checkpoint(cx)?;
                    if *shutdown.borrow() {
                        tracing::info!("snapshot engine shutting down after cleanup");
                        break;
                    }

                    let now = Instant::now();
                    let snapshot_is_due = last_snapshot_completion
                        .is_none_or(|last| now.saturating_duration_since(last) >= interval)
                        && snapshot_retry_not_before.is_none_or(|retry_at| now >= retry_at);
                    if !snapshot_is_due {
                        // Cleanup-only wake: do not manufacture an unrelated
                        // pane snapshot or move the snapshot cadence.
                        continue;
                    }

                    let trigger = if last_snapshot_completion.is_none() {
                        SnapshotTrigger::Startup
                    } else {
                        SnapshotTrigger::Periodic
                    };
                    let outcome = self
                        .capture_from_provider_with_cx(cx, &pane_provider, trigger)
                        .await?;
                    cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
                    let completed_at = Instant::now();
                    if outcome.settled() {
                        snapshot_retry_state.record_settled();
                        last_snapshot_completion = Some(completed_at);
                        snapshot_retry_not_before = None;
                    } else if let SchedulerCaptureOutcome::Deferred(reason) = outcome {
                        snapshot_retry_not_before = Some(snapshot_retry_state.retry_deadline(
                            completed_at,
                            trigger,
                            reason,
                        ));
                    }
                }
            }
            SnapshotSchedulingMode::Intelligent => {
                if *shutdown.borrow() {
                    tracing::info!("snapshot engine shutting down before startup cleanup");
                    return Ok(());
                }

                let mut trigger_rx = SnapshotTriggerReceiverLease::take(&self.trigger_rx)
                    .ok_or_else(|| {
                        tracing::error!(
                            "snapshot intelligent scheduler receiver is unavailable despite exclusive scheduler admission"
                        );
                        SnapshotError::TriggerReceiverUnavailable
                    })?;

                // Both retention lanes have independent startup contracts.
                // Attempt them before pane capture so a transient
                // capture/provider failure cannot suppress cleanup for the
                // entire scheduler invocation.
                self.maybe_run_checkpoint_cleanup_with_cx(cx).await?;
                snapshot_cx_checkpoint(cx)?;
                self.maybe_run_session_cleanup(cx, &mut session_cleanup_schedule)
                    .await;
                snapshot_cx_checkpoint(cx)?;
                if *shutdown.borrow() {
                    tracing::info!("snapshot engine shutting down after startup cleanup");
                    return Ok(());
                }

                let startup_outcome = self
                    .capture_from_provider_with_cx(cx, &pane_provider, SnapshotTrigger::Startup)
                    .await?;
                cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;

                let fallback_secs = self
                    .config
                    .scheduling
                    .periodic_fallback_minutes
                    .max(1)
                    .saturating_mul(60);
                let fallback_interval = Duration::from_secs(fallback_secs);
                let next_periodic_fallback_at = || {
                    let deadline = Instant::now().checked_add(fallback_interval);
                    if deadline.is_none() {
                        tracing::warn!(
                            ?fallback_interval,
                            "periodic snapshot fallback interval is too large to schedule"
                        );
                    }
                    deadline
                };
                let mut next_fallback_at = next_periodic_fallback_at();

                let mut accumulated_value = 0.0_f64;
                let snapshot_threshold = self.config.scheduling.snapshot_threshold.max(0.0);
                let mut snapshot_retry_state = SchedulerCaptureRetryState::default();
                let (mut pending_trigger, mut capture_retry_at) = match startup_outcome {
                    SchedulerCaptureOutcome::Deferred(reason) => {
                        let retry_at = snapshot_retry_state.retry_deadline(
                            Instant::now(),
                            SnapshotTrigger::Startup,
                            reason,
                        );
                        (Some(SnapshotTrigger::Startup), Some(retry_at))
                    }
                    SchedulerCaptureOutcome::Captured | SchedulerCaptureOutcome::Unchanged => {
                        snapshot_retry_state.record_settled();
                        (None, None)
                    }
                };

                enum TriggerPoll {
                    Ready(SnapshotTrigger),
                    Closed,
                    TimedOut,
                }

                loop {
                    if cx.checkpoint().is_err() {
                        tracing::info!(
                            "snapshot engine intelligent scheduler: cx cancelled, exiting"
                        );
                        return Err(SnapshotError::Cancelled);
                    }
                    if *shutdown.borrow() {
                        tracing::info!("snapshot engine shutting down before cleanup");
                        break;
                    }

                    self.maybe_run_checkpoint_cleanup_with_cx(cx).await?;
                    snapshot_cx_checkpoint(cx)?;
                    self.maybe_run_session_cleanup(cx, &mut session_cleanup_schedule)
                        .await;
                    snapshot_cx_checkpoint(cx)?;
                    if *shutdown.borrow() {
                        tracing::info!("snapshot engine shutting down after cleanup");
                        break;
                    }

                    // Retry deadlines take precedence over trigger ingress.
                    // A continuously ready bounded channel must not starve an
                    // already-admitted capture demand indefinitely.
                    if let Some(trigger) = due_intelligent_scheduler_retry(
                        pending_trigger,
                        capture_retry_at,
                        Instant::now(),
                    ) {
                        let outcome = self
                            .capture_from_provider_with_cx(cx, &pane_provider, trigger)
                            .await?;
                        cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
                        if outcome.settled() {
                            snapshot_retry_state.record_settled();
                        }
                        match outcome {
                            SchedulerCaptureOutcome::Captured => {
                                accumulated_value = 0.0;
                                pending_trigger = None;
                                capture_retry_at = None;
                                if trigger == SnapshotTrigger::PeriodicFallback
                                    || next_fallback_at
                                        .is_some_and(|deadline| Instant::now() >= deadline)
                                {
                                    next_fallback_at = next_periodic_fallback_at();
                                }
                            }
                            SchedulerCaptureOutcome::Unchanged => {
                                if self.is_immediate_trigger(trigger) || snapshot_threshold <= 0.0 {
                                    accumulated_value = 0.0;
                                }
                                pending_trigger = None;
                                capture_retry_at = None;
                                if trigger == SnapshotTrigger::PeriodicFallback
                                    || next_fallback_at
                                        .is_some_and(|deadline| Instant::now() >= deadline)
                                {
                                    next_fallback_at = next_periodic_fallback_at();
                                }
                            }
                            SchedulerCaptureOutcome::Deferred(reason) => {
                                capture_retry_at = Some(snapshot_retry_state.retry_deadline(
                                    Instant::now(),
                                    trigger,
                                    reason,
                                ));
                            }
                        }
                        continue;
                    }

                    // ft-83kc7: read the shutdown FLAG, do not wait for a change
                    // event with a zero-duration timeout.
                    //
                    // This was `timeout_with_cx(cx, Duration::ZERO,
                    // shutdown.changed(cx))`, intended as a non-blocking poll.
                    // A zero-duration timeout cannot do that: the deadline is
                    // the instant the future is built, so by the time it is
                    // first polled the ambient clock has usually passed it, and
                    // `TimeoutFuture` returns `Elapsed` *without polling the
                    // inner future at all* (it only prefers ready inner work at
                    // `now == deadline`). The shutdown signal was therefore
                    // never observed, this loop never exited, and every test
                    // that spawns the intelligent scheduler and awaits its
                    // handle hung forever — all ten of them. The same fact is
                    // already recorded for the search bridge under br-ft-qfklb:
                    // a `Duration::ZERO` timeout "fires immediately, before any
                    // work".
                    //
                    // The flag is what this check actually wants: `changed()`
                    // reports an edge, and an edge can be missed, whereas the
                    // value is monotonic for a shutdown latch. Shutdown latency
                    // stays bounded by the trigger wait-step below (<= 250 ms),
                    // exactly as the previous code intended.
                    let fallback_wait = next_fallback_at
                        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::from_millis(250));
                    let capture_retry_wait = capture_retry_at
                        .map(|deadline| deadline.saturating_duration_since(Instant::now()));
                    // A pending capture subsumes an expired fallback. Waiting
                    // on the fallback as well would yield a zero-duration
                    // hot loop until the bounded retry deadline. The settled
                    // retry advances an already-expired fallback below.
                    let poll_wait =
                        intelligent_scheduler_poll_wait(fallback_wait, capture_retry_wait);

                    let trigger_poll = if poll_wait.is_zero() {
                        TriggerPoll::TimedOut
                    } else {
                        let wait_step = poll_wait.min(Duration::from_millis(250));
                        let recv_fut = trigger_rx.receiver_mut()?.recv(cx);
                        let recv_result =
                            crate::runtime_async::timeout_with_cx(cx, wait_step, recv_fut).await;

                        if cx.is_cancel_requested() {
                            return Err(SnapshotError::Cancelled);
                        }

                        match recv_result {
                            Ok(Ok(trigger)) => TriggerPoll::Ready(trigger),
                            Ok(Err(_)) => TriggerPoll::Closed,
                            Err(_) => TriggerPoll::TimedOut,
                        }
                    };

                    match trigger_poll {
                        TriggerPoll::Ready(trigger) => {
                            let tv = self.trigger_value(trigger);
                            if tv > 0.0 {
                                accumulated_value += tv;
                            }

                            let immediate = self.is_immediate_trigger(trigger);
                            let should_capture = immediate
                                || snapshot_threshold <= 0.0
                                || accumulated_value >= snapshot_threshold;

                            if let Some(pending) = pending_trigger {
                                // Coalesce state-capture demand while an
                                // earlier attempt is provably deferred. Keep
                                // the stronger immediate trigger identity for
                                // the retry receipt, but never shorten an
                                // already-published contention/provider/DB
                                // backoff merely because a higher-priority
                                // event arrived.
                                if should_upgrade_pending_scheduler_trigger(
                                    pending,
                                    trigger,
                                    should_capture,
                                ) {
                                    pending_trigger = Some(trigger);
                                    let upgraded_retry = snapshot_retry_state.retry_deadline(
                                        Instant::now(),
                                        trigger,
                                        SchedulerCaptureDeferredReason::Busy,
                                    );
                                    capture_retry_at =
                                        Some(capture_retry_at.map_or(upgraded_retry, |current| {
                                            current.max(upgraded_retry)
                                        }));
                                }
                            } else if should_capture {
                                let outcome = self
                                    .capture_from_provider_with_cx(cx, &pane_provider, trigger)
                                    .await?;
                                cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
                                if outcome.settled() {
                                    snapshot_retry_state.record_settled();
                                }
                                match outcome {
                                    SchedulerCaptureOutcome::Captured => {
                                        accumulated_value = 0.0;
                                    }
                                    SchedulerCaptureOutcome::Unchanged
                                        if immediate || snapshot_threshold <= 0.0 =>
                                    {
                                        accumulated_value = 0.0;
                                    }
                                    SchedulerCaptureOutcome::Unchanged => {}
                                    SchedulerCaptureOutcome::Deferred(reason) => {
                                        pending_trigger = Some(trigger);
                                        capture_retry_at =
                                            Some(snapshot_retry_state.retry_deadline(
                                                Instant::now(),
                                                trigger,
                                                reason,
                                            ));
                                    }
                                }
                            }
                        }
                        TriggerPoll::Closed => {
                            tracing::info!(
                                "trigger channel closed; intelligent scheduler stopping"
                            );
                            break;
                        }
                        TriggerPoll::TimedOut => {
                            let now = Instant::now();
                            let Some(fallback_at) = next_fallback_at else {
                                continue;
                            };
                            if now < fallback_at || pending_trigger.is_some() {
                                continue;
                            }
                            let outcome = self
                                .capture_from_provider_with_cx(
                                    cx,
                                    &pane_provider,
                                    SnapshotTrigger::PeriodicFallback,
                                )
                                .await?;
                            cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
                            if outcome.settled() {
                                snapshot_retry_state.record_settled();
                            }
                            match outcome {
                                SchedulerCaptureOutcome::Captured => {
                                    accumulated_value = 0.0;
                                    next_fallback_at = next_periodic_fallback_at();
                                }
                                SchedulerCaptureOutcome::Unchanged => {
                                    next_fallback_at = next_periodic_fallback_at();
                                }
                                SchedulerCaptureOutcome::Deferred(reason) => {
                                    pending_trigger = Some(SnapshotTrigger::PeriodicFallback);
                                    capture_retry_at = Some(snapshot_retry_state.retry_deadline(
                                        Instant::now(),
                                        SnapshotTrigger::PeriodicFallback,
                                        reason,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Run `[snapshots.session_retention]` cleanup on the configured cadence
    /// (ft-0yuxe).
    ///
    /// Before this wiring the cleanup engine
    /// (`session_retention::cleanup_sessions`) had no production caller, so the
    /// whole `[snapshots.session_retention]` block — and the orphan/data-loss
    /// fixes inside it — were inert: closed sessions, checkpoints, and pane
    /// state grew unbounded.
    ///
    /// `cleanup_interval_hours == 0` means "one authoritative startup
    /// completion": admission contention or a typed retry-safe failure is
    /// retried after [`SESSION_CLEANUP_RETRY_DELAY`], while the first
    /// authoritative completed receipt ends automatic cleanup for this scheduler
    /// invocation. A positive value reruns every N hours after authoritative
    /// success. The DB connection is opened fresh inside the cleanup engine (a
    /// blocking SQLite pipeline run on the blocking pool). If its authoritative
    /// completion is lost, the
    /// engine latches both `session_cleanup_reconciliation_required` and the
    /// shared snapshot-authority reconciliation state. This suppresses every
    /// later mutation of the same authority tables for this engine instance,
    /// including after a same-engine scheduler restart.
    async fn maybe_run_session_cleanup(
        &self,
        cx: &crate::cx::Cx,
        schedule: &mut SessionCleanupSchedule,
    ) {
        if self.authority_reconciliation_is_required() {
            return;
        }
        let interval_hours = self.config.session_retention.cleanup_interval_hours;
        if !session_cleanup_due(schedule, interval_hours, Instant::now()) {
            return;
        }

        let Some(cleanup_attempt) = self.try_begin_session_cleanup() else {
            if !self.authority_reconciliation_is_required() {
                schedule.defer_retry(Instant::now());
                tracing::debug!(
                    retry_delay_seconds = SESSION_CLEANUP_RETRY_DELAY.as_secs(),
                    "Session retention cleanup admission is busy; retry deferred"
                );
            }
            return;
        };

        let db_path = Arc::clone(&self.db_path);
        let config = self.config.session_retention.clone();
        match self
            .spawn_blocking_authority_with_cx(
                cx,
                SnapshotAuthorityOperation::SessionRetentionCleanup,
                move || {
                    crate::session_retention::cleanup_sessions_from_path(db_path.as_str(), &config)
                },
            )
            .await
        {
            Ok(result) => {
                let total = result.total_sessions_deleted();
                if result.recovery_reconciliation_pending {
                    schedule.defer_retry(Instant::now());
                    tracing::debug!(
                        sessions_deleted = total,
                        retry_delay_seconds = SESSION_CLEANUP_RETRY_DELAY.as_secs(),
                        interval_hours,
                        "Session retention recovery reconciliation made bounded progress; retry deferred"
                    );
                } else if result.size_ineligible_shortfall_bytes > 0 {
                    schedule.record_authoritative_success(Instant::now());
                    tracing::warn!(
                        sessions_deleted = total,
                        size_measured_bytes = result.size_measured_bytes,
                        size_deleted_bytes = result.size_deleted_bytes,
                        size_retained_bytes = result.size_retained_bytes,
                        size_ineligible_shortfall_bytes = result.size_ineligible_shortfall_bytes,
                        interval_hours,
                        "Session retention cleanup completed above its size budget because no more sessions were eligible"
                    );
                } else if result.any_work_done() {
                    schedule.record_authoritative_success(Instant::now());
                    tracing::info!(
                        sessions_deleted = total,
                        orphaned_restore_lifecycle_rows = result.orphaned_restore_lifecycle_rows,
                        orphaned_checkpoints = result.orphaned_checkpoints,
                        orphaned_pane_states = result.orphaned_pane_states,
                        size_measured_bytes = result.size_measured_bytes,
                        size_deleted_bytes = result.size_deleted_bytes,
                        size_retained_bytes = result.size_retained_bytes,
                        size_ineligible_shortfall_bytes = result.size_ineligible_shortfall_bytes,
                        explicit_vacuum_attempted = false,
                        expected_default_free_space_policy = "auto_vacuum_none_freelist_reuse",
                        interval_hours,
                        "Session retention cleanup completed"
                    );
                } else {
                    schedule.record_authoritative_success(Instant::now());
                    tracing::debug!(
                        size_measured_bytes = result.size_measured_bytes,
                        size_deleted_bytes = result.size_deleted_bytes,
                        size_retained_bytes = result.size_retained_bytes,
                        size_ineligible_shortfall_bytes = result.size_ineligible_shortfall_bytes,
                        interval_hours,
                        "Session retention cleanup: nothing to remove"
                    );
                }
            }
            Err(error) => {
                let reconciliation_latched = self.authority_reconciliation_is_required();
                if error.requires_reconciliation() || reconciliation_latched {
                    tracing::warn!(
                        error_class = error.diagnostic_class(),
                        reconciliation_latched,
                        automatic_retry_suppressed = true,
                        "Session retention cleanup outcome is indeterminate; reconcile durable state before restarting cleanup"
                    );
                } else {
                    schedule.defer_retry(Instant::now());
                    tracing::warn!(
                        error_class = error.diagnostic_class(),
                        retry_delay_seconds = SESSION_CLEANUP_RETRY_DELAY.as_secs(),
                        "Session retention cleanup failed; retry deferred"
                    );
                }
            }
        }
        drop(cleanup_attempt);
    }

    /// Acquire exclusive authority for an automatic cleanup attempt. A second
    /// scheduler sharing this engine cannot enter cleanup concurrently.
    fn try_begin_session_cleanup(&self) -> Option<SessionCleanupAttemptGuard<'_>> {
        if self.authority_reconciliation_is_required() {
            return None;
        }
        self.session_cleanup_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;

        // Recheck after scheduler-local admission so a concurrently published
        // shared-authority latch suppresses cleanup before blocking handoff.
        if self.authority_reconciliation_is_required() {
            self.session_cleanup_in_progress
                .store(false, Ordering::Release);
            return None;
        }

        Some(SessionCleanupAttemptGuard {
            in_progress: &self.session_cleanup_in_progress,
        })
    }

    /// Monotonically latch cleanup reconciliation after any outcome whose
    /// durable effects are indeterminate. Retry-safe failures must never clear
    /// a latch set by an earlier observation loss.
    #[cfg(test)]
    fn latch_session_cleanup_reconciliation(
        &self,
        error: crate::session_retention::SessionCleanupError,
    ) -> bool {
        if error.requires_reconciliation() {
            self.snapshot_authority
                .latch_reconciliation(SnapshotAuthorityOperation::SessionRetentionCleanup);
        }
        self.snapshot_authority.reconciliation_is_required()
    }

    /// Capture a final shutdown checkpoint and mark the session as cleanly shut
    /// down. Shutdown captures deliberately bypass ordinary periodic dedup so
    /// success always includes a durable final-checkpoint receipt.
    pub async fn shutdown_checkpoint(
        &self,
        panes: &[PaneInfo],
        timeout: Duration,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.shutdown_checkpoint_with_cx(&cx, panes, timeout).await
    }

    /// Capture a final shutdown checkpoint, bound to the caller's asupersync
    /// capability context (ft-xbnl0.2.x Cx-first entry point).
    ///
    /// Identical semantics to [`shutdown_checkpoint`](Self::shutdown_checkpoint)
    /// with the inner timeout rebound via
    /// [`crate::runtime_async::timeout_with_cx`] so outer-scope cancellation
    /// (operator abort, deadline collapse) cuts the capture race
    /// deterministically under `LabRuntime` virtual time rather than only
    /// responding to the explicit `timeout` argument.
    ///
    /// Pre-flight: if `cx` is already cancelled, skip both mutations and return
    /// [`SnapshotError::Cancelled`]. A session is marked clean only after the
    /// final checkpoint and the mark itself both publish typed receipts inside
    /// the same timeout.
    ///
    /// A reconciliation latch always takes precedence over a successful mark,
    /// ordinary cancellation, or timeout. Those benign outcomes must never hide
    /// a blocking mutation whose authoritative result was lost.
    ///
    /// The legacy [`shutdown_checkpoint`](Self::shutdown_checkpoint) entry
    /// point is preserved for non-migrated callers; this is strictly
    /// additive.
    pub async fn shutdown_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        panes: &[PaneInfo],
        timeout: Duration,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        if self.authority_reconciliation_is_required() {
            return Err(
                self.authority_reconciliation_error(SnapshotAuthorityOperation::CheckpointCommit)
            );
        }
        if let Err(error) = snapshot_cx_checkpoint(cx) {
            tracing::debug!(
                "shutdown_checkpoint_with_cx: Cx pre-cancelled; skipping checkpoint and clean mark"
            );
            return Err(error);
        }
        let mut checkpoint_receipt: Option<SnapshotResult> = None;
        let result = crate::runtime_async::timeout_with_cx(cx, timeout, async {
            let reservation = self.reserve_capture_lifecycle(cx).await?;
            // Use the Cx-first capture variant so the inner RwLock
            // acquires (last_dedup_hash + session_id) bind to
            // the caller's Cx rather than an ambient one.
            let checkpoint = self
                .capture_with_options_and_shutdown_admission(
                    cx,
                    panes,
                    SnapshotTrigger::Shutdown,
                    SnapshotCaptureOptions {
                        include_scrollback: true,
                        metadata: None,
                    },
                    Some(&reservation),
                )
                .await?;
            checkpoint_receipt = Some(checkpoint.clone());

            if let Err(source) = self
                .mark_shutdown_with_reservation(cx, &reservation, &checkpoint)
                .await
            {
                return Err(SnapshotError::ShutdownMarkFailed {
                    checkpoint: Box::new(checkpoint),
                    source: Box::new(source),
                });
            }
            Ok(checkpoint)
        })
        .await;

        match result {
            Ok(Ok(checkpoint)) => {
                if self.authority_reconciliation_is_required() {
                    Err(self
                        .authority_reconciliation_error(SnapshotAuthorityOperation::ShutdownMark))
                } else {
                    Ok(checkpoint)
                }
            }
            Ok(Err(error)) if error.requires_reconciliation() => Err(error),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                // `timeout_with_cx` returns the same outer error for timeout and
                // budget exhaustion. Do not launch a second mark outside that
                // boundary: SQLite's busy timeout would otherwise make the API
                // exceed its advertised wall-time bound by several seconds.
                let operation = if checkpoint_receipt.is_some() {
                    SnapshotAuthorityOperation::ShutdownMark
                } else {
                    SnapshotAuthorityOperation::CheckpointCommit
                };
                let source = if self.authority_reconciliation_is_required() {
                    self.authority_reconciliation_error(operation)
                } else {
                    classify_shutdown_timeout(cx, timeout)
                };
                tracing::warn!(
                    error_class = source.diagnostic_class(),
                    "Shutdown checkpoint did not settle inside its capability/time boundary"
                );
                if let Some(checkpoint) = checkpoint_receipt.take() {
                    Err(SnapshotError::ShutdownMarkFailed {
                        checkpoint: Box::new(checkpoint),
                        source: Box::new(source),
                    })
                } else {
                    Err(source)
                }
            }
        }
    }

    /// Access the operational telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &SnapshotEngineTelemetry {
        &self.telemetry
    }

    /// Close the engine session using one exact committed checkpoint receipt.
    /// A stale or foreign receipt cannot mark a newer durable state clean.
    pub async fn close_after_checkpoint(
        &self,
        checkpoint: &SnapshotResult,
    ) -> std::result::Result<(), SnapshotError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.close_after_checkpoint_with_cx(&cx, checkpoint).await
    }

    /// Cx-first sibling of [`close_after_checkpoint`](Self::close_after_checkpoint).
    ///
    /// Cancellation is accepted before the session-id lock/DB mutation. Once
    /// admitted, loss of the blocking task's typed result is reported as an
    /// indeterminate authority mutation and latches all later durable writes.
    pub async fn close_after_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        checkpoint: &SnapshotResult,
    ) -> std::result::Result<(), SnapshotError> {
        snapshot_cx_checkpoint(cx)?;
        if self.authority_reconciliation_is_required() {
            return Err(
                self.authority_reconciliation_error(SnapshotAuthorityOperation::ShutdownMark)
            );
        }
        let reservation = self.reserve_capture_lifecycle(cx).await?;
        self.mark_shutdown_with_reservation(cx, &reservation, checkpoint)
            .await
    }

    async fn mark_shutdown_with_reservation(
        &self,
        cx: &crate::cx::Cx,
        reservation: &CaptureShutdownReservation<'_>,
        checkpoint: &SnapshotResult,
    ) -> std::result::Result<(), SnapshotError> {
        snapshot_cx_checkpoint(cx)?;
        // Route the session_id read-lock through read_with_cx(cx) so the lock
        // wait honors caller cancellation rather than an ambient context.
        let session_id = {
            self.session_id
                .read_with_cx(cx)
                .await
                .map_err(snapshot_lock_error)?
                .clone()
        };
        // The read may have waited behind a first capture. That capture can
        // lose its result, latch reconciliation, and release the session lock
        // without publishing an ID, so recheck before treating `None` as an
        // authoritative no-op.
        if self.authority_reconciliation_is_required() {
            return Err(
                self.authority_reconciliation_error(SnapshotAuthorityOperation::ShutdownMark)
            );
        }
        let id = session_id.ok_or(SnapshotError::ContextFailure)?;
        if id != checkpoint.session_id {
            return Err(SnapshotError::ContextFailure);
        }
        let db_path = Arc::clone(&self.db_path);
        let checkpoint_id = checkpoint.checkpoint_id;
        let checkpoint_at = checkpoint.checkpoint_at;
        let state_hash = checkpoint.state_hash.clone();
        let owner_identity = self.owner_identity.clone();
        self.spawn_blocking_authority_with_cx(
            cx,
            SnapshotAuthorityOperation::ShutdownMark,
            move || {
                mark_shutdown_authoritatively_sync(
                    &db_path,
                    &id,
                    checkpoint_id,
                    checkpoint_at,
                    &state_hash,
                    owner_identity.as_ref(),
                )
            },
        )
        .await?;

        reservation.complete()
    }
}

/// Run a non-SnapshotEngine checkpoint mutation through the same frozen
/// database locator, exclusive same-process admission, cancellation handoff,
/// and sticky reconciliation latch as capture/delete/cleanup. Restore receipt
/// bookkeeping uses this doorway instead of creating a second authority lane.
pub(crate) async fn run_checkpoint_authority_with_cx<T, E, F>(
    cx: &crate::cx::Cx,
    db_path: Arc<String>,
    operation: SnapshotAuthorityOperation,
    work: F,
) -> std::result::Result<T, SnapshotError>
where
    T: Send + 'static,
    E: SnapshotAuthorityWorkFailure + Send + 'static,
    F: FnOnce(&str) -> std::result::Result<T, E> + Send + 'static,
{
    let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
    let frozen_db_path = Arc::clone(&engine.db_path);
    engine
        .spawn_blocking_authority_with_cx(cx, operation, move || work(frozen_db_path.as_str()))
        .await
}

/// Synchronous sibling for explicitly synchronous callers. It shares the
/// same database-keyed admission and sticky error classification, but performs
/// the caller-supplied transaction on the current blocking thread.
pub(crate) fn run_checkpoint_authority_sync<T, E, F>(
    db_path: Arc<String>,
    operation: SnapshotAuthorityOperation,
    work: F,
) -> std::result::Result<T, SnapshotError>
where
    E: SnapshotAuthorityWorkFailure,
    F: FnOnce(&str) -> std::result::Result<T, E>,
{
    let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
    let attempt = engine.try_begin_snapshot_authority(operation)?;
    let handoff_state = attempt.handoff_state();
    let outcome = run_authority_work_if_started(&handoff_state, || work(engine.db_path.as_str()));
    let outcome = match outcome {
        AuthorityBlockingOutcome::Executed(result) => {
            match refresh_snapshot_authority_file_identities(
                engine.db_path.as_str(),
                &engine.snapshot_authority,
                operation,
            ) {
                Ok(()) => AuthorityBlockingOutcome::Executed(result),
                Err(error) => AuthorityBlockingOutcome::IdentityRefreshFailed(error),
            }
        }
        other => other,
    };
    match outcome {
        AuthorityBlockingOutcome::Executed(Ok(value)) => {
            attempt.settle();
            Ok(value)
        }
        AuthorityBlockingOutcome::Executed(Err(error)) if error.requires_reconciliation() => {
            tracing::warn!(
                %operation,
                error_class = "indeterminate_database_authority",
                "synchronous checkpoint authority work returned an indeterminate outcome"
            );
            attempt.latch_and_settle();
            Err(SnapshotError::IndeterminateAuthorityMutation { operation })
        }
        AuthorityBlockingOutcome::Executed(Err(error)) => {
            attempt.settle();
            Err(SnapshotError::Database(error.to_string()))
        }
        AuthorityBlockingOutcome::Suppressed => {
            // A synchronous caller has no cancellation path capable of
            // suppressing this just-created handoff. Preserve fail-closed
            // behavior if that invariant is ever violated.
            attempt.latch_and_settle();
            Err(SnapshotError::IndeterminateAuthorityMutation { operation })
        }
        AuthorityBlockingOutcome::IdentityRefreshFailed(error) => {
            tracing::warn!(
                %operation,
                error = %error,
                "synchronous authority identity refresh failed"
            );
            attempt.latch_and_settle();
            Err(SnapshotError::IndeterminateAuthorityMutation { operation })
        }
    }
}

const SNAPSHOT_SQLITE_IN_LIST_CHUNK: usize = 900;
// Snapshot correlation consumes only a stable rule ID and a small structured
// session hint. These caps bound corrupt/legacy event rows before SQLite moves
// their text into Rust.
const SNAPSHOT_DETECTION_RULE_ID_BYTES: usize = 1024;
const SNAPSHOT_DETECTION_EXTRACTED_BYTES: usize = 16 * 1024;

/// Load the most recent bounded, supported-agent detection per pane from
/// storage. Mux-level and unknown-provider events are intentionally outside
/// snapshot agent-correlation authority.
///
/// This is best-effort: if the `events` table does not exist (e.g., tests using a
/// minimal schema), it returns an empty map.
fn load_latest_detections_by_pane_sync(
    db_path: &str,
    pane_ids: &[u64],
    cutoff_ms: i64,
) -> std::result::Result<std::collections::HashMap<u64, Detection>, rusqlite::Error> {
    use std::collections::HashMap;

    if pane_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let conn = open_snapshot_query_conn(db_path)?;
    let mut out: HashMap<u64, Detection> = HashMap::with_capacity(pane_ids.len());
    for pane_chunk in pane_ids.chunks(SNAPSHOT_SQLITE_IN_LIST_CHUNK) {
        let placeholders = std::iter::repeat_n("?", pane_chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "WITH ranked AS (
                 SELECT pane_id,
                        rule_id,
                        agent_type,
                        CASE
                            WHEN extracted IS NULL THEN NULL
                            WHEN typeof(extracted) = 'text'
                             AND length(CAST(extracted AS BLOB)) <= {SNAPSHOT_DETECTION_EXTRACTED_BYTES}
                            THEN extracted
                        END AS bounded_extracted,
                        ROW_NUMBER() OVER (
                            PARTITION BY pane_id
                            ORDER BY detected_at DESC, id DESC
                        ) AS rn
                 FROM events
                 WHERE pane_id IN ({placeholders})
                   AND typeof(pane_id) = 'integer'
                   AND typeof(detected_at) = 'integer'
                   AND detected_at >= ?
                   AND typeof(rule_id) = 'text'
                   AND length(CAST(rule_id AS BLOB)) BETWEEN 1 AND {SNAPSHOT_DETECTION_RULE_ID_BYTES}
                   AND typeof(agent_type) = 'text'
                   AND agent_type IN ('codex', 'claude_code', 'gemini')
             )
             SELECT pane_id, rule_id, agent_type, bounded_extracted
             FROM ranked
             WHERE rn = 1"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(err) if is_missing_events_table(&err) => return Ok(HashMap::new()),
            Err(err) => return Err(err),
        };
        let mut params = Vec::with_capacity(pane_chunk.len().saturating_add(1));
        for &pane_id in pane_chunk {
            params.push(u64_to_sqlite_integer(pane_id)?);
        }
        params.push(cutoff_ms);

        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        while let Some(row) = rows.next()? {
            let pane_id = sqlite_integer_to_u64(0, row.get(0)?)?;
            let rule_id: String = row.get(1)?;
            let agent_type: String = row.get(2)?;
            let extracted: Option<String> = row.get(3)?;
            let detection = Detection {
                rule_id,
                agent_type: agent_type_from_db(&agent_type),
                event_type: String::new(),
                severity: Severity::Info,
                confidence: 0.0,
                extracted: extracted
                    .as_deref()
                    .and_then(|value| serde_json::from_str::<Value>(value).ok())
                    .unwrap_or(Value::Null),
                matched_text: String::new(),
                span: (0, 0),
            };
            out.insert(pane_id, detection);
        }
    }

    Ok(out)
}

/// Load the exact retained-segment projection for the requested panes.
///
/// Schema v39 maintains these rows transactionally at append/retention time.
/// This query must remain independent of `output_segments` history depth; a
/// `MIN`/`MAX`/`COUNT` regression here reintroduces long-session snapshot lag.
fn load_latest_scrollback_refs_sync(
    db_path: &str,
    pane_ids: &[u64],
) -> std::result::Result<std::collections::HashMap<u64, ScrollbackRef>, String> {
    use std::collections::HashMap;

    if pane_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let conn = open_snapshot_query_conn(db_path).map_err(|error| error.to_string())?;
    let mut refs = HashMap::with_capacity(pane_ids.len());
    for pane_chunk in pane_ids.chunks(SNAPSHOT_SQLITE_IN_LIST_CHUNK) {
        let placeholders = std::iter::repeat_n("?", pane_chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT pane_id, retained_segment_count, first_seq, last_seq,
                    first_captured_at, last_captured_at
             FROM pane_scrollback_summary
             WHERE pane_id IN ({placeholders})"
        );
        let mut params = Vec::with_capacity(pane_chunk.len());
        for &pane_id in pane_chunk {
            params.push(
                i64::try_from(pane_id)
                    .map_err(|_| format!("pane_id {pane_id} exceeds sqlite integer range"))?,
            );
        }
        let mut stmt = conn.prepare(&sql).map_err(|error| error.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|error| error.to_string())?;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let pane_id_raw: i64 = row.get(0).map_err(|error| error.to_string())?;
            let pane_id =
                sqlite_integer_to_u64(0, pane_id_raw).map_err(|error| error.to_string())?;
            let retained_segment_count: i64 = row.get(1).map_err(|error| error.to_string())?;
            let first_seq: Option<i64> = row.get(2).map_err(|error| error.to_string())?;
            let last_seq: Option<i64> = row.get(3).map_err(|error| error.to_string())?;
            let first_capture_at: Option<i64> = row.get(4).map_err(|error| error.to_string())?;
            let last_capture_at: Option<i64> = row.get(5).map_err(|error| error.to_string())?;

            if retained_segment_count < 0 {
                return Err(format!(
                    "invalid scrollback summary for pane_id={pane_id}: \
                     negative retained segment count {retained_segment_count}"
                ));
            }
            if retained_segment_count == 0 {
                if first_seq.is_some()
                    || last_seq.is_some()
                    || first_capture_at.is_some()
                    || last_capture_at.is_some()
                {
                    return Err(format!(
                        "invalid empty scrollback summary for pane_id={pane_id}: non-null bounds"
                    ));
                }
                continue;
            }

            let first_seq = first_seq.ok_or_else(|| {
                format!("invalid scrollback summary for pane_id={pane_id}: missing first_seq")
            })?;
            let output_segments_seq = last_seq.ok_or_else(|| {
                format!("invalid scrollback summary for pane_id={pane_id}: missing last_seq")
            })?;
            let first_capture_at = first_capture_at.ok_or_else(|| {
                format!(
                    "invalid scrollback summary for pane_id={pane_id}: missing first_captured_at"
                )
            })?;
            let Some(last_capture_at) = last_capture_at else {
                return Err(format!(
                    "invalid scrollback summary for pane_id={pane_id}: missing last_captured_at"
                ));
            };
            if first_seq < 0
                || output_segments_seq < 0
                || first_seq > output_segments_seq
                || first_capture_at < 0
                || last_capture_at < 0
                || first_capture_at > last_capture_at
            {
                return Err(format!(
                    "invalid scrollback summary for pane_id={pane_id}: \
                     first_seq={first_seq}, last_seq={output_segments_seq}, \
                     retained_segment_count={retained_segment_count}, \
                     first_capture_at={first_capture_at}, \
                     last_capture_at={last_capture_at}"
                ));
            }

            refs.insert(
                pane_id,
                ScrollbackRef {
                    output_segments_seq,
                    retained_segment_count: u64::try_from(retained_segment_count)
                        .map_err(|error| error.to_string())?,
                    last_capture_at: u64::try_from(last_capture_at)
                        .map_err(|error| error.to_string())?,
                },
            );
        }
    }

    Ok(refs)
}

fn is_missing_events_table(err: &rusqlite::Error) -> bool {
    err.to_string().contains("no such table: events")
}

fn agent_type_from_db(agent_type: &str) -> AgentType {
    match agent_type {
        "codex" => AgentType::Codex,
        "claude_code" => AgentType::ClaudeCode,
        "gemini" => AgentType::Gemini,
        "wezterm" => AgentType::Wezterm,
        _ => AgentType::Unknown,
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn epoch_ms() -> u64 {
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(epoch_ms).unwrap_or(u64::MAX)
}

fn checkpoint_cleanup_interval(configured_snapshot_interval_seconds: u64) -> Duration {
    Duration::from_secs(configured_snapshot_interval_seconds).clamp(
        CHECKPOINT_CLEANUP_MIN_INTERVAL,
        CHECKPOINT_CLEANUP_MAX_INTERVAL,
    )
}

fn checkpoint_cleanup_due(
    cadence: &CheckpointCleanupCadence,
    interval: Duration,
    now: Instant,
) -> bool {
    if cadence.in_progress {
        return false;
    }
    if cadence.retry_deferred_at.is_some_and(|deferred_at| {
        now.saturating_duration_since(deferred_at) < CHECKPOINT_CLEANUP_RETRY_DELAY
    }) {
        return false;
    }
    cadence
        .last_authoritative_success
        .is_none_or(|completed_at| now.saturating_duration_since(completed_at) >= interval)
}

/// Monotonic wait until this database may next run automatic checkpoint
/// pruning. An in-flight peer is polled only at the bounded retry cadence so a
/// dropped peer cannot strand cleanup, while a successful peer advances the
/// shared authoritative-success timestamp for every engine.
fn checkpoint_cleanup_wait_duration(
    cadence: &CheckpointCleanupCadence,
    interval: Duration,
    now: Instant,
) -> Duration {
    if cadence.in_progress {
        return CHECKPOINT_CLEANUP_RETRY_DELAY;
    }
    if let Some(deferred_at) = cadence.retry_deferred_at {
        let elapsed = now.saturating_duration_since(deferred_at);
        if elapsed < CHECKPOINT_CLEANUP_RETRY_DELAY {
            return CHECKPOINT_CLEANUP_RETRY_DELAY.saturating_sub(elapsed);
        }
    }
    cadence
        .last_authoritative_success
        .map_or(Duration::ZERO, |completed_at| {
            interval.saturating_sub(now.saturating_duration_since(completed_at))
        })
}

/// Decide whether `[snapshots.session_retention]` cleanup is due (ft-0yuxe).
///
/// * a retry deferral younger than [`SESSION_CLEANUP_RETRY_DELAY`] is not due;
/// * no authoritative success — the startup pass is due, including when
///   `interval_hours == 0`;
/// * `interval_hours == 0` — never due again after authoritative success;
/// * otherwise — due once `interval_hours` have elapsed since authoritative
///   success.
///
/// Uses `saturating_duration_since` so a non-monotonic `now < prev` reads as
/// zero elapsed (not due) rather than panicking.
fn session_cleanup_due(
    schedule: &SessionCleanupSchedule,
    interval_hours: u64,
    now: Instant,
) -> bool {
    if schedule.retry_deferred_at.is_some_and(|deferred_at| {
        now.saturating_duration_since(deferred_at) < SESSION_CLEANUP_RETRY_DELAY
    }) {
        return false;
    }

    match schedule.last_authoritative_success {
        None => true,
        Some(prev) => {
            interval_hours > 0
                && now.saturating_duration_since(prev)
                    >= Duration::from_secs(interval_hours.saturating_mul(3600))
        }
    }
}

/// Return the monotonic wait until the next automatic cleanup decision.
/// `None` means an interval-zero scheduler already obtained its authoritative
/// startup receipt and has no further cleanup deadline. A zero duration means
/// cleanup is due now.
fn session_cleanup_wait_duration(
    schedule: &SessionCleanupSchedule,
    interval_hours: u64,
    now: Instant,
) -> Option<Duration> {
    if let Some(deferred_at) = schedule.retry_deferred_at {
        let elapsed = now.saturating_duration_since(deferred_at);
        if elapsed < SESSION_CLEANUP_RETRY_DELAY {
            return Some(SESSION_CLEANUP_RETRY_DELAY.saturating_sub(elapsed));
        }
    }

    match schedule.last_authoritative_success {
        None => Some(Duration::ZERO),
        Some(_) if interval_hours == 0 => None,
        Some(last_success) => {
            let interval = Duration::from_secs(interval_hours.saturating_mul(3600));
            Some(interval.saturating_sub(now.saturating_duration_since(last_success)))
        }
    }
}

fn periodic_scheduler_wait_duration(
    cleanup_schedule: &SessionCleanupSchedule,
    cleanup_interval_hours: u64,
    cleanup_reconciliation_required: bool,
    last_snapshot_completion: Option<Instant>,
    snapshot_retry_not_before: Option<Instant>,
    snapshot_interval: Duration,
    now: Instant,
) -> Duration {
    let cadence_wait = last_snapshot_completion.map_or(Duration::ZERO, |last| {
        snapshot_interval.saturating_sub(now.saturating_duration_since(last))
    });
    let retry_wait = snapshot_retry_not_before
        .map(|retry_at| retry_at.saturating_duration_since(now))
        .unwrap_or(Duration::ZERO);
    let snapshot_wait = cadence_wait.max(retry_wait);
    let cleanup_wait = if cleanup_reconciliation_required {
        None
    } else {
        session_cleanup_wait_duration(cleanup_schedule, cleanup_interval_hours, now)
    };
    cleanup_wait.map_or(snapshot_wait, |wait| wait.min(snapshot_wait))
}

/// Generate a time-ordered session ID (UUID v7-like: timestamp prefix + random).
fn generate_session_id() -> String {
    let ts = epoch_ms();
    let rand: u64 = rand::random();
    format!("sess-{ts:013x}-{rand:016x}")
}

// Bound caller-controlled strings before JSON encoding. JSON escaping can
// expand these prefixes, so the authoritative 64 KiB check is still applied to
// the encoded row below.
const SNAPSHOT_TEXT_FIELD_INPUT_BYTES: usize = 8 * 1024;
const SNAPSHOT_AGENT_FIELD_INPUT_BYTES: usize = 4 * 1024;
const SNAPSHOT_ENV_VALUE_INPUT_BYTES: usize = 4 * 1024;
const SNAPSHOT_METADATA_MAX_DEPTH: usize = 64;
const SNAPSHOT_METADATA_MAX_NODES: usize = 65_536;
// Keep this in lockstep with the restore reader's host-id admission boundary.
#[cfg(test)]
const SNAPSHOT_HOST_ID_INPUT_BYTES: usize = 1024;
const FOREGROUND_PROCESS_NAME_FIELD: &str = "foreground_process_name";

struct JsonByteCounter {
    bytes: usize,
    max_bytes: usize,
    overflowed: bool,
    limit_exceeded: bool,
}

impl JsonByteCounter {
    const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: 0,
            max_bytes,
            overflowed: false,
            limit_exceeded: false,
        }
    }
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self.bytes.checked_add(buffer.len()) {
            Some(bytes) => {
                self.bytes = bytes;
                if bytes > self.max_bytes {
                    self.limit_exceeded = true;
                    return Err(std::io::Error::other(
                        "checkpoint metadata byte limit exceeded",
                    ));
                }
            }
            None => {
                self.bytes = usize::MAX;
                self.overflowed = true;
                return Err(std::io::Error::other(
                    "checkpoint metadata byte count overflowed",
                ));
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }

    const ELLIPSIS: &str = "…";
    let mut end = max_bytes.saturating_sub(ELLIPSIS.len()).min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(end.saturating_add(ELLIPSIS.len()));
    bounded.push_str(&value[..end]);
    if max_bytes >= ELLIPSIS.len() {
        bounded.push_str(ELLIPSIS);
    }
    (bounded, true)
}

fn bounded_optional_utf8(value: Option<&str>, max_bytes: usize) -> (Option<String>, bool) {
    value.map_or((None, false), |value| {
        let (bounded, truncated) = bounded_utf8(value, max_bytes);
        (Some(bounded), truncated)
    })
}

/// Preserve exact optional authority values or omit them. Prefix truncation is
/// appropriate for display text, but it can manufacture a different cwd,
/// process name, environment value, or session identity.
fn admitted_optional_utf8(value: Option<&str>, max_bytes: usize) -> (Option<String>, bool) {
    value.map_or((None, false), |value| {
        if value.len() > max_bytes {
            (None, true)
        } else {
            (Some(value.to_owned()), false)
        }
    })
}

/// Minimal, bounded pane-list projection consumed by snapshot topology,
/// per-pane state, and agent correlation. Keeping one projection prevents the
/// topology copy of cwd/title/workspace from retaining the raw oversized value
/// after the pane-row copy has been shortened.
struct SnapshotPaneProjection {
    panes: Vec<PaneInfo>,
    topology_workspace_id: Option<String>,
    truncated_pane_ids: HashSet<u64>,
}

fn project_snapshot_panes(
    panes: &[PaneInfo],
) -> std::result::Result<SnapshotPaneProjection, SnapshotError> {
    if panes.len() > MAX_TOPOLOGY_PANES {
        return Err(SnapshotError::Topology(
            TopologySnapshotError::ResourceLimit {
                resource: "panes",
                count: panes.len(),
                limit: MAX_TOPOLOGY_PANES,
            },
        ));
    }

    let first_workspace = panes.first().and_then(|pane| pane.workspace.as_deref());
    // Exact equality across multiple arbitrarily large workspace strings can
    // turn this async-thread preprojection into an unbounded prefix scan. An
    // oversized workspace is not restorable authority, so omit it instead of
    // manufacturing a truncated identifier that can alias another workspace.
    let oversized_workspace = panes.iter().any(|pane| {
        pane.workspace
            .as_ref()
            .is_some_and(|workspace| workspace.len() > SNAPSHOT_TEXT_FIELD_INPUT_BYTES)
    });
    let uniform_workspace = !oversized_workspace
        && panes
            .iter()
            .all(|pane| pane.workspace.as_deref() == first_workspace);
    let topology_workspace_id = if oversized_workspace {
        None
    } else if uniform_workspace {
        first_workspace.map(str::to_owned)
    } else {
        // Mixed workspace identity is deliberately unknown. Bounding each
        // value first could alias two different long names onto one prefix and
        // manufacture false single-workspace authority.
        None
    };

    let mut projected = Vec::with_capacity(panes.len());
    // Ordinary pane metadata is already small. Avoid reserving a second
    // per-pane table unless at least one projection actually loses data.
    let mut truncated_pane_ids = HashSet::new();
    for pane in panes {
        let (title, title_truncated) =
            bounded_optional_utf8(pane.title.as_deref(), SNAPSHOT_TEXT_FIELD_INPUT_BYTES);
        let (cwd, cwd_truncated) =
            admitted_optional_utf8(pane.cwd.as_deref(), SNAPSHOT_TEXT_FIELD_INPUT_BYTES);

        // AgentCorrelator consumes only this one forward-compatible extra. Do
        // not clone arbitrary backend extras into the blocking snapshot phase.
        // Most current mux projections do not expose a typed foreground
        // process. Keep the empty case allocation-free across large sessions.
        let mut extra = HashMap::new();
        let process = pane
            .extra
            .get(FOREGROUND_PROCESS_NAME_FIELD)
            .and_then(Value::as_str);
        let process_truncated =
            if process.is_some_and(|process| process.len() > SNAPSHOT_TEXT_FIELD_INPUT_BYTES) {
                true
            } else if let Some(process) = process {
                extra.insert(
                    FOREGROUND_PROCESS_NAME_FIELD.to_owned(),
                    Value::String(process.to_owned()),
                );
                false
            } else {
                false
            };

        let workspace_omitted = pane
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.len() > SNAPSHOT_TEXT_FIELD_INPUT_BYTES);
        if workspace_omitted || title_truncated || cwd_truncated || process_truncated {
            truncated_pane_ids.insert(pane.pane_id);
        }

        projected.push(PaneInfo {
            pane_id: pane.pane_id,
            tab_id: pane.tab_id,
            window_id: pane.window_id,
            domain_id: pane.domain_id,
            domain_name: None,
            // TopologySnapshot receives the one separately-projected workspace
            // identity after construction; per-pane copies are unnecessary.
            workspace: None,
            size: pane.size.clone(),
            rows: pane.rows,
            cols: pane.cols,
            title,
            cwd,
            tty_name: None,
            cursor_x: pane.cursor_x,
            cursor_y: pane.cursor_y,
            cursor_visibility: pane.cursor_visibility,
            left_col: pane.left_col,
            top_row: pane.top_row,
            is_active: pane.is_active,
            is_zoomed: pane.is_zoomed,
            extra,
        });
    }

    Ok(SnapshotPaneProjection {
        panes: projected,
        topology_workspace_id,
        truncated_pane_ids,
    })
}

fn bounded_agent_metadata(agent: &AgentMetadata) -> (Option<AgentMetadata>, bool) {
    if agent.agent_type.len() > SNAPSHOT_AGENT_FIELD_INPUT_BYTES {
        return (None, true);
    }
    let (session_id, session_truncated) = admitted_optional_utf8(
        agent.session_id.as_deref(),
        SNAPSHOT_AGENT_FIELD_INPUT_BYTES,
    );
    let (state, state_truncated) =
        admitted_optional_utf8(agent.state.as_deref(), SNAPSHOT_AGENT_FIELD_INPUT_BYTES);
    (
        Some(AgentMetadata {
            agent_type: agent.agent_type.clone(),
            session_id,
            state,
        }),
        session_truncated || state_truncated,
    )
}

fn bounded_captured_env(env: &CapturedEnv) -> (CapturedEnv, bool) {
    let mut vars = HashMap::with_capacity(SAFE_ENV_VARS.len().min(env.vars.len()));
    let mut truncated = false;
    for key in SAFE_ENV_VARS {
        let Some(value) = env.vars.get(*key) else {
            continue;
        };
        if value.len() > SNAPSHOT_ENV_VALUE_INPUT_BYTES {
            truncated = true;
        } else {
            vars.insert((*key).to_owned(), value.clone());
        }
    }
    // A programmatic caller can construct CapturedEnv directly. Refuse to
    // persist names outside the capture allow-list, and make that omission
    // visible through the same truncation evidence.
    truncated |= vars.len() != env.vars.len();
    (
        CapturedEnv {
            vars,
            redacted_count: env.redacted_count,
        },
        truncated,
    )
}

fn admit_checkpoint_metadata_child<'a>(
    stack: &mut Vec<(&'a Value, usize)>,
    discovered_nodes: &mut usize,
    child: &'a Value,
    child_depth: usize,
) -> Result<(), SnapshotPreparationError> {
    *discovered_nodes = discovered_nodes
        .checked_add(1)
        .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
    if *discovered_nodes > SNAPSHOT_METADATA_MAX_NODES {
        return Err(SnapshotPreparationError::MetadataShapeResourceLimit {
            resource: SnapshotProjectionResource::MetadataNodes,
            observed: *discovered_nodes,
            limit: SNAPSHOT_METADATA_MAX_NODES,
        });
    }
    stack.push((child, child_depth));
    Ok(())
}

fn canonical_checkpoint_metadata(metadata: &Value) -> Result<String, SnapshotPreparationError> {
    // `canonical_json_string` recursively rebuilds the Value tree. Bound both
    // recursion depth and allocation-amplifying node count iteratively before
    // either serde or canonicalization receives programmatically-built input.
    let mut discovered_nodes = 1_usize;
    let mut raw_string_bytes = 0_usize;
    let mut stack = vec![(metadata, 1_usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > SNAPSHOT_METADATA_MAX_DEPTH {
            return Err(SnapshotPreparationError::MetadataShapeResourceLimit {
                resource: SnapshotProjectionResource::MetadataDepth,
                observed: depth,
                limit: SNAPSHOT_METADATA_MAX_DEPTH,
            });
        }
        let child_depth = depth
            .checked_add(1)
            .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
        match value {
            Value::String(text) => {
                raw_string_bytes = raw_string_bytes
                    .checked_add(text.len())
                    .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
            }
            Value::Array(children) => {
                for child in children {
                    admit_checkpoint_metadata_child(
                        &mut stack,
                        &mut discovered_nodes,
                        child,
                        child_depth,
                    )?;
                }
            }
            Value::Object(fields) => {
                for (key, child) in fields {
                    raw_string_bytes = raw_string_bytes
                        .checked_add(key.len())
                        .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
                    if raw_string_bytes > MAX_CHECKPOINT_METADATA_BYTES {
                        return Err(SnapshotPreparationError::MetadataResourceLimit {
                            bytes: raw_string_bytes,
                            limit: MAX_CHECKPOINT_METADATA_BYTES,
                        });
                    }
                    admit_checkpoint_metadata_child(
                        &mut stack,
                        &mut discovered_nodes,
                        child,
                        child_depth,
                    )?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
        if raw_string_bytes > MAX_CHECKPOINT_METADATA_BYTES {
            return Err(SnapshotPreparationError::MetadataResourceLimit {
                bytes: raw_string_bytes,
                limit: MAX_CHECKPOINT_METADATA_BYTES,
            });
        }
    }

    // Canonical key ordering cannot change compact JSON's encoded length.
    // Count through serde's writer first so an oversized Value never gets
    // cloned into a second full tree and String merely to discover the cap.
    let mut counter = JsonByteCounter::new(MAX_CHECKPOINT_METADATA_BYTES);
    let serialization = serde_json::to_writer(&mut counter, metadata);
    if counter.overflowed {
        return Err(SnapshotPreparationError::ByteCountOverflow);
    }
    if counter.limit_exceeded {
        return Err(SnapshotPreparationError::MetadataResourceLimit {
            bytes: counter.bytes,
            limit: MAX_CHECKPOINT_METADATA_BYTES,
        });
    }
    serialization.map_err(CheckpointWitnessError::from)?;

    let canonical = canonical_json_string(metadata)?;
    debug_assert_eq!(
        canonical.len(),
        counter.bytes,
        "canonical object-key ordering must preserve compact JSON byte length"
    );
    Ok(canonical)
}

fn reduce_persisted_pane_to_budget(
    pane: &mut PersistedPaneState,
) -> Result<bool, SnapshotPreparationError> {
    let mut bytes =
        persisted_pane_text_bytes(pane).ok_or(SnapshotPreparationError::ByteCountOverflow)?;
    if bytes <= MAX_PERSISTED_PANE_TEXT_BYTES {
        return Ok(false);
    }

    // Keep the deterministic order stable: environment, agent observation,
    // command, cwd, then title. The numeric terminal geometry and scrollback
    // references always survive.
    for tier in 0..5 {
        match tier {
            0 => pane.env_json = None,
            1 => pane.agent_metadata_json = None,
            2 => pane.command = None,
            3 => pane.cwd = None,
            4 => {
                let mut terminal: crate::session_pane_state::TerminalState =
                    serde_json::from_str(&pane.terminal_state_json)
                        .map_err(CheckpointWitnessError::from)?;
                terminal.title.clear();
                pane.terminal_state_json = canonical_json_string(&terminal)?;
            }
            _ => unreachable!(),
        }
        bytes =
            persisted_pane_text_bytes(pane).ok_or(SnapshotPreparationError::ByteCountOverflow)?;
        if bytes <= MAX_PERSISTED_PANE_TEXT_BYTES {
            return Ok(true);
        }
    }

    Err(SnapshotPreparationError::PaneTextResourceLimit {
        pane_id: pane.pane_id,
        bytes,
        limit: MAX_PERSISTED_PANE_TEXT_BYTES,
    })
}

#[derive(Debug, thiserror::Error)]
enum SnapshotPreparationError {
    #[error(transparent)]
    Topology(#[from] TopologySnapshotError),
    #[error(transparent)]
    Witness(#[from] CheckpointWitnessError),
    #[error("snapshot pane id {0} exceeds SQLite integer range")]
    PaneIdRange(u64),
    #[error("snapshot scrollback sequence cannot be negative")]
    NegativeScrollbackSequence,
    #[error("snapshot timestamp {0} exceeds SQLite integer range")]
    TimestampRange(u64),
    #[error("snapshot serialized byte count overflow")]
    ByteCountOverflow,
    #[error("snapshot pane count exceeds SQLite integer range")]
    PaneCountOverflow,
    #[error(
        "snapshot topology/pane-state identity sets disagree (topology panes: {topology_panes}, pane states: {pane_states})"
    )]
    PaneIdentitySetMismatch {
        topology_panes: usize,
        pane_states: usize,
    },
    #[error("snapshot metadata is {bytes} bytes; limit is {limit}")]
    MetadataResourceLimit { bytes: usize, limit: usize },
    #[error("snapshot metadata {resource} is {observed}; limit is {limit}")]
    MetadataShapeResourceLimit {
        resource: SnapshotProjectionResource,
        observed: usize,
        limit: usize,
    },
    #[error("snapshot pane {pane_id} stores {bytes} text bytes; limit is {limit}")]
    PaneTextResourceLimit {
        pane_id: i64,
        bytes: usize,
        limit: usize,
    },
    #[error("snapshot stores {bytes} admitted text bytes; limit is {limit}")]
    CheckpointTextResourceLimit { bytes: usize, limit: usize },
}

impl From<SnapshotPreparationError> for SnapshotError {
    fn from(error: SnapshotPreparationError) -> Self {
        match error {
            SnapshotPreparationError::Topology(source) => Self::Topology(source),
            SnapshotPreparationError::PaneIdentitySetMismatch {
                topology_panes,
                pane_states,
            } => Self::PaneIdentitySetMismatch {
                topology_panes,
                pane_states,
            },
            SnapshotPreparationError::MetadataResourceLimit { bytes, limit } => {
                Self::ProjectionResourceLimit {
                    resource: SnapshotProjectionResource::MetadataBytes,
                    observed: bytes,
                    limit,
                }
            }
            SnapshotPreparationError::MetadataShapeResourceLimit {
                resource,
                observed,
                limit,
            } => Self::ProjectionResourceLimit {
                resource,
                observed,
                limit,
            },
            SnapshotPreparationError::PaneTextResourceLimit { bytes, limit, .. } => {
                Self::ProjectionResourceLimit {
                    resource: SnapshotProjectionResource::PaneTextBytes,
                    observed: bytes,
                    limit,
                }
            }
            SnapshotPreparationError::CheckpointTextResourceLimit { bytes, limit } => {
                Self::ProjectionResourceLimit {
                    resource: SnapshotProjectionResource::CheckpointTextBytes,
                    observed: bytes,
                    limit,
                }
            }
            other => Self::Serialization(other.to_string()),
        }
    }
}

#[derive(Clone)]
struct PreparedSnapshotPersistence {
    topology_json: String,
    metadata_json: Option<String>,
    panes: Vec<PersistedPaneState>,
    pane_count: usize,
    pane_count_sql: i64,
    total_bytes: usize,
    total_bytes_sql: i64,
    persisted_text_bytes: usize,
    truncated_pane_count: usize,
    dedup_hash: String,
}

impl std::fmt::Debug for PreparedSnapshotPersistence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedSnapshotPersistence")
            .field("has_topology", &!self.topology_json.is_empty())
            .field("has_metadata", &self.metadata_json.is_some())
            .field("persisted_pane_rows", &self.panes.len())
            .field("pane_count", &self.pane_count)
            .field("pane_count_sql", &self.pane_count_sql)
            .field("total_bytes", &self.total_bytes)
            .field("total_bytes_sql", &self.total_bytes_sql)
            .field("persisted_text_bytes", &self.persisted_text_bytes)
            .field("truncated_pane_count", &self.truncated_pane_count)
            .finish_non_exhaustive()
    }
}

/// Build once from exactly the values the SQLite transaction will insert.
/// Non-persisted process PID/argv, shell, and scrollback segment counts are
/// deliberately absent so they cannot manufacture redundant checkpoints.
#[cfg(test)]
fn prepare_snapshot_persistence(
    topology: &TopologySnapshot,
    pane_states: &[PaneStateSnapshot],
    metadata: Option<&Value>,
) -> std::result::Result<PreparedSnapshotPersistence, SnapshotPreparationError> {
    prepare_snapshot_persistence_with_prebounded_panes(
        topology,
        pane_states,
        metadata,
        &HashSet::new(),
    )
}

#[cfg(test)]
fn prepare_snapshot_persistence_with_prebounded_panes(
    topology: &TopologySnapshot,
    pane_states: &[PaneStateSnapshot],
    metadata: Option<&Value>,
    prebounded_pane_ids: &HashSet<u64>,
) -> std::result::Result<PreparedSnapshotPersistence, SnapshotPreparationError> {
    let metadata_json = metadata.map(canonical_checkpoint_metadata).transpose()?;
    prepare_snapshot_persistence_with_canonical_metadata(
        topology,
        pane_states,
        metadata_json,
        prebounded_pane_ids,
    )
}

fn prepare_snapshot_persistence_with_canonical_metadata(
    topology: &TopologySnapshot,
    pane_states: &[PaneStateSnapshot],
    metadata_json: Option<String>,
    prebounded_pane_ids: &HashSet<u64>,
) -> std::result::Result<PreparedSnapshotPersistence, SnapshotPreparationError> {
    let topology_json = topology.to_persistence_json()?;
    let mut topology_pane_ids = topology.pane_ids();
    topology_pane_ids.sort_unstable();
    let mut state_pane_ids = pane_states
        .iter()
        .map(|pane| pane.pane_id)
        .collect::<Vec<_>>();
    state_pane_ids.sort_unstable();
    if topology_pane_ids != state_pane_ids {
        return Err(SnapshotPreparationError::PaneIdentitySetMismatch {
            topology_panes: topology_pane_ids.len(),
            pane_states: state_pane_ids.len(),
        });
    }
    if metadata_json
        .as_ref()
        .is_some_and(|metadata| metadata.len() > MAX_CHECKPOINT_METADATA_BYTES)
    {
        return Err(SnapshotPreparationError::MetadataResourceLimit {
            bytes: metadata_json.as_ref().map_or(0, String::len),
            limit: MAX_CHECKPOINT_METADATA_BYTES,
        });
    }
    let mut panes = Vec::with_capacity(pane_states.len());
    let mut total_bytes = 0_usize;
    let mut truncated_pane_count = 0_usize;

    for pane in pane_states {
        let (title, title_truncated) =
            bounded_utf8(&pane.terminal.title, SNAPSHOT_TEXT_FIELD_INPUT_BYTES);
        // Construct the fixed-shape terminal projection directly. Cloning the
        // source first would duplicate an unbounded caller-supplied title
        // before the byte cap had a chance to apply.
        let terminal = crate::session_pane_state::TerminalState {
            rows: pane.terminal.rows,
            cols: pane.terminal.cols,
            cursor_row: pane.terminal.cursor_row,
            cursor_col: pane.terminal.cursor_col,
            is_alt_screen: pane.terminal.is_alt_screen,
            title,
        };
        let terminal_state_json = canonical_json_string(&terminal)?;
        let (cwd, cwd_truncated) =
            admitted_optional_utf8(pane.cwd.as_deref(), SNAPSHOT_TEXT_FIELD_INPUT_BYTES);
        let (command, command_truncated) = admitted_optional_utf8(
            pane.foreground_process
                .as_ref()
                .map(|process| process.name.as_str()),
            SNAPSHOT_TEXT_FIELD_INPUT_BYTES,
        );
        let (env_json, env_truncated) = match pane.env.as_ref() {
            Some(env) => {
                let (env, truncated) = bounded_captured_env(env);
                (Some(canonical_json_string(&env)?), truncated)
            }
            None => (None, false),
        };
        let (agent_metadata_json, agent_truncated) = match pane.agent.as_ref() {
            Some(agent) => {
                let (agent, truncated) = bounded_agent_metadata(agent);
                (
                    agent.as_ref().map(canonical_json_string).transpose()?,
                    truncated,
                )
            }
            None => (None, false),
        };
        let scrollback_checkpoint_seq = pane
            .scrollback_ref
            .as_ref()
            .map(|scrollback| {
                if scrollback.output_segments_seq < 0 {
                    Err(SnapshotPreparationError::NegativeScrollbackSequence)
                } else {
                    Ok(scrollback.output_segments_seq)
                }
            })
            .transpose()?;
        let last_output_at = pane
            .scrollback_ref
            .as_ref()
            .map(|scrollback| {
                i64::try_from(scrollback.last_capture_at).map_err(|_| {
                    SnapshotPreparationError::TimestampRange(scrollback.last_capture_at)
                })
            })
            .transpose()?;

        let mut persisted = PersistedPaneState {
            pane_id: i64::try_from(pane.pane_id)
                .map_err(|_| SnapshotPreparationError::PaneIdRange(pane.pane_id))?,
            cwd,
            command,
            env_json,
            terminal_state_json,
            agent_metadata_json,
            scrollback_checkpoint_seq,
            last_output_at,
        };
        let reduced = reduce_persisted_pane_to_budget(&mut persisted)?;
        if prebounded_pane_ids.contains(&pane.pane_id)
            || title_truncated
            || cwd_truncated
            || command_truncated
            || env_truncated
            || agent_truncated
            || reduced
        {
            truncated_pane_count = truncated_pane_count
                .checked_add(1)
                .ok_or(SnapshotPreparationError::PaneCountOverflow)?;
        }

        // Keep the historical database/API byte field stable for existing v2
        // readers and witnesses. The complete admitted projection is tracked
        // separately below.
        let pane_bytes = persisted
            .terminal_state_json
            .len()
            .checked_add(persisted.env_json.as_ref().map_or(0, String::len))
            .and_then(|bytes| {
                bytes.checked_add(
                    persisted
                        .agent_metadata_json
                        .as_ref()
                        .map_or(0, String::len),
                )
            })
            .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
        total_bytes = total_bytes
            .checked_add(pane_bytes)
            .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
        panes.push(persisted);
    }

    panes.sort_unstable_by_key(|pane| pane.pane_id);
    let pane_count = panes.len();
    let pane_count_sql =
        i64::try_from(pane_count).map_err(|_| SnapshotPreparationError::PaneCountOverflow)?;
    let total_bytes_sql =
        i64::try_from(total_bytes).map_err(|_| SnapshotPreparationError::ByteCountOverflow)?;
    let persisted_text_bytes =
        persisted_checkpoint_text_bytes(Some(&topology_json), metadata_json.as_deref(), &panes)
            .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
    if persisted_text_bytes > MAX_PERSISTED_CHECKPOINT_TEXT_BYTES {
        return Err(SnapshotPreparationError::CheckpointTextResourceLimit {
            bytes: persisted_text_bytes,
            limit: MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
        });
    }
    let dedup_hash = snapshot_dedup_witness(&topology_json, &panes)?;

    Ok(PreparedSnapshotPersistence {
        topology_json,
        metadata_json,
        panes,
        pane_count,
        pane_count_sql,
        total_bytes,
        total_bytes_sql,
        persisted_text_bytes,
        truncated_pane_count,
        dedup_hash,
    })
}

/// Test-only structural projection helper retained for focused pane tests.
#[cfg(test)]
fn compute_state_hash(panes: &[PaneInfo]) -> String {
    let (topology, _) = TopologySnapshot::from_panes(panes, 0);
    let pane_states: Vec<PaneStateSnapshot> = panes
        .iter()
        .map(|pane| PaneStateSnapshot::from_pane_info(pane, 0, false))
        .collect();
    prepare_snapshot_persistence(&topology, &pane_states, None)
        .expect("test pane projection should serialize")
        .dedup_hash
}

// =============================================================================
// SQLite operations (sync, run inside spawn_blocking)
// =============================================================================

/// Database failure disposition at the durable snapshot-authority boundary.
/// The retry-safe variant means either no transaction began or an explicit
/// rollback succeeded. A commit error is always indeterminate: SQLite may have
/// crossed the durable boundary even when the caller receives an error.
#[derive(Debug, thiserror::Error)]
enum SnapshotAuthorityDbError {
    #[error("{source}")]
    RetrySafe {
        #[source]
        source: rusqlite::Error,
    },
    #[error("commit outcome is indeterminate: {source}")]
    IndeterminateCommit {
        #[source]
        source: rusqlite::Error,
    },
    #[error("mutation failed ({source}) and rollback acknowledgement failed ({rollback})")]
    IndeterminateRollback {
        source: rusqlite::Error,
        rollback: Box<rusqlite::Error>,
    },
}

impl SnapshotAuthorityDbError {
    fn retry_safe(source: rusqlite::Error) -> Self {
        Self::RetrySafe { source }
    }

    #[cfg(test)]
    fn into_primary_source(self) -> rusqlite::Error {
        match self {
            Self::RetrySafe { source } | Self::IndeterminateCommit { source } => source,
            Self::IndeterminateRollback { source, .. } => source,
        }
    }
}

impl From<rusqlite::Error> for SnapshotAuthorityDbError {
    fn from(source: rusqlite::Error) -> Self {
        Self::retry_safe(source)
    }
}

impl SnapshotAuthorityWorkFailure for SnapshotAuthorityDbError {
    fn requires_reconciliation(&self) -> bool {
        matches!(
            self,
            Self::IndeterminateCommit { .. } | Self::IndeterminateRollback { .. }
        )
    }
}

impl SnapshotAuthorityWorkFailure for crate::session_retention::SessionCleanupError {
    fn requires_reconciliation(&self) -> bool {
        (*self).requires_reconciliation()
    }
}

/// Execute one SQLite transaction with an explicit proof boundary. Work errors
/// are retry-safe only after an acknowledged rollback; commit errors remain
/// indeterminate even if rusqlite's drop path later attempts a rollback.
fn run_snapshot_authority_transaction<T, F>(
    conn: &Connection,
    work: F,
) -> std::result::Result<T, SnapshotAuthorityDbError>
where
    F: FnOnce(&rusqlite::Transaction<'_>) -> std::result::Result<T, rusqlite::Error>,
{
    let tx = conn
        .unchecked_transaction()
        .map_err(SnapshotAuthorityDbError::retry_safe)?;
    match work(&tx) {
        Ok(value) => tx
            .commit()
            .map(|()| value)
            .map_err(|source| SnapshotAuthorityDbError::IndeterminateCommit { source }),
        Err(source) => match tx.rollback() {
            Ok(()) => Err(SnapshotAuthorityDbError::retry_safe(source)),
            Err(rollback) => Err(SnapshotAuthorityDbError::IndeterminateRollback {
                source,
                rollback: Box::new(rollback),
            }),
        },
    }
}

/// Execute an optional authority mutation while preserving a retry-safe,
/// explicitly acknowledged no-op path. Returning `Ok(None)` from `work` is a
/// contract that no statement mutated durable state; the transaction is rolled
/// back instead of crossing a commit boundary for a read-only miss.
fn run_optional_snapshot_authority_transaction<T, F>(
    conn: &Connection,
    work: F,
) -> std::result::Result<Option<T>, SnapshotAuthorityDbError>
where
    F: FnOnce(&rusqlite::Transaction<'_>) -> std::result::Result<Option<T>, rusqlite::Error>,
{
    let tx = conn
        .unchecked_transaction()
        .map_err(SnapshotAuthorityDbError::retry_safe)?;
    match work(&tx) {
        Ok(Some(value)) => tx
            .commit()
            .map(|()| Some(value))
            .map_err(|source| SnapshotAuthorityDbError::IndeterminateCommit { source }),
        Ok(None) => tx
            .rollback()
            .map(|()| None)
            .map_err(SnapshotAuthorityDbError::retry_safe),
        Err(source) => match tx.rollback() {
            Ok(()) => Err(SnapshotAuthorityDbError::retry_safe(source)),
            Err(rollback) => Err(SnapshotAuthorityDbError::IndeterminateRollback {
                source,
                rollback: Box::new(rollback),
            }),
        },
    }
}

fn u64_to_sqlite_integer(value: u64) -> std::result::Result<i64, rusqlite::Error> {
    i64::try_from(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

#[cfg(test)]
fn usize_to_sqlite_integer(value: usize) -> std::result::Result<i64, rusqlite::Error> {
    i64::try_from(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn sqlite_integer_to_u64(column: usize, value: i64) -> std::result::Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn snapshot_integer_overflow(detail: &'static str) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(detail)))
}

fn snapshot_witness_error(error: CheckpointWitnessError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

fn require_exactly_one_changed_row(affected: usize) -> std::result::Result<(), rusqlite::Error> {
    if affected == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::StatementChangedRows(affected))
    }
}

#[cfg(unix)]
const SNAPSHOT_SQLITE_DEFAULT_VFS: &str = "unix";
#[cfg(windows)]
const SNAPSHOT_SQLITE_DEFAULT_VFS: &str = "win32";

fn open_conn(db_path: &str) -> std::result::Result<Connection, rusqlite::Error> {
    #[cfg(any(unix, windows))]
    let conn = Connection::open_with_flags_and_vfs(
        db_path,
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        SNAPSHOT_SQLITE_DEFAULT_VFS,
    )?;
    #[cfg(not(any(unix, windows)))]
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )?;
    // Install the connection-local busy policy before the first potentially
    // write-taking PRAGMA. Otherwise journal-mode negotiation can fail
    // immediately under an active writer even though every later statement is
    // willing to wait.
    conn.busy_timeout(Duration::from_secs(5))?;
    // [ft-rfpk6] Enable foreign_keys so cleanup_sync's DELETE on
    // session_checkpoints cascades to mux_pane_state (schema line 646:
    // `REFERENCES session_checkpoints(id) ON DELETE CASCADE`).
    // Without this, every cleanup run leaves orphan pane-state rows
    // that accumulate forever. Same shape as ft-s4myu / session_restore.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

/// Open a connection for an exact authority observation without issuing a
/// write-capable journal-mode PRAGMA. The cached checkpoint can only exist
/// after schema initialization, so validation must observe rather than repair.
fn open_snapshot_query_conn(db_path: &str) -> std::result::Result<Connection, rusqlite::Error> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
    #[cfg(any(unix, windows))]
    let conn = Connection::open_with_flags_and_vfs(db_path, flags, SNAPSHOT_SQLITE_DEFAULT_VFS)?;
    #[cfg(not(any(unix, windows)))]
    let conn = Connection::open_with_flags(db_path, flags)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// Re-read and recompute the complete persisted v2 snapshot witness. The
/// in-memory dedup cache is only a hint: a row with matching identity columns
/// is not durable authority if its parent, topology, pane projection, or
/// witness has been deleted or rewritten.
fn exact_snapshot_checkpoint_is_verified(
    conn: &Connection,
    identity: &SnapshotCheckpointIdentity,
) -> std::result::Result<bool, rusqlite::Error> {
    if identity.checkpoint_role != CHECKPOINT_ROLE_SNAPSHOT
        || !identity.state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX)
    {
        return Ok(false);
    }
    let checkpoint_at = u64_to_sqlite_integer(identity.checkpoint_at)?;
    let checkpoint_exists = conn
        .query_row(
            "SELECT 1
             FROM session_checkpoints AS checkpoint
             JOIN mux_sessions AS session
               ON session.session_id = checkpoint.session_id
             WHERE checkpoint.id = ?1
               AND checkpoint.session_id = ?2
               AND checkpoint.checkpoint_at = ?3
               AND checkpoint.checkpoint_role = ?4
               AND checkpoint.state_hash = ?5
               AND session.last_checkpoint_at = checkpoint.checkpoint_at
               AND session.topology_json = checkpoint.topology_json
               AND checkpoint.id = (
                   SELECT latest.id
                   FROM session_checkpoints AS latest
                   WHERE latest.session_id = checkpoint.session_id
                   ORDER BY latest.id DESC
                   LIMIT 1
               )",
            rusqlite::params![
                identity.checkpoint_id,
                identity.session_id.as_str(),
                checkpoint_at,
                identity.checkpoint_role.as_str(),
                identity.state_hash.as_str(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if !checkpoint_exists {
        return Ok(false);
    }

    match crate::session_restore::load_checkpoint_by_id_from_conn(conn, identity.checkpoint_id) {
        Ok(Some(checkpoint)) => Ok(checkpoint.checkpoint_id == identity.checkpoint_id
            && checkpoint.session_id.as_str() == identity.session_id.as_str()
            && checkpoint.checkpoint_at == identity.checkpoint_at
            && checkpoint.checkpoint_role == crate::session_restore::CheckpointRole::Snapshot
            && checkpoint.state_hash.as_str() == identity.state_hash.as_str()
            && checkpoint.verification
                == crate::session_restore::CheckpointVerification::VerifiedV2),
        Ok(None) => Ok(false),
        Err(error) => {
            tracing::warn!(
                checkpoint_id = identity.checkpoint_id,
                error = %error,
                "snapshot checkpoint authority failed bounded verification"
            );
            Ok(false)
        }
    }
}

fn exact_snapshot_checkpoint_exists_sync(
    db_path: &str,
    identity: &SnapshotCheckpointIdentity,
) -> std::result::Result<bool, rusqlite::Error> {
    let conn = open_snapshot_query_conn(db_path)?;
    // The identity/session-summary preflight and bounded witness loader are
    // separate statements. Pin them to one read snapshot so another process
    // cannot rewrite the session summary between those observations and turn
    // a stale cache hint into an authoritative dedup skip.
    let tx = conn.unchecked_transaction()?;
    let verified = exact_snapshot_checkpoint_is_verified(&tx, identity)?;
    tx.commit()?;
    Ok(verified)
}

#[cfg(test)]
fn bounded_host_id(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(bounded_utf8(trimmed, SNAPSHOT_HOST_ID_INPUT_BYTES).0)
}

#[cfg(test)]
fn current_host_id() -> Option<String> {
    bounded_host_id(
        crate::session_retention::current_session_owner_identity().map(|identity| identity.host_id),
    )
}

/// Creation-only fields inserted alongside a first checkpoint. Keeping this
/// separate from the checkpoint arguments makes it impossible for an existing
/// session capture to accidentally rewrite creation authority.
struct NewSessionMetadata {
    ft_version: String,
    host_id: Option<String>,
}

impl std::fmt::Debug for NewSessionMetadata {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NewSessionMetadata")
            .field("has_ft_version", &!self.ft_version.is_empty())
            .field("has_host_id", &self.host_id.is_some())
            .finish_non_exhaustive()
    }
}

/// Authoritative result published only after the SQLite transaction commits.
struct CheckpointCommitReceipt {
    session_id: String,
    checkpoint_id: i64,
    state_hash: String,
    total_bytes: usize,
    persisted_text_bytes: usize,
    truncated_pane_count: usize,
}

impl std::fmt::Debug for CheckpointCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CheckpointCommitReceipt")
            .field("checkpoint_id", &self.checkpoint_id)
            .field("total_bytes", &self.total_bytes)
            .field("persisted_text_bytes", &self.persisted_text_bytes)
            .field("truncated_pane_count", &self.truncated_pane_count)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
fn create_session_sync(
    db_path: &str,
    session_id: &str,
    now_ms: u64,
    topology_json: &str,
    ft_version: &str,
) -> std::result::Result<(), rusqlite::Error> {
    let now_ms = u64_to_sqlite_integer(now_ms)?;
    let conn = open_conn(db_path)?;
    let host_id = current_host_id();
    let tx = conn.unchecked_transaction()?;
    crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    let inserted = tx.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, host_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session_id,
            now_ms,
            topology_json,
            ft_version,
            host_id.as_deref()
        ],
    )?;
    require_exactly_one_changed_row(inserted)?;
    tx.commit()
}

fn mark_shutdown_authoritatively_sync(
    db_path: &str,
    session_id: &str,
    checkpoint_id: i64,
    checkpoint_at: u64,
    state_hash: &str,
    owner_identity: Option<&crate::session_retention::SessionOwnerIdentity>,
) -> std::result::Result<(), SnapshotAuthorityDbError> {
    let identity = SnapshotCheckpointIdentity {
        checkpoint_id,
        session_id: session_id.to_string(),
        checkpoint_at,
        checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
        state_hash: state_hash.to_string(),
    };
    let checkpoint_at = u64_to_sqlite_integer(checkpoint_at)?;
    let conn = open_conn(db_path).map_err(SnapshotAuthorityDbError::retry_safe)?;
    run_snapshot_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        if !exact_snapshot_checkpoint_is_verified(tx, &identity)? {
            return Err(rusqlite::Error::StatementChangedRows(0));
        }
        let updated = tx.execute(
            "UPDATE mux_sessions AS session
             SET shutdown_clean = 1,
                 clean_checkpoint_id = ?3
             WHERE session.session_id = ?1
               AND session.last_checkpoint_at = ?2
               AND session.topology_json = (
                   SELECT exact.topology_json
                   FROM session_checkpoints AS exact
                   WHERE exact.id = ?3
                     AND exact.session_id = ?1
               )
               AND EXISTS (
                   SELECT 1
                   FROM session_checkpoints AS exact
                   WHERE exact.id = ?3
                     AND exact.session_id = ?1
                     AND exact.checkpoint_at = ?2
                     AND exact.checkpoint_role = 'snapshot'
                     AND exact.state_hash = ?4
               )
               AND ?3 = (
                   SELECT latest.id
                   FROM session_checkpoints AS latest
                   WHERE latest.session_id = ?1
                   ORDER BY latest.id DESC
                   LIMIT 1
               )
               AND (
                   (?5 IS NULL AND session.owner_pid IS NULL
                       AND session.owner_process_start IS NULL
                       AND session.owner_heartbeat_at IS NULL)
                   OR (session.host_id = ?5
                       AND session.owner_pid = ?6
                       AND session.owner_process_start = ?7)
               )",
            rusqlite::params![
                session_id,
                checkpoint_at,
                checkpoint_id,
                state_hash,
                owner_identity.map(|identity| identity.host_id.as_str()),
                owner_identity.map(|identity| identity.pid),
                owner_identity.map(|identity| identity.process_start),
            ],
        )?;
        require_exactly_one_changed_row(updated)
    })
}

#[cfg(test)]
fn mark_shutdown_sync(
    db_path: &str,
    session_id: &str,
    checkpoint_id: i64,
    checkpoint_at: u64,
    state_hash: &str,
) -> std::result::Result<(), rusqlite::Error> {
    mark_shutdown_authoritatively_sync(
        db_path,
        session_id,
        checkpoint_id,
        checkpoint_at,
        state_hash,
        None,
    )
    .map_err(SnapshotAuthorityDbError::into_primary_source)
}

/// Save a checkpoint with all pane states in a single transaction. When
/// `new_session` is present, session creation is part of that same transaction.
/// Returns an authoritative commit receipt.
#[allow(clippy::too_many_arguments)] // One flattened argument set defines the SQLite transaction.
fn save_checkpoint_authoritatively_sync(
    db_path: &str,
    session_id: &str,
    now_ms: u64,
    checkpoint_type: &str,
    prepared: &PreparedSnapshotPersistence,
    new_session: Option<&NewSessionMetadata>,
    owner_identity: Option<&crate::session_retention::SessionOwnerIdentity>,
) -> std::result::Result<CheckpointCommitReceipt, SnapshotAuthorityDbError> {
    let now_ms = u64_to_sqlite_integer(now_ms)?;
    let conn = open_conn(db_path)?;

    run_snapshot_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let total_changes_before: i64 =
            tx.query_row("SELECT total_changes()", [], |row| row.get(0))?;

        // Derive the exact v44 trigger contribution from the same transaction
        // snapshot before any source-row mutation can dirty the usability row.
        // A checkpoint against a reconciled (`state <> 'dirty'`) existing
        // session advances the selection generation and dirties that row (two
        // writes). Reopening a clean or acknowledged session then advances the
        // generation once more when the final mux_sessions update clears those
        // fields. An absent canonical usability row fails closed here.
        let recovery_usability_trigger_changes = if new_session.is_some() {
            2_i64
        } else {
            tx.query_row(
                "SELECT
                     CASE WHEN usability.state <> 'dirty' THEN 2 ELSE 0 END
                     + CASE
                         WHEN session.shutdown_clean IS NOT 0
                           OR session.recovery_acknowledged_at IS NOT NULL
                         THEN 1 ELSE 0
                       END
                 FROM mux_sessions AS session
                 INNER JOIN session_recovery_usability AS usability
                    ON usability.session_id = session.session_id
                 WHERE session.session_id = ?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )?
        };

        if let Some(metadata) = new_session {
            let inserted_session = tx.execute(
                "INSERT INTO mux_sessions
                 (session_id, created_at, last_checkpoint_at, topology_json, ft_version, host_id,
                  owner_pid, owner_process_start, owner_heartbeat_at)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    session_id,
                    now_ms,
                    prepared.topology_json.as_str(),
                    metadata.ft_version.as_str(),
                    owner_identity
                        .map(|identity| identity.host_id.as_str())
                        .or(metadata.host_id.as_deref()),
                    owner_identity.map(|identity| identity.pid),
                    owner_identity.map(|identity| identity.process_start),
                    owner_identity.map(|_| now_ms),
                ],
            )?;
            require_exactly_one_changed_row(inserted_session)?;
        }

        // Insert checkpoint
        let inserted_checkpoint = tx.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role, topology_json)
             VALUES (?1, ?2, ?3, 'pending:snp2', ?4, ?5, ?6, 'snapshot', ?7)",
            rusqlite::params![
                session_id,
                now_ms,
                checkpoint_type,
                prepared.pane_count_sql,
                prepared.total_bytes_sql,
                prepared.metadata_json.as_deref(),
                prepared.topology_json.as_str(),
            ],
        )?;
        require_exactly_one_changed_row(inserted_checkpoint)?;

        let checkpoint_id = tx.last_insert_rowid();
        let state_hash = checkpoint_witness(
            CHECKPOINT_ROLE_SNAPSHOT,
            session_id,
            checkpoint_id,
            now_ms,
            checkpoint_type,
            prepared.pane_count_sql,
            prepared.total_bytes_sql,
            prepared.metadata_json.as_deref(),
            Some(prepared.topology_json.as_str()),
            &prepared.panes,
        )
        .map_err(snapshot_witness_error)?;
        let updated_witness = tx.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            rusqlite::params![state_hash.as_str(), checkpoint_id],
        )?;
        require_exactly_one_changed_row(updated_witness)?;

        // Insert per-pane states
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO mux_pane_state
                 (checkpoint_id, pane_id, cwd, command, env_json, terminal_state_json,
                 agent_metadata_json, scrollback_checkpoint_seq, last_output_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;

            for pane in &prepared.panes {
                let inserted_pane = stmt.execute(rusqlite::params![
                    checkpoint_id,
                    pane.pane_id,
                    pane.cwd.as_deref(),
                    pane.command.as_deref(),
                    pane.env_json.as_deref(),
                    pane.terminal_state_json.as_str(),
                    pane.agent_metadata_json.as_deref(),
                    pane.scrollback_checkpoint_seq,
                    pane.last_output_at,
                ])?;
                require_exactly_one_changed_row(inserted_pane)?;
            }
        } // drop stmt before commit

        if new_session.is_none() {
            let updated_session = tx.execute(
                "UPDATE mux_sessions
                 SET last_checkpoint_at = ?1,
                     topology_json = ?2,
                     shutdown_clean = 0,
                     clean_checkpoint_id = NULL,
                     owner_heartbeat_at = ?7,
                     recovery_acknowledged_at = NULL
                 WHERE session_id = ?3
                   AND (
                       (?4 IS NULL AND owner_pid IS NULL
                           AND owner_process_start IS NULL
                           AND owner_heartbeat_at IS NULL)
                       OR (host_id = ?4
                           AND owner_pid = ?5
                           AND owner_process_start = ?6)
                   )",
                rusqlite::params![
                    now_ms,
                    prepared.topology_json.as_str(),
                    session_id,
                    owner_identity.map(|identity| identity.host_id.as_str()),
                    owner_identity.map(|identity| identity.pid),
                    owner_identity.map(|identity| identity.process_start),
                    owner_identity.map(|_| now_ms),
                ],
            )?;
            require_exactly_one_changed_row(updated_session)?;
        }

        // Direct execute counts deliberately exclude trigger side effects, while
        // SQLite's connection-wide total_changes() includes them. Schema v40
        // performs exactly one canonical retained-size summary write for every
        // session/checkpoint/pane source-row mutation below, so the base DML
        // witness is twice the source-row count. The canonical v44 contribution
        // was captured above: two writes for session creation or for dirtying a
        // reconciled existing session, plus one when reopening a clean or
        // acknowledged session. The trigger allowlist above and current-schema
        // exact-body validation make both components authoritative.
        // Avoid re-reading every just-written JSON payload here: that doubled
        // large-session I/O and allocation while the SQLite writer lock was held.
        let total_changes_after: i64 =
            tx.query_row("SELECT total_changes()", [], |row| row.get(0))?;
        let expected_changes = prepared
            .pane_count_sql
            .checked_add(3)
            .and_then(|source_changes| source_changes.checked_mul(2))
            .and_then(|base_changes| base_changes.checked_add(recovery_usability_trigger_changes))
            .ok_or_else(|| snapshot_integer_overflow("snapshot expected DML count overflow"))?;
        if total_changes_after.checked_sub(total_changes_before) != Some(expected_changes) {
            return Err(snapshot_integer_overflow(
                "snapshot transaction performed unexpected DML",
            ));
        }

        Ok(CheckpointCommitReceipt {
            session_id: session_id.to_string(),
            checkpoint_id,
            state_hash,
            total_bytes: prepared.total_bytes,
            persisted_text_bytes: prepared.persisted_text_bytes,
            truncated_pane_count: prepared.truncated_pane_count,
        })
    })
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn save_checkpoint_sync(
    db_path: &str,
    session_id: &str,
    now_ms: u64,
    checkpoint_type: &str,
    prepared: &PreparedSnapshotPersistence,
    new_session: Option<&NewSessionMetadata>,
) -> std::result::Result<CheckpointCommitReceipt, rusqlite::Error> {
    save_checkpoint_authoritatively_sync(
        db_path,
        session_id,
        now_ms,
        checkpoint_type,
        prepared,
        new_session,
        None,
    )
    .map_err(SnapshotAuthorityDbError::into_primary_source)
}

/// Recompute the mutable session summary after checkpoint rows were removed.
/// Clean state remains true only while its exact receipt row still exists,
/// belongs to this session, and is still the deterministic latest checkpoint.
fn reconcile_session_after_checkpoint_deletion(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    force_unclean: bool,
) -> std::result::Result<(), rusqlite::Error> {
    let updated = tx.execute(
        "UPDATE mux_sessions AS session
         SET last_checkpoint_at = (
                 SELECT checkpoint.checkpoint_at
                 FROM session_checkpoints AS checkpoint
                 WHERE checkpoint.session_id = session.session_id
                 ORDER BY checkpoint.id DESC
                 LIMIT 1
             ),
             topology_json = COALESCE(
                 (
                     SELECT checkpoint.topology_json
                     FROM session_checkpoints AS checkpoint
                     WHERE checkpoint.session_id = session.session_id
                       AND checkpoint.checkpoint_role = 'snapshot'
                       AND checkpoint.topology_json IS NOT NULL
                     ORDER BY checkpoint.id DESC
                     LIMIT 1
                 ),
                 session.topology_json
             ),
             shutdown_clean = CASE
                 WHEN ?2 = 0
                  AND session.shutdown_clean = 1
                  AND EXISTS (
                      SELECT 1
                      FROM session_checkpoints AS clean
                      WHERE clean.id = session.clean_checkpoint_id
                        AND clean.session_id = session.session_id
                        AND clean.id = (
                            SELECT latest.id
                            FROM session_checkpoints AS latest
                            WHERE latest.session_id = session.session_id
                            ORDER BY latest.id DESC
                            LIMIT 1
                        )
                  )
                 THEN 1
                 ELSE 0
             END,
             clean_checkpoint_id = CASE
                 WHEN ?2 = 0
                  AND session.shutdown_clean = 1
                  AND EXISTS (
                      SELECT 1
                      FROM session_checkpoints AS clean
                      WHERE clean.id = session.clean_checkpoint_id
                        AND clean.session_id = session.session_id
                        AND clean.id = (
                            SELECT latest.id
                            FROM session_checkpoints AS latest
                            WHERE latest.session_id = session.session_id
                            ORDER BY latest.id DESC
                            LIMIT 1
                        )
                  )
                 THEN session.clean_checkpoint_id
                 ELSE NULL
             END
         WHERE session.session_id = ?1",
        rusqlite::params![session_id, force_unclean],
    )?;
    require_exactly_one_changed_row(updated)
}

type CheckpointDeleteRow = (i64, String, i64, String, String, i64, i64, i64, i64, i64);

fn decode_checkpoint_delete_row(
    row: &rusqlite::Row<'_>,
) -> std::result::Result<CheckpointDeleteRow, rusqlite::Error> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn delete_checkpoint_authoritatively_sync(
    db_path: &str,
    target: &SnapshotDeleteTarget,
) -> std::result::Result<Option<SnapshotDeleteResult>, SnapshotAuthorityDbError> {
    let conn = open_conn(db_path)?;
    run_optional_snapshot_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let projection = format!(
            "SELECT checkpoint.id,
                                 CASE
                                     WHEN typeof(checkpoint.session_id) = 'text'
                                      AND length(CAST(checkpoint.session_id AS BLOB))
                                          BETWEEN 1 AND {MAX_CHECKPOINT_SESSION_ID_BYTES}
                                     THEN checkpoint.session_id
                                 END,
                                 checkpoint.checkpoint_at,
                                 CASE
                                     WHEN typeof(checkpoint.checkpoint_role) = 'text'
                                      AND length(CAST(checkpoint.checkpoint_role AS BLOB))
                                          BETWEEN 1 AND {MAX_CHECKPOINT_ROLE_BYTES}
                                     THEN checkpoint.checkpoint_role
                                 END,
                                 CASE
                                     WHEN typeof(checkpoint.state_hash) = 'text'
                                      AND length(CAST(checkpoint.state_hash AS BLOB))
                                          BETWEEN 1 AND {MAX_CHECKPOINT_STATE_HASH_BYTES}
                                     THEN checkpoint.state_hash
                                 END,
                                 checkpoint.total_bytes,
                                 COALESCE(
                                     session.shutdown_clean = 1
                                     AND session.clean_checkpoint_id = checkpoint.id,
                                     0
                                 ),
                                 COALESCE(
                                     checkpoint.id = (
                                         SELECT latest.id
                                         FROM session_checkpoints AS latest
                                         WHERE latest.session_id = checkpoint.session_id
                                         ORDER BY latest.id DESC
                                         LIMIT 1
                                     ),
                                     0
                                 ),
                                 COALESCE(
                                     checkpoint.checkpoint_role = 'snapshot'
                                     AND checkpoint.id = (
                                         SELECT latest_snapshot.id
                                         FROM session_checkpoints AS latest_snapshot
                                         WHERE latest_snapshot.session_id = checkpoint.session_id
                                           AND latest_snapshot.checkpoint_role = 'snapshot'
                                         ORDER BY latest_snapshot.id DESC
                                         LIMIT 1
                                     ),
                                     0
                                 ),
                                 COALESCE(session.shutdown_clean = 1, 0)
                          FROM session_checkpoints AS checkpoint
                          JOIN mux_sessions AS session
                            ON session.session_id = checkpoint.session_id"
        );
        let checkpoint = match target {
            SnapshotDeleteTarget::Id(checkpoint_id) => tx
                .query_row(
                    &format!("{projection} WHERE checkpoint.id = ?1"),
                    [checkpoint_id],
                    decode_checkpoint_delete_row,
                )
                .optional()?,
            SnapshotDeleteTarget::Exact(identity) => {
                let checkpoint_at = u64_to_sqlite_integer(identity.checkpoint_at)?;
                tx.query_row(
                    &format!(
                        "{projection}
                         WHERE checkpoint.id = ?1
                           AND checkpoint.session_id = ?2
                           AND checkpoint.checkpoint_at = ?3
                           AND checkpoint.checkpoint_role = ?4
                           AND checkpoint.state_hash = ?5"
                    ),
                    rusqlite::params![
                        identity.checkpoint_id,
                        identity.session_id.as_str(),
                        checkpoint_at,
                        identity.checkpoint_role.as_str(),
                        identity.state_hash.as_str(),
                    ],
                    decode_checkpoint_delete_row,
                )
                .optional()?
            }
            SnapshotDeleteTarget::Latest(scope) => {
                let role_predicate = match scope {
                    SnapshotCheckpointRoleScope::Snapshot => {
                        " WHERE checkpoint.checkpoint_role = 'snapshot'"
                    }
                    SnapshotCheckpointRoleScope::Any => "",
                };
                tx.query_row(
                    &format!(
                        "{projection}{role_predicate}
                         ORDER BY checkpoint.id DESC
                         LIMIT 1"
                    ),
                    [],
                    decode_checkpoint_delete_row,
                )
                .optional()?
            }
        };
        let Some((
            checkpoint_id,
            session_id,
            checkpoint_at,
            checkpoint_role,
            state_hash,
            recorded_payload_bytes,
            was_clean_receipt,
            was_latest_checkpoint,
            was_latest_snapshot,
            session_was_clean,
        )) = checkpoint
        else {
            return Ok(None);
        };
        let checkpoint_at = sqlite_integer_to_u64(2, checkpoint_at)?;
        let recorded_payload_bytes = sqlite_integer_to_u64(5, recorded_payload_bytes)?;

        let protects_unresolved_restore = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM restore_attempt_lifecycle
                 WHERE source_checkpoint_id = ?1
                   AND CASE
                           WHEN typeof(status) = 'text' AND status = 'resolved' THEN 0
                           ELSE 1
                       END = 1
             )",
            [checkpoint_id],
            |row| row.get::<_, i64>(0),
        )?;
        if protects_unresolved_restore != 0 {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::other("checkpoint is protected by an unresolved restore attempt"),
            )));
        }

        let deleted = tx.execute(
            "DELETE FROM session_checkpoints WHERE id = ?1",
            [checkpoint_id],
        )?;
        require_exactly_one_changed_row(deleted)?;
        let force_unclean = was_latest_checkpoint != 0 || was_latest_snapshot != 0;
        reconcile_session_after_checkpoint_deletion(tx, &session_id, force_unclean)?;

        Ok(Some(SnapshotDeleteResult {
            identity: SnapshotCheckpointIdentity {
                checkpoint_id,
                session_id,
                checkpoint_at,
                checkpoint_role,
                state_hash,
            },
            recorded_payload_bytes,
            invalidated_clean_state: was_clean_receipt != 0
                || (force_unclean && session_was_clean != 0),
        }))
    })
}

/// Remove checkpoints exceeding retention limits.
/// Returns the number of checkpoints deleted.
fn cleanup_authoritatively_sync(
    db_path: &str,
    retention_count: usize,
    retention_days: u64,
) -> std::result::Result<usize, SnapshotAuthorityDbError> {
    let conn = open_conn(db_path)?;
    let cutoff_ms = epoch_ms().saturating_sub(retention_days.saturating_mul(86_400_000));
    let cutoff_ms = u64_to_sqlite_integer(cutoff_ms)?;
    // A count above SQLite's signed integer range means "retain everything"
    // for any representable database, so clamp rather than wrapping negative
    // or turning a safe no-op policy into a permanent cleanup failure.
    let retention_count = i64::try_from(retention_count).unwrap_or(i64::MAX);

    // Wrap both DELETEs in a transaction so a concurrent checkpoint insert
    // between them cannot be incorrectly deleted by the second statement.
    run_snapshot_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;

        let affected_sql = format!(
            "WITH ranked AS (
                 SELECT id,
                        session_id,
                        checkpoint_at,
                        ROW_NUMBER() OVER (
                            PARTITION BY session_id
                            ORDER BY id DESC
                        ) AS checkpoint_rank
                 FROM session_checkpoints
                 WHERE checkpoint_role = 'snapshot'
             )
             SELECT CASE
                        WHEN typeof(session_id) = 'text'
                         AND length(CAST(session_id AS BLOB))
                             BETWEEN 1 AND {MAX_CHECKPOINT_SESSION_ID_BYTES}
                        THEN session_id
                    END,
                    MAX(
                        CASE
                            WHEN checkpoint_rank = 1
                             AND (checkpoint_at < ?1 OR checkpoint_rank > ?2)
                            THEN 1
                            ELSE 0
                        END
                    ) AS deletes_latest_snapshot
             FROM ranked
             WHERE (checkpoint_at < ?1 OR checkpoint_rank > ?2)
               AND id NOT IN (
                   SELECT source_checkpoint_id
                   FROM restore_attempt_lifecycle
                   WHERE CASE
                           WHEN typeof(status) = 'text' AND status = 'resolved' THEN 0
                           ELSE 1
                         END = 1
               )
             GROUP BY session_id"
        );
        let mut affected_stmt = tx.prepare(&affected_sql)?;
        let affected_sessions = affected_stmt
            .query_map(rusqlite::params![cutoff_ms, retention_count], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(affected_stmt);

        // Delete checkpoints older than retention_days
        let deleted_by_age: usize = tx.execute(
            "DELETE FROM session_checkpoints
             WHERE checkpoint_role = 'snapshot'
               AND checkpoint_at < ?1
               AND id NOT IN (
                   SELECT source_checkpoint_id
                   FROM restore_attempt_lifecycle
                   WHERE CASE
                           WHEN typeof(status) = 'text' AND status = 'resolved' THEN 0
                           ELSE 1
                         END = 1
               )",
            [cutoff_ms],
        )?;

        // Keep only the latest retention_count checkpoints per session.
        // The ranking must be partitioned by session_id; a global LIMIT would let
        // one busy session evict another session's newest retained checkpoints.
        let deleted_by_count: usize = tx.execute(
            "DELETE FROM session_checkpoints
             WHERE id IN (
                 SELECT id
                 FROM (
                     SELECT id,
                            ROW_NUMBER() OVER (
                                PARTITION BY session_id
                                ORDER BY id DESC
                            ) AS checkpoint_rank
                     FROM session_checkpoints
                     WHERE checkpoint_role = 'snapshot'
                 )
                 WHERE checkpoint_rank > ?1
             )
               AND id NOT IN (
                   SELECT source_checkpoint_id
                   FROM restore_attempt_lifecycle
                   WHERE CASE
                           WHEN typeof(status) = 'text' AND status = 'resolved' THEN 0
                           ELSE 1
                         END = 1
               )",
            [retention_count],
        )?;

        for (session_id, force_unclean) in affected_sessions {
            reconcile_session_after_checkpoint_deletion(tx, &session_id, force_unclean)?;
        }

        deleted_by_age
            .checked_add(deleted_by_count)
            .ok_or_else(|| snapshot_integer_overflow("snapshot cleanup deletion count overflow"))
    })
}

// =============================================================================
// Verified checkpoint + durable scrollback artifacts
// =============================================================================

/// Frozen schema identifier for an offline checkpoint/scrollback artifact.
pub const CHECKPOINT_SCROLLBACK_ARTIFACT_SCHEMA: &str =
    "frankenterm.checkpoint-scrollback-artifact.v1";

/// Canonical suffix used by the bounded artifact inventory.
pub const CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX: &str = ".ft-checkpoint-scrollback.json";

/// Hard ceiling for one artifact, independent of caller-provided limits.
pub const CHECKPOINT_SCROLLBACK_ARTIFACT_HARD_MAX_BYTES: u64 = 384 * 1024 * 1024;
/// Hard ceiling for exported segment content across one artifact.
pub const CHECKPOINT_SCROLLBACK_CONTENT_HARD_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Hard ceiling for durable output rows across one artifact.
pub const CHECKPOINT_SCROLLBACK_HARD_MAX_SEGMENTS: usize = 1_000_000;
/// Hard ceiling for explicit capture-gap rows across one artifact.
pub const CHECKPOINT_SCROLLBACK_HARD_MAX_GAPS: usize = 262_144;
/// Maximum byte length admitted for one redaction catalog identity.
const CHECKPOINT_SCROLLBACK_MAX_CATALOG_VERSION_BYTES: usize = 256;
/// Maximum byte length admitted for one explicit capture-gap reason.
const CHECKPOINT_SCROLLBACK_MAX_GAP_REASON_BYTES: usize = 16 * 1024;
/// One stable lock inode serializes publication within an artifact directory.
const CHECKPOINT_SCROLLBACK_PUBLICATION_LOCK: &str = ".ft-checkpoint-scrollback-publication.lock";
/// Publication lock acquisition is finite so a wedged peer cannot hang export.
const CHECKPOINT_SCROLLBACK_PUBLICATION_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// Short polling interval for the cross-process publication lock.
const CHECKPOINT_SCROLLBACK_PUBLICATION_LOCK_POLL: Duration = Duration::from_millis(10);

/// Resource contract used by production and offline artifact paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackArtifactLimits {
    /// Maximum checkpoint panes admitted to the artifact.
    pub max_panes: usize,
    /// Maximum durable output rows admitted for any one pane.
    pub max_segments_per_pane: usize,
    /// Maximum durable output rows admitted across the artifact.
    pub max_total_segments: usize,
    /// Maximum explicit capture-gap rows admitted for any one pane.
    pub max_gaps_per_pane: usize,
    /// Maximum explicit capture-gap rows admitted across the artifact.
    pub max_total_gaps: usize,
    /// Maximum UTF-8 content bytes admitted across all output rows.
    pub max_content_bytes: u64,
    /// Maximum canonical JSON artifact bytes admitted on write or read.
    pub max_artifact_bytes: u64,
    /// Maximum directory entries admitted by one inventory operation.
    pub max_inventory_entries: usize,
}

impl Default for CheckpointScrollbackArtifactLimits {
    fn default() -> Self {
        Self {
            max_panes: MAX_TOPOLOGY_PANES,
            max_segments_per_pane: 250_000,
            max_total_segments: CHECKPOINT_SCROLLBACK_HARD_MAX_SEGMENTS,
            max_gaps_per_pane: 65_536,
            max_total_gaps: CHECKPOINT_SCROLLBACK_HARD_MAX_GAPS,
            max_content_bytes: CHECKPOINT_SCROLLBACK_CONTENT_HARD_MAX_BYTES,
            max_artifact_bytes: CHECKPOINT_SCROLLBACK_ARTIFACT_HARD_MAX_BYTES,
            max_inventory_entries: 4_096,
        }
    }
}

impl CheckpointScrollbackArtifactLimits {
    fn validate(self) -> Result<Self, CheckpointScrollbackArtifactError> {
        let nonzero = [
            ("max_panes", self.max_panes),
            ("max_segments_per_pane", self.max_segments_per_pane),
            ("max_total_segments", self.max_total_segments),
            ("max_gaps_per_pane", self.max_gaps_per_pane),
            ("max_total_gaps", self.max_total_gaps),
            ("max_inventory_entries", self.max_inventory_entries),
        ];
        if let Some((field, _)) = nonzero.into_iter().find(|(_, value)| *value == 0) {
            return Err(CheckpointScrollbackArtifactError::InvalidLimits(format!(
                "{field} must be non-zero"
            )));
        }
        if self.max_content_bytes == 0 || self.max_artifact_bytes == 0 {
            return Err(CheckpointScrollbackArtifactError::InvalidLimits(
                "byte limits must be non-zero".to_string(),
            ));
        }
        if self.max_panes > MAX_TOPOLOGY_PANES {
            return Err(CheckpointScrollbackArtifactError::InvalidLimits(format!(
                "max_panes exceeds the topology ceiling of {MAX_TOPOLOGY_PANES}"
            )));
        }
        if self.max_segments_per_pane > CHECKPOINT_SCROLLBACK_HARD_MAX_SEGMENTS
            || self.max_total_segments > CHECKPOINT_SCROLLBACK_HARD_MAX_SEGMENTS
            || self.max_segments_per_pane > self.max_total_segments
        {
            return Err(CheckpointScrollbackArtifactError::InvalidLimits(
                "segment limits exceed the hard ceiling or conflict".to_string(),
            ));
        }
        if self.max_gaps_per_pane > CHECKPOINT_SCROLLBACK_HARD_MAX_GAPS
            || self.max_total_gaps > CHECKPOINT_SCROLLBACK_HARD_MAX_GAPS
            || self.max_gaps_per_pane > self.max_total_gaps
        {
            return Err(CheckpointScrollbackArtifactError::InvalidLimits(
                "gap limits exceed the hard ceiling or conflict".to_string(),
            ));
        }
        if self.max_content_bytes > CHECKPOINT_SCROLLBACK_CONTENT_HARD_MAX_BYTES
            || self.max_artifact_bytes > CHECKPOINT_SCROLLBACK_ARTIFACT_HARD_MAX_BYTES
            || self.max_content_bytes > self.max_artifact_bytes
        {
            return Err(CheckpointScrollbackArtifactError::InvalidLimits(
                "artifact byte limits exceed the hard ceiling or conflict".to_string(),
            ));
        }
        Ok(self)
    }

    fn admits(&self, embedded: &Self) -> bool {
        embedded.max_panes <= self.max_panes
            && embedded.max_segments_per_pane <= self.max_segments_per_pane
            && embedded.max_total_segments <= self.max_total_segments
            && embedded.max_gaps_per_pane <= self.max_gaps_per_pane
            && embedded.max_total_gaps <= self.max_total_gaps
            && embedded.max_content_bytes <= self.max_content_bytes
            && embedded.max_artifact_bytes <= self.max_artifact_bytes
            && embedded.max_inventory_entries <= self.max_inventory_entries
    }
}

/// Errors from artifact construction, publication, inventory, and verification.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointScrollbackArtifactError {
    /// A caller supplied a contradictory or excessive resource contract.
    #[error("invalid checkpoint scrollback artifact limits: {0}")]
    InvalidLimits(String),
    /// SQLite authority could not be read consistently.
    #[error("checkpoint scrollback database observation failed: {0}")]
    Database(#[from] rusqlite::Error),
    /// The selected checkpoint was absent or did not carry verified v2 authority.
    #[error("checkpoint scrollback source is not a verified snapshot: {0}")]
    Checkpoint(String),
    /// A source or artifact exceeded a declared resource bound.
    #[error("checkpoint scrollback resource limit exceeded: {0}")]
    ResourceLimit(String),
    /// Persisted source bytes were not a fixed point under the active redactor.
    #[error("checkpoint scrollback source is not a current redaction fixed point")]
    RedactionNotFixedPoint,
    /// A filesystem operation failed.
    #[error("checkpoint scrollback filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization or decoding failed.
    #[error("checkpoint scrollback JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    /// Offline semantic verification rejected the artifact.
    #[error("invalid checkpoint scrollback artifact: {0}")]
    InvalidArtifact(String),
    /// A verified or prepared artifact belongs to a different snapshot result.
    #[error("checkpoint scrollback artifact identity does not match the requested snapshot result")]
    CheckpointIdentityMismatch,
    /// No-clobber publication found an existing conflicting target.
    #[error("checkpoint scrollback target already exists")]
    AlreadyExists,
    /// Deterministic staging evidence belongs to a different payload.
    #[error("checkpoint scrollback staging artifact conflicts with the requested publication")]
    StagingConflict,
    /// Another producer retained the directory-wide publication lock too long.
    #[error("checkpoint scrollback publication lock acquisition timed out")]
    PublicationBusy,
}

/// Exact source columns required to recompute a v2 checkpoint witness offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackPaneProjection {
    /// Pane identifier stored in `mux_pane_state`.
    pub pane_id: u64,
    /// Captured working directory.
    pub cwd: Option<String>,
    /// Captured foreground command.
    pub command: Option<String>,
    /// Canonical curated environment JSON.
    pub env_json: Option<String>,
    /// Canonical terminal-state JSON.
    pub terminal_state_json: String,
    /// Canonical agent metadata JSON.
    pub agent_metadata_json: Option<String>,
    /// Highest output segment visible to the checkpoint.
    pub scrollback_checkpoint_seq: Option<u64>,
    /// Timestamp of the checkpoint's latest retained output.
    pub last_output_at: Option<u64>,
}

/// Exact verified snapshot identity and projection embedded in the artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackCheckpoint {
    /// SQLite checkpoint row ID.
    pub checkpoint_id: i64,
    /// Parent mux session identity.
    pub session_id: String,
    /// Checkpoint timestamp in epoch milliseconds.
    pub checkpoint_at: u64,
    /// Snapshot trigger/type persisted with the checkpoint.
    pub checkpoint_type: String,
    /// Frozen role; v1 accepts only `snapshot`.
    pub checkpoint_role: String,
    /// Recomputed `snp2:` witness over this exact embedded projection.
    pub state_hash: String,
    /// Optional exact checkpoint metadata JSON.
    pub metadata_json: Option<String>,
    /// Exact topology JSON covered by `state_hash`.
    pub topology_json: String,
    /// SHA-256 of the exact topology JSON bytes.
    pub topology_sha256: String,
    /// Declared and embedded pane count.
    pub pane_count: usize,
    /// Historical checkpoint payload byte estimate.
    pub total_bytes: usize,
    /// Exact persisted pane projections, sorted by pane ID.
    pub panes: Vec<CheckpointScrollbackPaneProjection>,
}

/// One durable output row retained in a pane prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackSegment {
    /// Per-pane monotonic output sequence.
    pub seq: u64,
    /// Capture timestamp in epoch milliseconds.
    pub captured_at: u64,
    /// Redaction catalog stamped on the source row, or `None` for legacy rows.
    pub redaction_catalog_version: Option<String>,
    /// Exact UTF-8 byte count of `content`.
    pub content_bytes: usize,
    /// SHA-256 of the exact exported content bytes.
    pub content_sha256: String,
    /// Exact already-redacted durable content.
    pub content: String,
}

/// An inclusive sequence interval absent from the retained prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackSequenceGap {
    /// First missing sequence.
    pub first_missing_seq: u64,
    /// Last missing sequence.
    pub last_missing_seq: u64,
}

/// Explicit capture-loss evidence retained in `output_gaps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackCaptureGap {
    /// Last known sequence before the loss boundary.
    pub seq_before: u64,
    /// First known sequence after the loss boundary.
    pub seq_after: u64,
    /// Bounded persisted cause.
    pub reason: String,
    /// Gap detection timestamp in epoch milliseconds.
    pub detected_at: u64,
}

/// One checkpoint pane's exact retained output prefix and continuity evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackPanePrefix {
    /// Pane identifier; must match the corresponding checkpoint projection.
    pub pane_id: u64,
    /// Checkpoint-owned inclusive sequence ceiling.
    pub checkpoint_seq: Option<u64>,
    /// First retained sequence at or below the checkpoint ceiling.
    pub first_seq: Option<u64>,
    /// Last retained sequence at or below the checkpoint ceiling.
    pub last_seq: Option<u64>,
    /// Exact number of embedded durable rows.
    pub segment_count: usize,
    /// Exact UTF-8 bytes across embedded segment content.
    pub content_bytes: u64,
    /// Missing retained sequence intervals, including retention floor/suffix loss.
    pub sequence_gaps: Vec<CheckpointScrollbackSequenceGap>,
    /// Explicit capture-loss rows visible no later than the checkpoint.
    pub capture_gaps: Vec<CheckpointScrollbackCaptureGap>,
    /// Whether the retained sequence set starts at zero when output exists.
    pub starts_at_zero: bool,
    /// Whether the retained sequence set reaches the checkpoint ceiling.
    pub reaches_checkpoint: bool,
    /// Whether every sequence through the checkpoint ceiling is present.
    pub sequence_contiguous: bool,
    /// True only when no explicit capture-gap evidence intersects the prefix.
    pub no_capture_gaps: bool,
    /// True only for a zero-based, contiguous, ceiling-reaching prefix with no capture gaps.
    pub complete: bool,
    /// Sorted unique source redaction-catalog identities.
    pub redaction_catalog_versions: Vec<String>,
    /// Must remain true after an independent current-catalog fixed-point pass.
    pub redaction_fixed_point: bool,
    /// Domain-separated SHA-256 over every prefix field except this digest.
    pub prefix_sha256: String,
    /// Exact durable rows in ascending sequence order.
    pub segments: Vec<CheckpointScrollbackSegment>,
}

/// Explicitly conservative capability claims for the offline artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackCapabilities {
    /// Artifact contains bounded forensic scrollback text.
    pub forensic_scrollback_export: bool,
    /// Artifact can be verified without the source database.
    pub offline_verification: bool,
    /// Artifact carries checkpoint topology metadata.
    pub checkpoint_topology: bool,
    /// Artifact is not an executable restore image.
    pub executable_restore_image: bool,
    /// Artifact does not preserve terminal parser/render state.
    pub terminal_parser_state: bool,
    /// Artifact does not preserve PTY descriptors or kernel queues.
    pub pty_descriptor_state: bool,
    /// Artifact does not preserve process memory or kernel process state.
    pub process_state: bool,
    /// Artifact does not preserve running-process continuity.
    pub running_process_continuity: bool,
    /// Embedded pane IDs are evidence labels, not stable live-mux identities.
    pub stable_mux_local_pane_ids: bool,
    /// Artifact verification and import perform no live mux mutation.
    pub live_mux_mutation: bool,
}

impl CheckpointScrollbackCapabilities {
    const V1: Self = Self {
        forensic_scrollback_export: true,
        offline_verification: true,
        checkpoint_topology: true,
        executable_restore_image: false,
        terminal_parser_state: false,
        pty_descriptor_state: false,
        process_state: false,
        running_process_continuity: false,
        stable_mux_local_pane_ids: false,
        live_mux_mutation: false,
    };
}

/// Aggregate counters cross-checked by the offline verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackSummary {
    /// Number of checkpoint panes and prefix records.
    pub pane_count: usize,
    /// Total durable output rows embedded.
    pub segment_count: usize,
    /// Total explicit capture-gap rows embedded.
    pub capture_gap_count: usize,
    /// Exact UTF-8 bytes across all embedded content.
    pub content_bytes: u64,
    /// Number of panes with complete retained history through their checkpoint ceiling.
    pub complete_pane_count: usize,
    /// Number of panes with explicitly incomplete history.
    pub incomplete_pane_count: usize,
}

/// Canonical v1 payload bound by the envelope checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackPayload {
    /// Frozen integer schema version.
    pub schema_version: u32,
    /// Deterministic checkpoint-bound publication timestamp in epoch milliseconds.
    pub created_at_epoch_ms: u64,
    /// Current redaction catalog used for fixed-point verification.
    pub redaction_catalog_version: String,
    /// Resource contract embedded by the producer.
    pub limits: CheckpointScrollbackArtifactLimits,
    /// Conservative capability matrix.
    pub capabilities: CheckpointScrollbackCapabilities,
    /// Exact verified checkpoint projection.
    pub checkpoint: CheckpointScrollbackCheckpoint,
    /// Exact per-pane durable prefixes, sorted by pane ID.
    pub scrollback: Vec<CheckpointScrollbackPanePrefix>,
    /// Cross-checked aggregate counters.
    pub summary: CheckpointScrollbackSummary,
}

/// Canonical outer envelope for no-clobber publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointScrollbackArtifact {
    /// Frozen schema identity.
    pub schema: String,
    /// Only `complete` is published in the canonical namespace.
    pub publication_state: String,
    /// SHA-256 of compact canonical payload JSON.
    pub payload_sha256: String,
    /// Bounded verified payload.
    pub payload: CheckpointScrollbackPayload,
}

/// Receipt returned after independent reread and offline verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointScrollbackArtifactReceipt {
    /// Verified artifact schema.
    pub schema: String,
    /// Verifier-enforced conservative capability matrix.
    pub capabilities: CheckpointScrollbackCapabilities,
    /// Exact checkpoint row identity.
    pub checkpoint_id: i64,
    /// Exact parent mux-session identity proven by the artifact payload.
    pub session_id: String,
    /// Exact checkpoint role proven by the artifact payload.
    pub checkpoint_role: String,
    /// Exact checkpoint state witness.
    pub checkpoint_state_hash: String,
    /// Deterministic checkpoint-bound publication timestamp in epoch milliseconds.
    pub created_at_epoch_ms: u64,
    /// Exact compact payload digest.
    pub payload_sha256: String,
    /// Exact published-file digest.
    pub artifact_sha256: String,
    /// Exact published-file size.
    pub artifact_bytes: u64,
    /// Embedded pane count.
    pub pane_count: usize,
    /// Embedded segment count.
    pub segment_count: usize,
    /// Embedded content bytes.
    pub content_bytes: u64,
    /// Number of panes whose retained prefix is complete.
    pub complete_pane_count: usize,
    /// Publication durability claim.
    pub durability: &'static str,
}

/// Expected immutable identity for canonical artifact publication or recovery.
///
/// Callers exporting an already-existing checkpoint can build this value from
/// their bounded verified checkpoint query. Post-capture callers should use
/// [`Self::from_snapshot_result`] or the `SnapshotResult` convenience API.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckpointScrollbackArtifactExpectedIdentity {
    /// Exact SQLite checkpoint row ID.
    pub checkpoint_id: i64,
    /// Exact parent mux-session identity.
    pub session_id: String,
    /// Exact checkpoint timestamp in epoch milliseconds.
    pub checkpoint_at: u64,
    /// Exact checkpoint role; durable scrollback artifacts currently require `snapshot`.
    pub checkpoint_role: String,
    /// Exact verified v2 checkpoint witness.
    pub checkpoint_state_hash: String,
    /// Exact checkpoint pane count.
    pub pane_count: usize,
}

impl CheckpointScrollbackArtifactExpectedIdentity {
    /// Build the exact artifact identity returned by a successful live capture.
    #[must_use]
    pub fn from_snapshot_result(snapshot: &SnapshotResult) -> Self {
        Self {
            checkpoint_id: snapshot.checkpoint_id,
            session_id: snapshot.session_id.clone(),
            checkpoint_at: snapshot.checkpoint_at,
            checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
            checkpoint_state_hash: snapshot.state_hash.clone(),
            pane_count: snapshot.pane_count,
        }
    }
}

impl std::fmt::Debug for CheckpointScrollbackArtifactExpectedIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CheckpointScrollbackArtifactExpectedIdentity")
            .field("checkpoint_id", &self.checkpoint_id)
            .field("checkpoint_at", &self.checkpoint_at)
            .field("checkpoint_role", &self.checkpoint_role)
            .field("pane_count", &self.pane_count)
            .finish_non_exhaustive()
    }
}

impl CheckpointScrollbackArtifactReceipt {
    /// Whether every checkpoint pane has a complete retained scrollback prefix.
    #[must_use]
    pub const fn scrollback_complete(&self) -> bool {
        self.complete_pane_count == self.pane_count
    }

    /// Number of checkpoint panes with an explicitly incomplete retained prefix.
    #[must_use]
    pub const fn incomplete_pane_count(&self) -> usize {
        self.pane_count.saturating_sub(self.complete_pane_count)
    }
}

/// How the requested checkpoint artifact became durable for this invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointScrollbackArtifactResolution {
    /// This invocation published the requested target.
    Published,
    /// A prior or concurrent invocation had already published the exact target.
    RecoveredExisting,
}

/// Exact result of publishing or recovering one checkpoint artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointScrollbackArtifactPublication {
    /// Exact requested artifact path; canonical-catalog calls derive it from the checkpoint identity.
    pub path: PathBuf,
    /// Whether this invocation published or recovered the target.
    pub resolution: CheckpointScrollbackArtifactResolution,
    /// Independently verified artifact identity, durability, and coverage truth.
    pub receipt: CheckpointScrollbackArtifactReceipt,
}

impl CheckpointScrollbackArtifactPublication {
    /// Whether every checkpoint pane has a complete retained scrollback prefix.
    #[must_use]
    pub const fn scrollback_complete(&self) -> bool {
        self.receipt.scrollback_complete()
    }

    /// Number of checkpoint panes with an explicitly incomplete retained prefix.
    #[must_use]
    pub const fn incomplete_pane_count(&self) -> usize {
        self.receipt.incomplete_pane_count()
    }
}

/// One verified entry returned by a bounded artifact-directory inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointScrollbackInventoryEntry {
    /// Leaf path relative to the inventoried directory.
    pub file_name: PathBuf,
    /// Deterministic checkpoint-bound publication timestamp.
    pub created_at_epoch_ms: u64,
    /// Verified checkpoint row ID.
    pub checkpoint_id: i64,
    /// Verified checkpoint state witness used by the canonical leaf name.
    pub checkpoint_state_hash: String,
    /// Verified artifact bytes.
    pub artifact_bytes: u64,
    /// Verified artifact SHA-256.
    pub artifact_sha256: String,
}

/// Side-effect-free retention selection for a bounded artifact inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointScrollbackRetentionPlan {
    /// Newest verified entries retained by count and byte budget.
    pub retain: Vec<PathBuf>,
    /// Oldest verified entries eligible for later explicit deletion.
    pub retire: Vec<PathBuf>,
    /// Exact bytes retained by the plan.
    pub retained_bytes: u64,
}

struct BoundedCheckpointArtifactWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedCheckpointArtifactWriter {
    fn new(limit: u64) -> Result<Self, CheckpointScrollbackArtifactError> {
        let limit = usize::try_from(limit).map_err(|_| {
            CheckpointScrollbackArtifactError::InvalidLimits(
                "artifact byte limit does not fit this platform".to_string(),
            )
        })?;
        Ok(Self {
            bytes: Vec::new(),
            limit,
        })
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedCheckpointArtifactWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or_else(|| std::io::Error::other("artifact byte count overflow"))?;
        if next > self.limit {
            return Err(std::io::Error::other(format!(
                "artifact exceeds the {} byte limit",
                self.limit
            )));
        }
        self.bytes.try_reserve(buffer.len()).map_err(|error| {
            std::io::Error::other(format!("bounded artifact allocation failed: {error}"))
        })?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Allocation-free canonical serializer sink over one already-admitted byte slice.
///
/// The verifier necessarily owns both the bounded input bytes and the decoded
/// artifact tree while checking canonical encoding.  Writing the decoded tree
/// into this comparator avoids a third artifact-sized `Vec`: each serializer
/// chunk is compared directly with the corresponding input slice.
struct ExactCheckpointArtifactWriter<'a> {
    expected: &'a [u8],
    offset: usize,
    limit: usize,
    exact: bool,
}

impl<'a> ExactCheckpointArtifactWriter<'a> {
    fn new(expected: &'a [u8], limit: u64) -> Result<Self, CheckpointScrollbackArtifactError> {
        let limit = usize::try_from(limit).map_err(|_| {
            CheckpointScrollbackArtifactError::InvalidLimits(
                "artifact byte limit does not fit this platform".to_string(),
            )
        })?;
        Ok(Self {
            expected,
            offset: 0,
            limit,
            exact: true,
        })
    }

    fn is_exact(&self) -> bool {
        self.exact && self.offset == self.expected.len()
    }
}

impl Write for ExactCheckpointArtifactWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next = self
            .offset
            .checked_add(buffer.len())
            .ok_or_else(|| std::io::Error::other("canonical byte count overflow"))?;
        if next > self.limit {
            return Err(std::io::Error::other(format!(
                "canonical encoding exceeds the {} byte limit",
                self.limit
            )));
        }
        if self
            .expected
            .get(self.offset..next)
            .is_none_or(|expected| expected != buffer)
        {
            self.exact = false;
        }
        self.offset = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CheckpointArtifactHashWriter {
    hasher: sha2::Sha256,
    bytes: u64,
    limit: u64,
}

impl CheckpointArtifactHashWriter {
    fn new(limit: u64) -> Self {
        use sha2::Digest as _;

        Self {
            hasher: sha2::Sha256::new(),
            bytes: 0,
            limit,
        }
    }

    fn finish(self) -> String {
        use sha2::Digest as _;

        hex::encode(self.hasher.finalize())
    }
}

impl Write for CheckpointArtifactHashWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;

        let length = u64::try_from(buffer.len())
            .map_err(|_| std::io::Error::other("hash input length overflow"))?;
        let next = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| std::io::Error::other("hash input byte count overflow"))?;
        if next > self.limit {
            return Err(std::io::Error::other(format!(
                "hash input exceeds the {} byte limit",
                self.limit
            )));
        }
        self.hasher.update(buffer);
        self.bytes = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn checkpoint_artifact_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    hex::encode(Sha256::digest(bytes))
}

fn hash_checkpoint_artifact_json(
    value: &impl Serialize,
    limit: u64,
) -> Result<String, CheckpointScrollbackArtifactError> {
    let mut writer = CheckpointArtifactHashWriter::new(limit);
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.finish())
}

fn serialize_checkpoint_artifact(
    artifact: &CheckpointScrollbackArtifact,
    limit: u64,
) -> Result<Vec<u8>, CheckpointScrollbackArtifactError> {
    let mut writer = BoundedCheckpointArtifactWriter::new(limit)?;
    serde_json::to_writer_pretty(&mut writer, artifact)?;
    writer.write_all(b"\n")?;
    Ok(writer.into_inner())
}

fn checkpoint_artifact_has_canonical_encoding(
    artifact: &CheckpointScrollbackArtifact,
    expected: &[u8],
    limit: u64,
) -> Result<bool, CheckpointScrollbackArtifactError> {
    let mut writer = ExactCheckpointArtifactWriter::new(expected, limit)?;
    serde_json::to_writer_pretty(&mut writer, artifact)?;
    writer.write_all(b"\n")?;
    Ok(writer.is_exact())
}

fn checkpoint_artifact_untrusted_json_error(
    site: &'static str,
    error: &serde_json::Error,
) -> CheckpointScrollbackArtifactError {
    // `serde_json` errors may embed an attacker-controlled unknown field name
    // or invalid string value.  Preserve only the finite call-site label and
    // numeric source position; never render the original message.
    CheckpointScrollbackArtifactError::InvalidArtifact(format!(
        "{site} rejected at line {} column {}",
        error.line(),
        error.column()
    ))
}

fn require_checkpoint_redaction_fixed_point(
    redactor: &crate::redactor::Redactor,
    value: &str,
) -> Result<(), CheckpointScrollbackArtifactError> {
    if redactor.redact(value) == value {
        Ok(())
    } else {
        Err(CheckpointScrollbackArtifactError::RedactionNotFixedPoint)
    }
}

fn require_checkpoint_projection_redaction_fixed_point(
    checkpoint: &CheckpointScrollbackCheckpoint,
    redactor: &crate::redactor::Redactor,
) -> Result<(), CheckpointScrollbackArtifactError> {
    for value in [
        Some(checkpoint.session_id.as_str()),
        Some(checkpoint.checkpoint_type.as_str()),
        checkpoint.metadata_json.as_deref(),
        Some(checkpoint.topology_json.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        require_checkpoint_redaction_fixed_point(redactor, value)?;
    }
    for pane in &checkpoint.panes {
        for value in [
            pane.cwd.as_deref(),
            pane.command.as_deref(),
            pane.env_json.as_deref(),
            Some(pane.terminal_state_json.as_str()),
            pane.agent_metadata_json.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            require_checkpoint_redaction_fixed_point(redactor, value)?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct CheckpointScrollbackPrefixDigest<'a> {
    pane_id: u64,
    checkpoint_seq: Option<u64>,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    segment_count: usize,
    content_bytes: u64,
    sequence_gaps: &'a [CheckpointScrollbackSequenceGap],
    capture_gaps: &'a [CheckpointScrollbackCaptureGap],
    starts_at_zero: bool,
    reaches_checkpoint: bool,
    sequence_contiguous: bool,
    no_capture_gaps: bool,
    complete: bool,
    redaction_catalog_versions: &'a [String],
    redaction_fixed_point: bool,
    segments: &'a [CheckpointScrollbackSegment],
}

fn checkpoint_scrollback_prefix_sha256(
    prefix: &CheckpointScrollbackPanePrefix,
) -> Result<String, CheckpointScrollbackArtifactError> {
    hash_checkpoint_artifact_json(
        &CheckpointScrollbackPrefixDigest {
            pane_id: prefix.pane_id,
            checkpoint_seq: prefix.checkpoint_seq,
            first_seq: prefix.first_seq,
            last_seq: prefix.last_seq,
            segment_count: prefix.segment_count,
            content_bytes: prefix.content_bytes,
            sequence_gaps: &prefix.sequence_gaps,
            capture_gaps: &prefix.capture_gaps,
            starts_at_zero: prefix.starts_at_zero,
            reaches_checkpoint: prefix.reaches_checkpoint,
            sequence_contiguous: prefix.sequence_contiguous,
            no_capture_gaps: prefix.no_capture_gaps,
            complete: prefix.complete,
            redaction_catalog_versions: &prefix.redaction_catalog_versions,
            redaction_fixed_point: prefix.redaction_fixed_point,
            segments: &prefix.segments,
        },
        CHECKPOINT_SCROLLBACK_ARTIFACT_HARD_MAX_BYTES,
    )
}

fn persisted_pane_from_artifact(
    pane: &CheckpointScrollbackPaneProjection,
) -> Result<PersistedPaneState, CheckpointScrollbackArtifactError> {
    Ok(PersistedPaneState {
        pane_id: i64::try_from(pane.pane_id).map_err(|_| {
            CheckpointScrollbackArtifactError::InvalidArtifact(
                "pane ID exceeds SQLite integer range".to_string(),
            )
        })?,
        cwd: pane.cwd.clone(),
        command: pane.command.clone(),
        env_json: pane.env_json.clone(),
        terminal_state_json: pane.terminal_state_json.clone(),
        agent_metadata_json: pane.agent_metadata_json.clone(),
        scrollback_checkpoint_seq: pane
            .scrollback_checkpoint_seq
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    CheckpointScrollbackArtifactError::InvalidArtifact(
                        "scrollback sequence exceeds SQLite integer range".to_string(),
                    )
                })
            })
            .transpose()?,
        last_output_at: pane
            .last_output_at
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    CheckpointScrollbackArtifactError::InvalidArtifact(
                        "last output timestamp exceeds SQLite integer range".to_string(),
                    )
                })
            })
            .transpose()?,
    })
}

fn artifact_pane_from_persisted(
    pane: PersistedPaneState,
) -> Result<CheckpointScrollbackPaneProjection, CheckpointScrollbackArtifactError> {
    Ok(CheckpointScrollbackPaneProjection {
        pane_id: u64::try_from(pane.pane_id).map_err(|_| {
            CheckpointScrollbackArtifactError::Checkpoint(
                "checkpoint pane ID is negative".to_string(),
            )
        })?,
        cwd: pane.cwd,
        command: pane.command,
        env_json: pane.env_json,
        terminal_state_json: pane.terminal_state_json,
        agent_metadata_json: pane.agent_metadata_json,
        scrollback_checkpoint_seq: pane
            .scrollback_checkpoint_seq
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    CheckpointScrollbackArtifactError::Checkpoint(
                        "checkpoint scrollback sequence is negative".to_string(),
                    )
                })
            })
            .transpose()?,
        last_output_at: pane
            .last_output_at
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    CheckpointScrollbackArtifactError::Checkpoint(
                        "checkpoint output timestamp is negative".to_string(),
                    )
                })
            })
            .transpose()?,
    })
}

fn load_checkpoint_persisted_panes_for_artifact(
    conn: &Connection,
    checkpoint_id: i64,
    expected_panes: usize,
) -> Result<Vec<PersistedPaneState>, CheckpointScrollbackArtifactError> {
    let max_pane_text_bytes = i64::try_from(MAX_PERSISTED_PANE_TEXT_BYTES).unwrap_or(i64::MAX);
    let (observed_rows, malformed_rows): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN
                    typeof(pane_id) = 'integer' AND pane_id >= 0
                    AND typeof(terminal_state_json) = 'text'
                    AND (cwd IS NULL OR typeof(cwd) = 'text')
                    AND (command IS NULL OR typeof(command) = 'text')
                    AND (env_json IS NULL OR typeof(env_json) = 'text')
                    AND (agent_metadata_json IS NULL OR typeof(agent_metadata_json) = 'text')
                    AND (scrollback_checkpoint_seq IS NULL OR
                         (typeof(scrollback_checkpoint_seq) = 'integer'
                          AND scrollback_checkpoint_seq >= 0))
                    AND (last_output_at IS NULL OR
                         (typeof(last_output_at) = 'integer' AND last_output_at >= 0))
                    AND COALESCE(length(CAST(cwd AS BLOB)), 0)
                      + COALESCE(length(CAST(command AS BLOB)), 0)
                      + COALESCE(length(CAST(env_json AS BLOB)), 0)
                      + length(CAST(terminal_state_json AS BLOB))
                      + COALESCE(length(CAST(agent_metadata_json AS BLOB)), 0) <= ?2
                    THEN 0 ELSE 1 END), 0)
         FROM mux_pane_state
         WHERE checkpoint_id = ?1",
        rusqlite::params![checkpoint_id, max_pane_text_bytes],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let expected_rows = i64::try_from(expected_panes).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "checkpoint pane count does not fit SQLite integer range".to_string(),
        )
    })?;
    if observed_rows != expected_rows {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(format!(
            "checkpoint declares {expected_panes} panes but contains {observed_rows} pane rows"
        )));
    }
    if malformed_rows != 0 {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(format!(
            "checkpoint contains {malformed_rows} malformed pane rows"
        )));
    }

    let row_limit = expected_panes.checked_add(1).ok_or_else(|| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "checkpoint pane row limit overflows usize".to_string(),
        )
    })?;
    let mut statement = conn.prepare(
        "SELECT pane_id, cwd, command, env_json, terminal_state_json,
                agent_metadata_json, scrollback_checkpoint_seq, last_output_at
         FROM mux_pane_state
         WHERE checkpoint_id = ?1
         ORDER BY pane_id ASC, id ASC
         LIMIT ?2",
    )?;
    let mut rows = statement.query(rusqlite::params![
        checkpoint_id,
        i64::try_from(row_limit).unwrap_or(i64::MAX),
    ])?;
    let mut panes = Vec::with_capacity(expected_panes);
    while let Some(row) = rows.next()? {
        if panes.len() == expected_panes {
            return Err(CheckpointScrollbackArtifactError::Checkpoint(
                "checkpoint contains more pane rows than declared".to_string(),
            ));
        }
        panes.push(PersistedPaneState {
            pane_id: row.get(0)?,
            cwd: row.get(1)?,
            command: row.get(2)?,
            env_json: row.get(3)?,
            terminal_state_json: row.get(4)?,
            agent_metadata_json: row.get(5)?,
            scrollback_checkpoint_seq: row.get(6)?,
            last_output_at: row.get(7)?,
        });
    }
    if panes.len() != expected_panes {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(format!(
            "checkpoint declares {expected_panes} panes but {} bounded rows were readable",
            panes.len()
        )));
    }
    Ok(panes)
}

fn load_verified_checkpoint_for_artifact(
    conn: &Connection,
    checkpoint_id: i64,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackCheckpoint, CheckpointScrollbackArtifactError> {
    let checkpoint = crate::session_restore::load_checkpoint_by_id_from_conn(conn, checkpoint_id)
        .map_err(|_| {
            CheckpointScrollbackArtifactError::Checkpoint(
                "selected checkpoint could not be decoded".to_string(),
            )
        })?
        .ok_or_else(|| {
            CheckpointScrollbackArtifactError::Checkpoint(
                "selected checkpoint does not exist".to_string(),
            )
        })?;
    if checkpoint.checkpoint_role != crate::session_restore::CheckpointRole::Snapshot
        || checkpoint.verification != crate::session_restore::CheckpointVerification::VerifiedV2
        || !checkpoint.state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX)
    {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "selected row is not a verified v2 snapshot".to_string(),
        ));
    }
    if checkpoint.pane_count > limits.max_panes {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
            "checkpoint has {} panes, limit {}",
            checkpoint.pane_count, limits.max_panes
        )));
    }
    let topology_json = checkpoint.topology_json.clone().ok_or_else(|| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "verified snapshot has no topology".to_string(),
        )
    })?;
    let topology = TopologySnapshot::from_json(&topology_json).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "verified snapshot topology JSON is invalid".to_string(),
        )
    })?;
    let persisted_panes =
        load_checkpoint_persisted_panes_for_artifact(conn, checkpoint_id, checkpoint.pane_count)?;
    let checkpoint_at = i64::try_from(checkpoint.checkpoint_at).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint timestamp exceeds SQLite integer range".to_string(),
        )
    })?;
    let pane_count = i64::try_from(checkpoint.pane_count).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint pane count exceeds SQLite integer range".to_string(),
        )
    })?;
    let total_bytes = i64::try_from(checkpoint.total_bytes).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint byte count exceeds SQLite integer range".to_string(),
        )
    })?;
    let recomputed = checkpoint_witness(
        CHECKPOINT_ROLE_SNAPSHOT,
        &checkpoint.session_id,
        checkpoint.checkpoint_id,
        checkpoint_at,
        &checkpoint.checkpoint_type,
        pane_count,
        total_bytes,
        checkpoint.metadata_json(),
        Some(&topology_json),
        &persisted_panes,
    )
    .map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint witness could not be recomputed".to_string(),
        )
    })?;
    if recomputed != checkpoint.state_hash {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint witness changed during artifact projection".to_string(),
        ));
    }
    let persisted_pane_ids = persisted_panes
        .iter()
        .map(|pane| u64::try_from(pane.pane_id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            CheckpointScrollbackArtifactError::Checkpoint(
                "checkpoint contains a negative pane ID".to_string(),
            )
        })?;
    let mut topology_pane_ids = topology.pane_ids();
    topology_pane_ids.sort_unstable();
    if topology.pane_count() != checkpoint.pane_count || topology_pane_ids != persisted_pane_ids {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint topology and pane projection disagree".to_string(),
        ));
    }

    let metadata_json = checkpoint.metadata_json().map(str::to_owned);
    let panes = persisted_panes
        .into_iter()
        .map(artifact_pane_from_persisted)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CheckpointScrollbackCheckpoint {
        checkpoint_id: checkpoint.checkpoint_id,
        session_id: checkpoint.session_id,
        checkpoint_at: checkpoint.checkpoint_at,
        checkpoint_type: checkpoint.checkpoint_type,
        checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
        state_hash: checkpoint.state_hash,
        metadata_json,
        topology_sha256: checkpoint_artifact_sha256(topology_json.as_bytes()),
        topology_json,
        pane_count: checkpoint.pane_count,
        total_bytes: checkpoint.total_bytes,
        panes,
    })
}

fn checked_artifact_u64_add(
    left: u64,
    right: u64,
    label: &'static str,
) -> Result<u64, CheckpointScrollbackArtifactError> {
    left.checked_add(right).ok_or_else(|| {
        CheckpointScrollbackArtifactError::ResourceLimit(format!("{label} overflow"))
    })
}

fn checked_artifact_usize_add(
    left: usize,
    right: usize,
    label: &'static str,
) -> Result<usize, CheckpointScrollbackArtifactError> {
    left.checked_add(right).ok_or_else(|| {
        CheckpointScrollbackArtifactError::ResourceLimit(format!("{label} overflow"))
    })
}

fn compute_checkpoint_sequence_gaps(
    checkpoint_seq: Option<u64>,
    segments: &[CheckpointScrollbackSegment],
) -> Result<Vec<CheckpointScrollbackSequenceGap>, CheckpointScrollbackArtifactError> {
    let Some(checkpoint_seq) = checkpoint_seq else {
        if segments.is_empty() {
            return Ok(Vec::new());
        }
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint with no scrollback ceiling selected durable output rows".to_string(),
        ));
    };
    let mut gaps = Vec::new();
    let mut expected = 0_u64;
    for segment in segments {
        if segment.seq > checkpoint_seq {
            return Err(CheckpointScrollbackArtifactError::Checkpoint(
                "durable output row exceeds the checkpoint ceiling".to_string(),
            ));
        }
        if segment.seq < expected {
            return Err(CheckpointScrollbackArtifactError::Checkpoint(
                "durable output sequence is duplicate or out of order".to_string(),
            ));
        }
        if segment.seq > expected {
            gaps.push(CheckpointScrollbackSequenceGap {
                first_missing_seq: expected,
                last_missing_seq: segment.seq.saturating_sub(1),
            });
        }
        expected = segment.seq.checked_add(1).ok_or_else(|| {
            CheckpointScrollbackArtifactError::Checkpoint(
                "durable output sequence cannot advance".to_string(),
            )
        })?;
    }
    if expected <= checkpoint_seq {
        gaps.push(CheckpointScrollbackSequenceGap {
            first_missing_seq: expected,
            last_missing_seq: checkpoint_seq,
        });
    }
    Ok(gaps)
}

fn load_checkpoint_capture_gaps(
    conn: &Connection,
    pane_id: u64,
    checkpoint_seq: Option<u64>,
    checkpoint_at: u64,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<Vec<CheckpointScrollbackCaptureGap>, CheckpointScrollbackArtifactError> {
    let Some(checkpoint_seq) = checkpoint_seq else {
        return Ok(Vec::new());
    };
    let pane_id = i64::try_from(pane_id).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "pane ID exceeds SQLite integer range".to_string(),
        )
    })?;
    let checkpoint_seq = i64::try_from(checkpoint_seq).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint sequence exceeds SQLite integer range".to_string(),
        )
    })?;
    let checkpoint_at = i64::try_from(checkpoint_at).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint timestamp exceeds SQLite integer range".to_string(),
        )
    })?;
    let invalid_rows: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM output_gaps
         WHERE pane_id = ?1
           AND (typeof(seq_before) != 'integer' OR seq_before < 0
             OR typeof(seq_after) != 'integer' OR seq_after <= seq_before
             OR typeof(reason) != 'text'
             OR length(CAST(reason AS BLOB)) > ?2
             OR typeof(detected_at) != 'integer' OR detected_at < 0)",
        rusqlite::params![
            pane_id,
            i64::try_from(CHECKPOINT_SCROLLBACK_MAX_GAP_REASON_BYTES).unwrap_or(i64::MAX),
        ],
        |row| row.get(0),
    )?;
    if invalid_rows != 0 {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "output_gaps contains malformed rows for a checkpoint pane".to_string(),
        ));
    }
    let relevant_count: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM output_gaps
         WHERE pane_id = ?1 AND seq_after <= ?2 AND detected_at <= ?3",
        rusqlite::params![pane_id, checkpoint_seq, checkpoint_at],
        |row| row.get(0),
    )?;
    let relevant_count = usize::try_from(relevant_count).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "capture-gap count is negative or out of range".to_string(),
        )
    })?;
    if relevant_count > limits.max_gaps_per_pane {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
            "pane capture-gap count {relevant_count} exceeds {}",
            limits.max_gaps_per_pane
        )));
    }
    let row_limit = relevant_count.checked_add(1).ok_or_else(|| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "capture-gap row limit overflow".to_string(),
        )
    })?;
    let mut statement = conn.prepare(
        "SELECT seq_before, seq_after, reason, detected_at
         FROM output_gaps
         WHERE pane_id = ?1 AND seq_after <= ?2 AND detected_at <= ?3
         ORDER BY seq_before ASC, seq_after ASC, detected_at ASC, reason ASC, id ASC
         LIMIT ?4",
    )?;
    let mut rows = statement.query(rusqlite::params![
        pane_id,
        checkpoint_seq,
        checkpoint_at,
        i64::try_from(row_limit).unwrap_or(i64::MAX),
    ])?;
    let mut gaps = Vec::with_capacity(relevant_count);
    while let Some(row) = rows.next()? {
        if gaps.len() == relevant_count {
            return Err(CheckpointScrollbackArtifactError::Checkpoint(
                "capture-gap inventory changed inside a pinned read transaction".to_string(),
            ));
        }
        let seq_before: i64 = row.get(0)?;
        let seq_after: i64 = row.get(1)?;
        let reason: String = row.get(2)?;
        let detected_at: i64 = row.get(3)?;
        gaps.push(CheckpointScrollbackCaptureGap {
            seq_before: u64::try_from(seq_before).map_err(|_| {
                CheckpointScrollbackArtifactError::Checkpoint(
                    "capture gap has a negative lower sequence".to_string(),
                )
            })?,
            seq_after: u64::try_from(seq_after).map_err(|_| {
                CheckpointScrollbackArtifactError::Checkpoint(
                    "capture gap has a negative upper sequence".to_string(),
                )
            })?,
            reason,
            detected_at: u64::try_from(detected_at).map_err(|_| {
                CheckpointScrollbackArtifactError::Checkpoint(
                    "capture gap has a negative timestamp".to_string(),
                )
            })?,
        });
        if gaps.len() >= 2 {
            let previous = &gaps[gaps.len() - 2];
            let current = &gaps[gaps.len() - 1];
            if checkpoint_capture_gap_order(previous) >= checkpoint_capture_gap_order(current) {
                return Err(CheckpointScrollbackArtifactError::Checkpoint(
                    "capture-gap rows contain a duplicate canonical identity".to_string(),
                ));
            }
        }
    }
    if gaps.len() != relevant_count {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "capture-gap count disagrees with its bounded rows".to_string(),
        ));
    }
    Ok(gaps)
}

fn checkpoint_capture_gap_order(gap: &CheckpointScrollbackCaptureGap) -> (u64, u64, u64, &str) {
    (
        gap.seq_before,
        gap.seq_after,
        gap.detected_at,
        gap.reason.as_str(),
    )
}

fn load_checkpoint_scrollback_prefix(
    conn: &Connection,
    pane: &CheckpointScrollbackPaneProjection,
    checkpoint_at: u64,
    limits: CheckpointScrollbackArtifactLimits,
    redactor: &crate::redactor::Redactor,
) -> Result<CheckpointScrollbackPanePrefix, CheckpointScrollbackArtifactError> {
    let pane_id = i64::try_from(pane.pane_id).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "pane ID exceeds SQLite integer range".to_string(),
        )
    })?;
    let checkpoint_at_sql = i64::try_from(checkpoint_at).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint timestamp exceeds SQLite integer range".to_string(),
        )
    })?;
    let invalid_rows: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM output_segments
         WHERE pane_id = ?1
           AND (typeof(seq) != 'integer' OR seq < 0
             OR typeof(content) != 'text'
             OR typeof(captured_at) != 'integer' OR captured_at < 0
             OR (redaction_catalog_version IS NOT NULL AND
                 (typeof(redaction_catalog_version) != 'text' OR
                  length(CAST(redaction_catalog_version AS BLOB)) > ?2)))",
        rusqlite::params![
            pane_id,
            i64::try_from(CHECKPOINT_SCROLLBACK_MAX_CATALOG_VERSION_BYTES).unwrap_or(i64::MAX),
        ],
        |row| row.get(0),
    )?;
    if invalid_rows != 0 {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "output_segments contains malformed rows for a checkpoint pane".to_string(),
        ));
    }
    let (checkpoint_seq, last_output_at) =
        match (pane.scrollback_checkpoint_seq, pane.last_output_at) {
            (Some(checkpoint_seq), Some(last_output_at)) => (checkpoint_seq, last_output_at),
            (None, None) => {
                let unbound_rows: i64 = conn.query_row(
                    "SELECT COUNT(*)
                 FROM output_segments
                 WHERE pane_id = ?1 AND captured_at <= ?2",
                    rusqlite::params![pane_id, checkpoint_at_sql],
                    |row| row.get(0),
                )?;
                if unbound_rows != 0 {
                    return Err(CheckpointScrollbackArtifactError::Checkpoint(
                        "checkpoint omitted a scrollback reference for already-durable output"
                            .to_string(),
                    ));
                }
                let mut prefix = CheckpointScrollbackPanePrefix {
                    pane_id: pane.pane_id,
                    checkpoint_seq: None,
                    first_seq: None,
                    last_seq: None,
                    segment_count: 0,
                    content_bytes: 0,
                    sequence_gaps: Vec::new(),
                    capture_gaps: Vec::new(),
                    starts_at_zero: true,
                    reaches_checkpoint: true,
                    sequence_contiguous: true,
                    no_capture_gaps: true,
                    complete: true,
                    redaction_catalog_versions: Vec::new(),
                    redaction_fixed_point: true,
                    prefix_sha256: String::new(),
                    segments: Vec::new(),
                };
                prefix.prefix_sha256 = checkpoint_scrollback_prefix_sha256(&prefix)?;
                return Ok(prefix);
            }
            _ => {
                return Err(CheckpointScrollbackArtifactError::Checkpoint(
                    "checkpoint scrollback sequence and output timestamp disagree".to_string(),
                ));
            }
        };
    let checkpoint_seq_sql = i64::try_from(checkpoint_seq).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "checkpoint sequence exceeds SQLite integer range".to_string(),
        )
    })?;
    let (row_count, content_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(length(CAST(content AS BLOB))), 0)
         FROM output_segments
         WHERE pane_id = ?1 AND seq <= ?2",
        rusqlite::params![pane_id, checkpoint_seq_sql],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let row_count = usize::try_from(row_count).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "output segment count is negative or out of range".to_string(),
        )
    })?;
    let content_bytes = u64::try_from(content_bytes).map_err(|_| {
        CheckpointScrollbackArtifactError::Checkpoint(
            "output content byte count is negative or out of range".to_string(),
        )
    })?;
    if row_count > limits.max_segments_per_pane {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
            "pane {} has {row_count} output rows through its checkpoint, limit {}",
            pane.pane_id, limits.max_segments_per_pane
        )));
    }
    if content_bytes > limits.max_content_bytes {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
            "pane {} has {content_bytes} content bytes through its checkpoint, limit {}",
            pane.pane_id, limits.max_content_bytes
        )));
    }
    let row_limit = row_count.checked_add(1).ok_or_else(|| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "output segment row limit overflow".to_string(),
        )
    })?;
    let mut statement = conn.prepare(
        "SELECT seq, content, captured_at, redaction_catalog_version
         FROM output_segments
         WHERE pane_id = ?1 AND seq <= ?2
         ORDER BY seq ASC, id ASC
         LIMIT ?3",
    )?;
    let mut rows = statement.query(rusqlite::params![
        pane_id,
        checkpoint_seq_sql,
        i64::try_from(row_limit).unwrap_or(i64::MAX),
    ])?;
    let mut segments = Vec::with_capacity(row_count);
    let mut observed_content_bytes = 0_u64;
    let mut catalogs = std::collections::BTreeSet::new();
    while let Some(row) = rows.next()? {
        if segments.len() == row_count {
            return Err(CheckpointScrollbackArtifactError::Checkpoint(
                "output segment inventory changed inside a pinned read transaction".to_string(),
            ));
        }
        let seq: i64 = row.get(0)?;
        let content: String = row.get(1)?;
        let captured_at: i64 = row.get(2)?;
        let redaction_catalog_version: Option<String> = row.get(3)?;
        require_checkpoint_redaction_fixed_point(redactor, &content)?;
        if let Some(version) = redaction_catalog_version.as_deref() {
            require_checkpoint_redaction_fixed_point(redactor, version)?;
            catalogs.insert(version.to_string());
        }
        let content_len = content.len();
        observed_content_bytes = checked_artifact_u64_add(
            observed_content_bytes,
            u64::try_from(content_len).map_err(|_| {
                CheckpointScrollbackArtifactError::ResourceLimit(
                    "segment content length does not fit u64".to_string(),
                )
            })?,
            "pane content byte count",
        )?;
        let captured_at = u64::try_from(captured_at).map_err(|_| {
            CheckpointScrollbackArtifactError::Checkpoint(
                "output segment has a negative timestamp".to_string(),
            )
        })?;
        if captured_at > last_output_at {
            return Err(CheckpointScrollbackArtifactError::Checkpoint(
                "output segment timestamp exceeds the checkpoint scrollback ceiling".to_string(),
            ));
        }
        segments.push(CheckpointScrollbackSegment {
            seq: u64::try_from(seq).map_err(|_| {
                CheckpointScrollbackArtifactError::Checkpoint(
                    "output segment has a negative sequence".to_string(),
                )
            })?,
            captured_at,
            redaction_catalog_version,
            content_bytes: content_len,
            content_sha256: checkpoint_artifact_sha256(content.as_bytes()),
            content,
        });
    }
    if segments.len() != row_count || observed_content_bytes != content_bytes {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "output segment aggregate disagrees with its bounded rows".to_string(),
        ));
    }
    let sequence_gaps = compute_checkpoint_sequence_gaps(Some(checkpoint_seq), &segments)?;
    if sequence_gaps.is_empty()
        && segments.iter().map(|segment| segment.captured_at).max() != Some(last_output_at)
    {
        return Err(CheckpointScrollbackArtifactError::Checkpoint(
            "complete durable output rows disagree with the checkpoint timestamp witness"
                .to_string(),
        ));
    }
    let capture_gaps = load_checkpoint_capture_gaps(
        conn,
        pane.pane_id,
        Some(checkpoint_seq),
        checkpoint_at,
        limits,
    )?;
    for gap in &capture_gaps {
        require_checkpoint_redaction_fixed_point(redactor, &gap.reason)?;
    }
    let first_seq = segments.first().map(|segment| segment.seq);
    let last_seq = segments.last().map(|segment| segment.seq);
    let starts_at_zero = first_seq == Some(0);
    let reaches_checkpoint = last_seq == Some(checkpoint_seq);
    let sequence_contiguous = sequence_gaps.is_empty();
    let no_capture_gaps = capture_gaps.is_empty();
    let complete = starts_at_zero && reaches_checkpoint && sequence_contiguous && no_capture_gaps;
    let mut prefix = CheckpointScrollbackPanePrefix {
        pane_id: pane.pane_id,
        checkpoint_seq: Some(checkpoint_seq),
        first_seq,
        last_seq,
        segment_count: segments.len(),
        content_bytes,
        sequence_gaps,
        capture_gaps,
        starts_at_zero,
        reaches_checkpoint,
        sequence_contiguous,
        no_capture_gaps,
        complete,
        redaction_catalog_versions: catalogs.into_iter().collect(),
        redaction_fixed_point: true,
        prefix_sha256: String::new(),
        segments,
    };
    prefix.prefix_sha256 = checkpoint_scrollback_prefix_sha256(&prefix)?;
    Ok(prefix)
}

fn build_checkpoint_scrollback_payload(
    db_path: &str,
    checkpoint_id: i64,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackPayload, CheckpointScrollbackArtifactError> {
    let limits = limits.validate()?;
    let conn = open_snapshot_query_conn(db_path)?;
    let transaction = conn.unchecked_transaction()?;
    let checkpoint = load_verified_checkpoint_for_artifact(&transaction, checkpoint_id, limits)?;
    let redactor = crate::redactor::Redactor::new();
    require_checkpoint_projection_redaction_fixed_point(&checkpoint, &redactor)?;

    let mut scrollback = Vec::with_capacity(checkpoint.panes.len());
    let mut total_segments = 0_usize;
    let mut total_gaps = 0_usize;
    let mut total_content_bytes = 0_u64;
    let mut complete_pane_count = 0_usize;
    for pane in &checkpoint.panes {
        let prefix = load_checkpoint_scrollback_prefix(
            &transaction,
            pane,
            checkpoint.checkpoint_at,
            limits,
            &redactor,
        )?;
        total_segments = checked_artifact_usize_add(
            total_segments,
            prefix.segment_count,
            "artifact segment count",
        )?;
        total_gaps = checked_artifact_usize_add(
            total_gaps,
            prefix.capture_gaps.len(),
            "artifact capture-gap count",
        )?;
        total_content_bytes = checked_artifact_u64_add(
            total_content_bytes,
            prefix.content_bytes,
            "artifact content byte count",
        )?;
        if total_segments > limits.max_total_segments {
            return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
                "artifact segment count {total_segments} exceeds {}",
                limits.max_total_segments
            )));
        }
        if total_gaps > limits.max_total_gaps {
            return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
                "artifact capture-gap count {total_gaps} exceeds {}",
                limits.max_total_gaps
            )));
        }
        if total_content_bytes > limits.max_content_bytes {
            return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
                "artifact content byte count {total_content_bytes} exceeds {}",
                limits.max_content_bytes
            )));
        }
        if prefix.complete {
            complete_pane_count =
                checked_artifact_usize_add(complete_pane_count, 1, "complete pane count")?;
        }
        scrollback.push(prefix);
    }
    transaction.commit()?;
    let pane_count = checkpoint.panes.len();
    let created_at_epoch_ms = checkpoint.checkpoint_at;
    Ok(CheckpointScrollbackPayload {
        schema_version: 1,
        created_at_epoch_ms,
        redaction_catalog_version: crate::redact_backfill::current_catalog_version().to_string(),
        limits,
        capabilities: CheckpointScrollbackCapabilities::V1,
        checkpoint,
        scrollback,
        summary: CheckpointScrollbackSummary {
            pane_count,
            segment_count: total_segments,
            capture_gap_count: total_gaps,
            content_bytes: total_content_bytes,
            complete_pane_count,
            incomplete_pane_count: pane_count.saturating_sub(complete_pane_count),
        },
    })
}

#[derive(Clone, Copy)]
struct CheckpointArtifactJsonStructureLimits {
    max_nodes: usize,
    max_map_entries: usize,
    max_sequence_entries: usize,
    max_string_bytes: u64,
    max_depth: usize,
}

const CHECKPOINT_ARTIFACT_JSON_STRUCTURE_LIMITS: CheckpointArtifactJsonStructureLimits =
    CheckpointArtifactJsonStructureLimits {
        max_nodes: 20_000_000,
        max_map_entries: 9_000_000,
        max_sequence_entries: 2_000_000,
        max_string_bytes: CHECKPOINT_SCROLLBACK_ARTIFACT_HARD_MAX_BYTES,
        max_depth: 64,
    };

struct CheckpointArtifactJsonStructureBudget {
    limits: CheckpointArtifactJsonStructureLimits,
    nodes: std::cell::Cell<usize>,
    map_entries: std::cell::Cell<usize>,
    sequence_entries: std::cell::Cell<usize>,
    string_bytes: std::cell::Cell<u64>,
}

impl CheckpointArtifactJsonStructureBudget {
    fn new(limits: CheckpointArtifactJsonStructureLimits) -> Self {
        Self {
            limits,
            nodes: std::cell::Cell::new(0),
            map_entries: std::cell::Cell::new(0),
            sequence_entries: std::cell::Cell::new(0),
            string_bytes: std::cell::Cell::new(0),
        }
    }

    fn consume_usize(
        counter: &std::cell::Cell<usize>,
        amount: usize,
        limit: usize,
        label: &'static str,
    ) -> Result<(), String> {
        let next = counter
            .get()
            .checked_add(amount)
            .ok_or_else(|| format!("artifact JSON {label} count overflow"))?;
        if next > limit {
            return Err(format!(
                "artifact JSON exceeds the {limit} {label} safety limit"
            ));
        }
        counter.set(next);
        Ok(())
    }

    fn consume_node(&self, depth: usize) -> Result<(), String> {
        if depth > self.limits.max_depth {
            return Err(format!(
                "artifact JSON exceeds the {} level nesting safety limit",
                self.limits.max_depth
            ));
        }
        Self::consume_usize(&self.nodes, 1, self.limits.max_nodes, "node")
    }

    fn consume_map_entry(&self) -> Result<(), String> {
        Self::consume_usize(
            &self.map_entries,
            1,
            self.limits.max_map_entries,
            "map entry",
        )
    }

    fn consume_sequence_entry(&self) -> Result<(), String> {
        Self::consume_usize(
            &self.sequence_entries,
            1,
            self.limits.max_sequence_entries,
            "sequence entry",
        )
    }

    fn consume_string(&self, bytes: usize) -> Result<(), String> {
        let bytes = u64::try_from(bytes)
            .map_err(|_| "artifact JSON decoded string length overflow".to_string())?;
        let next = self
            .string_bytes
            .get()
            .checked_add(bytes)
            .ok_or_else(|| "artifact JSON decoded string byte count overflow".to_string())?;
        if next > self.limits.max_string_bytes {
            return Err(format!(
                "artifact JSON exceeds the {} decoded string byte safety limit",
                self.limits.max_string_bytes
            ));
        }
        self.string_bytes.set(next);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct CheckpointArtifactJsonStructureSeed<'a> {
    budget: &'a CheckpointArtifactJsonStructureBudget,
    depth: usize,
}

impl CheckpointArtifactJsonStructureSeed<'_> {
    fn child(self) -> Self {
        Self {
            budget: self.budget,
            depth: self.depth.saturating_add(1),
        }
    }

    fn budget_error<E: serde::de::Error>(error: String) -> E {
        E::custom(error)
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for CheckpointArtifactJsonStructureSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.budget
            .consume_node(self.depth)
            .map_err(Self::budget_error)?;
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for CheckpointArtifactJsonStructureSeed<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON within the checkpoint artifact structural budget")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde::de::DeserializeSeed::deserialize(self.child(), deserializer)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget
            .consume_string(value.len())
            .map_err(Self::budget_error)
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(&value)
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget
            .consume_string(value.len())
            .map_err(Self::budget_error)
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_bytes(value)
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_bytes(&value)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        while sequence.next_element_seed(self.child())?.is_some() {
            self.budget
                .consume_sequence_entry()
                .map_err(Self::budget_error)?;
        }
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        while map.next_key_seed(self.child())?.is_some() {
            self.budget
                .consume_map_entry()
                .map_err(Self::budget_error)?;
            map.next_value_seed(self.child())?;
        }
        Ok(())
    }
}

fn verify_checkpoint_artifact_json_structure(
    bytes: &[u8],
) -> Result<(), CheckpointScrollbackArtifactError> {
    use serde::de::DeserializeSeed as _;

    let budget =
        CheckpointArtifactJsonStructureBudget::new(CHECKPOINT_ARTIFACT_JSON_STRUCTURE_LIMITS);
    let seed = CheckpointArtifactJsonStructureSeed {
        budget: &budget,
        depth: 0,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    seed.deserialize(&mut deserializer)
        .map_err(|error| checkpoint_artifact_untrusted_json_error("JSON structure", &error))?;
    deserializer
        .end()
        .map_err(|error| checkpoint_artifact_untrusted_json_error("JSON trailing data", &error))?;
    Ok(())
}

fn validate_checkpoint_scrollback_checkpoint(
    checkpoint: &CheckpointScrollbackCheckpoint,
    limits: CheckpointScrollbackArtifactLimits,
    redactor: &crate::redactor::Redactor,
) -> Result<(), CheckpointScrollbackArtifactError> {
    if checkpoint.checkpoint_id <= 0
        || checkpoint.checkpoint_role != CHECKPOINT_ROLE_SNAPSHOT
        || !checkpoint.state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX)
        || checkpoint.pane_count != checkpoint.panes.len()
        || checkpoint.pane_count > limits.max_panes
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint identity, role, or pane count is invalid".to_string(),
        ));
    }
    if checkpoint.topology_sha256 != checkpoint_artifact_sha256(checkpoint.topology_json.as_bytes())
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint topology checksum mismatch".to_string(),
        ));
    }
    let topology = TopologySnapshot::from_json(&checkpoint.topology_json).map_err(|_| {
        CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint topology JSON is invalid".to_string(),
        )
    })?;
    let mut prior_pane_id = None;
    for pane in &checkpoint.panes {
        if prior_pane_id.is_some_and(|previous| previous >= pane.pane_id)
            || pane.scrollback_checkpoint_seq.is_some() != pane.last_output_at.is_some()
        {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "checkpoint panes or scrollback witnesses are inconsistent".to_string(),
            ));
        }
        prior_pane_id = Some(pane.pane_id);
    }
    let pane_ids = checkpoint
        .panes
        .iter()
        .map(|pane| pane.pane_id)
        .collect::<Vec<_>>();
    let mut topology_pane_ids = topology.pane_ids();
    topology_pane_ids.sort_unstable();
    if topology.pane_count() != checkpoint.pane_count || topology_pane_ids != pane_ids {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint topology and pane projection disagree".to_string(),
        ));
    }
    let persisted_panes = checkpoint
        .panes
        .iter()
        .map(persisted_pane_from_artifact)
        .collect::<Result<Vec<_>, _>>()?;
    let checkpoint_at = i64::try_from(checkpoint.checkpoint_at).map_err(|_| {
        CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint timestamp exceeds SQLite integer range".to_string(),
        )
    })?;
    let pane_count = i64::try_from(checkpoint.pane_count).map_err(|_| {
        CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint pane count exceeds SQLite integer range".to_string(),
        )
    })?;
    let total_bytes = i64::try_from(checkpoint.total_bytes).map_err(|_| {
        CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint byte count exceeds SQLite integer range".to_string(),
        )
    })?;
    let recomputed = checkpoint_witness(
        CHECKPOINT_ROLE_SNAPSHOT,
        &checkpoint.session_id,
        checkpoint.checkpoint_id,
        checkpoint_at,
        &checkpoint.checkpoint_type,
        pane_count,
        total_bytes,
        checkpoint.metadata_json.as_deref(),
        Some(&checkpoint.topology_json),
        &persisted_panes,
    )
    .map_err(|_| {
        CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint witness cannot be recomputed".to_string(),
        )
    })?;
    if recomputed != checkpoint.state_hash {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint witness mismatch".to_string(),
        ));
    }
    require_checkpoint_projection_redaction_fixed_point(checkpoint, redactor).map_err(|error| {
        match error {
            CheckpointScrollbackArtifactError::RedactionNotFixedPoint => {
                CheckpointScrollbackArtifactError::InvalidArtifact(
                    "checkpoint projection is not a current redaction fixed point".to_string(),
                )
            }
            other => other,
        }
    })
}

fn validate_checkpoint_scrollback_prefix(
    prefix: &CheckpointScrollbackPanePrefix,
    checkpoint_pane: &CheckpointScrollbackPaneProjection,
    checkpoint_at: u64,
    limits: CheckpointScrollbackArtifactLimits,
    redactor: &crate::redactor::Redactor,
) -> Result<(usize, usize, u64), CheckpointScrollbackArtifactError> {
    let pane_identity_matches = prefix.pane_id == checkpoint_pane.pane_id;
    let scrollback_sequence_matches =
        prefix.checkpoint_seq == checkpoint_pane.scrollback_checkpoint_seq;
    if !pane_identity_matches
        || !scrollback_sequence_matches
        || !prefix.redaction_fixed_point
        || prefix.segment_count != prefix.segments.len()
        || prefix.segment_count > limits.max_segments_per_pane
        || prefix.capture_gaps.len() > limits.max_gaps_per_pane
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "pane prefix identity, counts, or redaction claim is invalid".to_string(),
        ));
    }
    let mut observed_content_bytes = 0_u64;
    let mut prior_seq = None;
    let mut observed_catalogs = std::collections::BTreeSet::new();
    let last_output_at = checkpoint_pane.last_output_at;
    for segment in &prefix.segments {
        if prior_seq.is_some_and(|previous| previous >= segment.seq)
            || prefix
                .checkpoint_seq
                .is_some_and(|checkpoint_seq| segment.seq > checkpoint_seq)
            || prefix.checkpoint_seq.is_none()
            || last_output_at.is_none_or(|ceiling| segment.captured_at > ceiling)
            || segment.content_bytes != segment.content.len()
            || segment.content_sha256 != checkpoint_artifact_sha256(segment.content.as_bytes())
        {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "pane prefix segment metadata or ordering is invalid".to_string(),
            ));
        }
        require_checkpoint_redaction_fixed_point(redactor, &segment.content).map_err(|_| {
            CheckpointScrollbackArtifactError::InvalidArtifact(
                "pane prefix content is not a current redaction fixed point".to_string(),
            )
        })?;
        if let Some(version) = segment.redaction_catalog_version.as_deref() {
            if version.len() > CHECKPOINT_SCROLLBACK_MAX_CATALOG_VERSION_BYTES {
                return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                    "segment redaction catalog identity is oversized".to_string(),
                ));
            }
            require_checkpoint_redaction_fixed_point(redactor, version).map_err(|_| {
                CheckpointScrollbackArtifactError::InvalidArtifact(
                    "segment redaction catalog identity is not a fixed point".to_string(),
                )
            })?;
            observed_catalogs.insert(version.to_string());
        }
        observed_content_bytes = checked_artifact_u64_add(
            observed_content_bytes,
            u64::try_from(segment.content_bytes).map_err(|_| {
                CheckpointScrollbackArtifactError::InvalidArtifact(
                    "segment content length does not fit u64".to_string(),
                )
            })?,
            "verified pane content bytes",
        )?;
        prior_seq = Some(segment.seq);
    }
    if observed_content_bytes != prefix.content_bytes
        || observed_content_bytes > limits.max_content_bytes
        || prefix.first_seq != prefix.segments.first().map(|segment| segment.seq)
        || prefix.last_seq != prefix.segments.last().map(|segment| segment.seq)
        || prefix.redaction_catalog_versions != observed_catalogs.into_iter().collect::<Vec<_>>()
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "pane prefix aggregate does not match its segments".to_string(),
        ));
    }
    let expected_sequence_gaps =
        compute_checkpoint_sequence_gaps(prefix.checkpoint_seq, &prefix.segments).map_err(
            |_| {
                CheckpointScrollbackArtifactError::InvalidArtifact(
                    "pane prefix sequence gaps cannot be recomputed".to_string(),
                )
            },
        )?;
    if prefix.sequence_gaps != expected_sequence_gaps
        || (expected_sequence_gaps.is_empty()
            && prefix
                .segments
                .iter()
                .map(|segment| segment.captured_at)
                .max()
                != last_output_at)
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "pane prefix sequence-gap or timestamp evidence is not canonical".to_string(),
        ));
    }
    let mut prior_capture_gap = None;
    for gap in &prefix.capture_gaps {
        if gap.seq_after <= gap.seq_before
            || prefix
                .checkpoint_seq
                .is_none_or(|checkpoint_seq| gap.seq_after > checkpoint_seq)
            || gap.detected_at > checkpoint_at
            || gap.reason.len() > CHECKPOINT_SCROLLBACK_MAX_GAP_REASON_BYTES
            || prior_capture_gap.is_some_and(|previous| {
                checkpoint_capture_gap_order(previous) >= checkpoint_capture_gap_order(gap)
            })
        {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "pane prefix capture-gap evidence is invalid or out of order".to_string(),
            ));
        }
        require_checkpoint_redaction_fixed_point(redactor, &gap.reason).map_err(|_| {
            CheckpointScrollbackArtifactError::InvalidArtifact(
                "capture-gap reason is not a current redaction fixed point".to_string(),
            )
        })?;
        prior_capture_gap = Some(gap);
    }
    let starts_at_zero = match prefix.checkpoint_seq {
        None => prefix.segments.is_empty(),
        Some(_) => prefix.first_seq == Some(0),
    };
    let reaches_checkpoint = match prefix.checkpoint_seq {
        None => prefix.segments.is_empty(),
        Some(checkpoint_seq) => prefix.last_seq == Some(checkpoint_seq),
    };
    let sequence_contiguous = prefix.sequence_gaps.is_empty();
    let no_capture_gaps = prefix.capture_gaps.is_empty();
    let complete = starts_at_zero && reaches_checkpoint && sequence_contiguous && no_capture_gaps;
    if prefix.starts_at_zero != starts_at_zero
        || prefix.reaches_checkpoint != reaches_checkpoint
        || prefix.sequence_contiguous != sequence_contiguous
        || prefix.no_capture_gaps != no_capture_gaps
        || prefix.complete != complete
        || prefix.prefix_sha256 != checkpoint_scrollback_prefix_sha256(prefix)?
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "pane prefix completeness or checksum evidence is invalid".to_string(),
        ));
    }
    Ok((
        prefix.segment_count,
        prefix.capture_gaps.len(),
        prefix.content_bytes,
    ))
}

fn validate_checkpoint_scrollback_payload(
    payload: &CheckpointScrollbackPayload,
    caller_limits: CheckpointScrollbackArtifactLimits,
) -> Result<(), CheckpointScrollbackArtifactError> {
    let caller_limits = caller_limits.validate()?;
    let embedded_limits = payload.limits.validate().map_err(|_| {
        CheckpointScrollbackArtifactError::InvalidArtifact(
            "embedded producer limits are invalid".to_string(),
        )
    })?;
    if !caller_limits.admits(&embedded_limits) {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(
            "embedded producer limits exceed verifier limits".to_string(),
        ));
    }
    if payload.schema_version != 1
        || payload.created_at_epoch_ms != payload.checkpoint.checkpoint_at
        || payload.capabilities != CheckpointScrollbackCapabilities::V1
        || payload.redaction_catalog_version.len() > CHECKPOINT_SCROLLBACK_MAX_CATALOG_VERSION_BYTES
        || payload.redaction_catalog_version.is_empty()
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "payload schema, capabilities, or redaction catalog is invalid".to_string(),
        ));
    }
    let redactor = crate::redactor::Redactor::new();
    require_checkpoint_redaction_fixed_point(&redactor, &payload.redaction_catalog_version)
        .map_err(|_| {
            CheckpointScrollbackArtifactError::InvalidArtifact(
                "payload redaction catalog identity is not a fixed point".to_string(),
            )
        })?;
    validate_checkpoint_scrollback_checkpoint(&payload.checkpoint, embedded_limits, &redactor)?;
    if payload.scrollback.len() != payload.checkpoint.panes.len() {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "scrollback pane inventory does not match the checkpoint".to_string(),
        ));
    }
    let mut total_segments = 0_usize;
    let mut total_gaps = 0_usize;
    let mut total_content_bytes = 0_u64;
    let mut complete_pane_count = 0_usize;
    for (prefix, checkpoint_pane) in payload
        .scrollback
        .iter()
        .zip(payload.checkpoint.panes.iter())
    {
        let (segments, gaps, bytes) = validate_checkpoint_scrollback_prefix(
            prefix,
            checkpoint_pane,
            payload.checkpoint.checkpoint_at,
            embedded_limits,
            &redactor,
        )?;
        total_segments = checked_artifact_usize_add(
            total_segments,
            segments,
            "verified artifact segment count",
        )?;
        total_gaps =
            checked_artifact_usize_add(total_gaps, gaps, "verified artifact capture-gap count")?;
        total_content_bytes = checked_artifact_u64_add(
            total_content_bytes,
            bytes,
            "verified artifact content bytes",
        )?;
        if prefix.complete {
            complete_pane_count =
                checked_artifact_usize_add(complete_pane_count, 1, "verified complete pane count")?;
        }
    }
    let pane_count = payload.checkpoint.panes.len();
    if total_segments > embedded_limits.max_total_segments
        || total_gaps > embedded_limits.max_total_gaps
        || total_content_bytes > embedded_limits.max_content_bytes
        || payload.summary
            != (CheckpointScrollbackSummary {
                pane_count,
                segment_count: total_segments,
                capture_gap_count: total_gaps,
                content_bytes: total_content_bytes,
                complete_pane_count,
                incomplete_pane_count: pane_count.saturating_sub(complete_pane_count),
            })
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "payload summary or total resource accounting is invalid".to_string(),
        ));
    }
    SnapshotRecoveryEvidence::verified_checkpoint_scrollback_export(
        complete_pane_count == pane_count,
    )
    .validate_claim(
        SnapshotRecoveryCapability::ForensicContentExport,
        SnapshotRecoveryReadiness::Candidate,
    )
    .map_err(|error| {
        CheckpointScrollbackArtifactError::InvalidArtifact(format!(
            "checkpoint artifact recovery claim contract rejected: {error}"
        ))
    })?;
    Ok(())
}

fn checkpoint_artifact_path_contains_parent(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn create_checkpoint_artifact_private_directory(
    parent: &cap_std::fs::Dir,
    name: &Path,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt as _;

        let mut builder = cap_std::fs::DirBuilder::new();
        builder.mode(0o700);
        parent.create_dir_with(name, &builder)
    }
    #[cfg(not(unix))]
    {
        parent.create_dir(name)
    }
}

fn sync_checkpoint_artifact_directory(directory: &cap_std::fs::Dir) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        directory.open(".")?.into_std().sync_all()
    }
    #[cfg(windows)]
    {
        let _ = directory;
        Ok(())
    }
}

fn checkpoint_artifact_absolute_directory_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn checkpoint_artifact_validate_directory_type(
    metadata: &cap_std::fs::Metadata,
) -> std::io::Result<()> {
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact directory authority is not a directory",
        ))
    }
}

#[cfg(unix)]
fn checkpoint_artifact_uid_is_trusted(uid: u32, effective_uid: u32) -> bool {
    uid == effective_uid || uid == 0
}

/// Require an opened ancestor to protect one child pathname from peer rename.
///
/// A trusted, non-peer-writable directory protects all of its entries. The
/// Unix sticky bit is the one deliberate exception for shared roots such as
/// `/tmp`, but it is sufficient only when the child owner is also the effective
/// user or root. An untrusted directory owner could first change its mode, so
/// mode bits alone never authenticate an ancestor.
fn validate_checkpoint_artifact_parent_child_authority(
    parent: &cap_std::fs::Dir,
    child: &cap_std::fs::Dir,
) -> std::io::Result<()> {
    let parent_metadata = parent.dir_metadata()?;
    let child_metadata = child.dir_metadata()?;
    checkpoint_artifact_validate_directory_type(&parent_metadata)?;
    checkpoint_artifact_validate_directory_type(&child_metadata)?;
    #[cfg(unix)]
    {
        let effective_uid = rustix::process::geteuid().as_raw();
        let parent_mode = cap_std::fs::MetadataExt::mode(&parent_metadata);
        let peer_writable = parent_mode & 0o022 != 0;
        let sticky = parent_mode & 0o1000 != 0;
        let parent_owner_is_trusted = checkpoint_artifact_uid_is_trusted(
            cap_std::fs::MetadataExt::uid(&parent_metadata),
            effective_uid,
        );
        let child_owner_is_trusted = checkpoint_artifact_uid_is_trusted(
            cap_std::fs::MetadataExt::uid(&child_metadata),
            effective_uid,
        );
        if !parent_owner_is_trusted || (peer_writable && !(sticky && child_owner_is_trusted)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "artifact directory ancestor does not protect its child entry",
            ));
        }
    }
    Ok(())
}

/// Check whether a new effective-user-owned child can be created safely.
///
/// This runs before `mkdir`, so an unsafe ancestor fails without leaving a new
/// directory behind. An existing child is checked again using its actual owner.
fn validate_checkpoint_artifact_parent_for_new_child(
    parent: &cap_std::fs::Dir,
) -> std::io::Result<()> {
    let metadata = parent.dir_metadata()?;
    checkpoint_artifact_validate_directory_type(&metadata)?;
    #[cfg(unix)]
    {
        let effective_uid = rustix::process::geteuid().as_raw();
        let mode = cap_std::fs::MetadataExt::mode(&metadata);
        let peer_writable = mode & 0o022 != 0;
        let sticky = mode & 0o1000 != 0;
        let owner_is_trusted = checkpoint_artifact_uid_is_trusted(
            cap_std::fs::MetadataExt::uid(&metadata),
            effective_uid,
        );
        if !owner_is_trusted || (peer_writable && !sticky) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "artifact directory ancestor cannot safely admit a new child",
            ));
        }
    }
    Ok(())
}

/// The directory containing artifact, lock, and staging names is an integrity
/// authority, not merely a traversal component. Refuse existing directories
/// that another effective user owns or that group/other users can modify.
/// Group/other read and traversal bits are permitted; artifact confidentiality
/// is enforced by the create-new `0600` file authority instead.
fn validate_checkpoint_artifact_final_directory_authority(
    directory: &cap_std::fs::Dir,
) -> std::io::Result<()> {
    let metadata = directory.dir_metadata()?;
    checkpoint_artifact_validate_directory_type(&metadata)?;
    #[cfg(unix)]
    {
        let effective_uid = rustix::process::geteuid().as_raw();
        if cap_std::fs::MetadataExt::uid(&metadata) != effective_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "artifact directory is not owned by the effective user",
            ));
        }
        if cap_std::fs::MetadataExt::mode(&metadata) & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "artifact directory is writable by group or other users",
            ));
        }
    }
    Ok(())
}

fn ensure_checkpoint_artifact_directory_tree_nofollow(
    path: &Path,
) -> std::io::Result<cap_std::fs::Dir> {
    use cap_fs_ext::DirExt as _;

    let Some(leaf) = path.file_name() else {
        let base = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        return cap_std::fs::Dir::open_ambient_dir(base, cap_std::ambient_authority());
    };
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = ensure_checkpoint_artifact_directory_tree_nofollow(parent_path)?;
    validate_checkpoint_artifact_parent_for_new_child(&parent)?;
    let leaf = Path::new(leaf);
    let created = match create_checkpoint_artifact_private_directory(&parent, leaf) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error),
    };
    let directory = parent.open_dir_nofollow(leaf)?;
    validate_checkpoint_artifact_parent_child_authority(&parent, &directory)?;
    if created {
        sync_checkpoint_artifact_directory(&parent)?;
    }
    Ok(directory)
}

fn ensure_checkpoint_artifact_directory_nofollow(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    if checkpoint_artifact_path_contains_parent(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact directory contains a parent component",
        ));
    }
    let absolute = checkpoint_artifact_absolute_directory_path(path)?;
    let directory = ensure_checkpoint_artifact_directory_tree_nofollow(&absolute)?;
    validate_checkpoint_artifact_final_directory_authority(&directory)?;
    Ok(directory)
}

fn open_checkpoint_artifact_directory_tree_nofollow(
    path: &Path,
) -> std::io::Result<cap_std::fs::Dir> {
    use cap_fs_ext::DirExt as _;

    let Some(leaf) = path.file_name() else {
        let base = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        return cap_std::fs::Dir::open_ambient_dir(base, cap_std::ambient_authority());
    };
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_checkpoint_artifact_directory_tree_nofollow(parent_path)?;
    let directory = parent.open_dir_nofollow(Path::new(leaf))?;
    validate_checkpoint_artifact_parent_child_authority(&parent, &directory)?;
    Ok(directory)
}

fn open_checkpoint_artifact_directory_nofollow(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    if checkpoint_artifact_path_contains_parent(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact directory contains a parent component",
        ));
    }
    let absolute = checkpoint_artifact_absolute_directory_path(path)?;
    let directory = open_checkpoint_artifact_directory_tree_nofollow(&absolute)?;
    validate_checkpoint_artifact_final_directory_authority(&directory)?;
    Ok(directory)
}

fn checkpoint_artifact_parent_and_leaf(
    path: &Path,
    create_parent: bool,
) -> Result<(cap_std::fs::Dir, PathBuf, PathBuf), CheckpointScrollbackArtifactError> {
    if checkpoint_artifact_path_contains_parent(path) {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact path contains a parent component".to_string(),
        ));
    }
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            CheckpointScrollbackArtifactError::InvalidArtifact(
                "artifact path has no leaf name".to_string(),
            )
        })?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let parent = if create_parent {
        ensure_checkpoint_artifact_directory_nofollow(&parent_path)?
    } else {
        open_checkpoint_artifact_directory_nofollow(&parent_path)?
    };
    Ok((parent, leaf, parent_path))
}

fn revalidate_checkpoint_artifact_parent(
    parent_path: &Path,
    pinned: &cap_std::fs::Dir,
) -> Result<(), CheckpointScrollbackArtifactError> {
    use cap_fs_ext::OsMetadataExt as _;

    validate_checkpoint_artifact_final_directory_authority(pinned)?;
    let pinned_metadata = pinned.dir_metadata()?;
    let reopened = open_checkpoint_artifact_directory_nofollow(parent_path)?;
    let reopened_metadata = reopened.dir_metadata()?;
    if !pinned_metadata.is_dir()
        || !reopened_metadata.is_dir()
        || pinned_metadata.dev() != reopened_metadata.dev()
        || pinned_metadata.ino() != reopened_metadata.ino()
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact parent directory changed identity".to_string(),
        ));
    }
    Ok(())
}

/// One complete filesystem observation used to reject mutation during a read.
///
/// The Unix fields are the supported macOS/Linux authority: `ctime` cannot be
/// restored by an unprivileged writer, so a same-size rewrite followed by an
/// `mtime` restoration still changes this snapshot. Portable targets retain
/// the strongest stable metadata exposed by `std`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckpointArtifactFileSnapshot {
    byte_len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    owner: u32,
    #[cfg(unix)]
    link_count: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl CheckpointArtifactFileSnapshot {
    fn capture_cap(
        metadata: &cap_std::fs::Metadata,
    ) -> Result<Self, CheckpointScrollbackArtifactError> {
        #[cfg(unix)]
        use cap_fs_ext::OsMetadataExt as _;
        #[cfg(unix)]
        use cap_std::fs::PermissionsExt as _;

        Ok(Self {
            byte_len: metadata.len(),
            modified: metadata.modified()?.into_std(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.permissions().mode(),
            #[cfg(unix)]
            owner: cap_std::fs::MetadataExt::uid(metadata),
            #[cfg(unix)]
            link_count: metadata.nlink(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }

    fn capture_std(
        metadata: &std::fs::Metadata,
    ) -> Result<Self, CheckpointScrollbackArtifactError> {
        #[cfg(unix)]
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        Ok(Self {
            byte_len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.permissions().mode(),
            #[cfg(unix)]
            owner: metadata.uid(),
            #[cfg(unix)]
            link_count: metadata.nlink(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }
}

fn validate_checkpoint_artifact_file_metadata(
    path_metadata: &cap_std::fs::Metadata,
    handle_metadata: &cap_std::fs::Metadata,
    expected_len: Option<u64>,
) -> Result<CheckpointArtifactFileSnapshot, CheckpointScrollbackArtifactError> {
    if !path_metadata.is_file() || !handle_metadata.is_file() {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact authority is not a regular file".to_string(),
        ));
    }
    let path_snapshot = CheckpointArtifactFileSnapshot::capture_cap(path_metadata)?;
    let handle_snapshot = CheckpointArtifactFileSnapshot::capture_cap(handle_metadata)?;
    #[cfg(unix)]
    let invalid_link_count = path_snapshot.link_count != 1 || handle_snapshot.link_count != 1;
    #[cfg(not(unix))]
    let invalid_link_count = false;
    if path_snapshot != handle_snapshot
        || invalid_link_count
        || expected_len.is_some_and(|length| handle_snapshot.byte_len != length)
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact is not one stable regular file with link count one".to_string(),
        ));
    }
    #[cfg(unix)]
    if path_snapshot.mode & 0o077 != 0 {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact permissions are not private".to_string(),
        ));
    }
    #[cfg(unix)]
    if path_snapshot.owner != rustix::process::geteuid().as_raw() {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact is not owned by the effective user".to_string(),
        ));
    }
    Ok(handle_snapshot)
}

fn checkpoint_artifact_staging_name(leaf: &Path) -> String {
    let leaf_digest = checkpoint_artifact_sha256(leaf.as_os_str().as_encoded_bytes());
    format!(".ft-checkpoint-scrollback-{leaf_digest}.staging")
}

fn acquire_checkpoint_artifact_publication_lock(
    parent: &cap_std::fs::Dir,
) -> Result<std::fs::File, CheckpointScrollbackArtifactError> {
    let lock_leaf = Path::new(CHECKPOINT_SCROLLBACK_PUBLICATION_LOCK);
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let cap_file = parent.open_with(lock_leaf, &options)?;
    let handle_metadata = cap_file.metadata()?;
    let path_metadata = parent.symlink_metadata(lock_leaf)?;
    let opened_snapshot =
        validate_checkpoint_artifact_file_metadata(&path_metadata, &handle_metadata, Some(0))?;
    let file = cap_file.into_std();
    if CheckpointArtifactFileSnapshot::capture_std(&file.metadata()?)? != opened_snapshot {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "publication lock changed identity while opening".to_string(),
        ));
    }

    let started = Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < CHECKPOINT_SCROLLBACK_PUBLICATION_LOCK_TIMEOUT =>
            {
                std::thread::sleep(CHECKPOINT_SCROLLBACK_PUBLICATION_LOCK_POLL);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(CheckpointScrollbackArtifactError::PublicationBusy);
            }
            Err(error) => return Err(error.into()),
        }
    }

    let locked_snapshot = CheckpointArtifactFileSnapshot::capture_std(&file.metadata()?)?;
    let locked_path_snapshot =
        CheckpointArtifactFileSnapshot::capture_cap(&parent.symlink_metadata(lock_leaf)?)?;
    if locked_snapshot != opened_snapshot || locked_path_snapshot != locked_snapshot {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "publication lock changed while acquisition was pending".to_string(),
        ));
    }
    file.sync_all()?;
    sync_checkpoint_artifact_directory(parent)?;
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_checkpoint_artifact_noreplace(
    parent: &cap_std::fs::Dir,
    staging: &Path,
    target: &Path,
) -> Result<(), CheckpointScrollbackArtifactError> {
    use rustix::fs::{RenameFlags, renameat_with};

    let parent_file = parent.open(".")?.into_std();
    match renameat_with(
        &parent_file,
        staging,
        &parent_file,
        target,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => {
            Err(CheckpointScrollbackArtifactError::AlreadyExists)
        }
        Err(error) => Err(CheckpointScrollbackArtifactError::Io(
            std::io::Error::from_raw_os_error(error.raw_os_error()),
        )),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn publish_checkpoint_artifact_noreplace(
    _parent: &cap_std::fs::Dir,
    _staging: &Path,
    _target: &Path,
) -> Result<(), CheckpointScrollbackArtifactError> {
    checkpoint_artifact_noreplace_unsupported()
}

#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
fn checkpoint_artifact_noreplace_unsupported() -> Result<(), CheckpointScrollbackArtifactError> {
    Err(CheckpointScrollbackArtifactError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace artifact publication is unsupported on this platform",
    )))
}

fn read_checkpoint_artifact_from_parent_bounded(
    parent: &cap_std::fs::Dir,
    leaf: &Path,
    max_bytes: u64,
    synchronize_file: bool,
) -> Result<Vec<u8>, CheckpointScrollbackArtifactError> {
    read_checkpoint_artifact_from_parent_bounded_with_hook(
        parent,
        leaf,
        max_bytes,
        synchronize_file,
        || {},
    )
}

fn read_checkpoint_artifact_from_parent_bounded_with_hook(
    parent: &cap_std::fs::Dir,
    leaf: &Path,
    max_bytes: u64,
    synchronize_file: bool,
    after_read: impl FnOnce(),
) -> Result<Vec<u8>, CheckpointScrollbackArtifactError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = parent.open_with(leaf, &options)?;
    let before = validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(leaf)?,
        &file.metadata()?,
        None,
    )?;
    if before.byte_len > max_bytes {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
            "artifact has {} bytes, verifier limit {max_bytes}",
            before.byte_len
        )));
    }
    let capacity = usize::try_from(before.byte_len).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact length does not fit this platform".to_string(),
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|error| {
        CheckpointScrollbackArtifactError::ResourceLimit(format!(
            "bounded artifact allocation failed: {error}"
        ))
    })?;
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact grew beyond the verifier limit during read".to_string(),
        ));
    }
    after_read();
    if synchronize_file {
        file.sync_all()?;
    }
    let after = validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(leaf)?,
        &file.metadata()?,
        Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
    )?;
    if before != after {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact changed while it was read".to_string(),
        ));
    }
    Ok(bytes)
}

fn read_checkpoint_artifact_bounded(
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, CheckpointScrollbackArtifactError> {
    let (parent, leaf, parent_path) = checkpoint_artifact_parent_and_leaf(path, false)?;
    let bytes = read_checkpoint_artifact_from_parent_bounded(&parent, &leaf, max_bytes, false)?;
    revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
    Ok(bytes)
}

fn checkpoint_artifact_open_file_matches_expected(
    parent: &cap_std::fs::Dir,
    leaf: &Path,
    file: &mut cap_std::fs::File,
    expected: &[u8],
    synchronize_file: bool,
) -> Result<bool, CheckpointScrollbackArtifactError> {
    checkpoint_artifact_open_file_matches_expected_with_hook(
        parent,
        leaf,
        file,
        expected,
        synchronize_file,
        || {},
    )
}

fn checkpoint_artifact_open_file_matches_expected_with_hook(
    parent: &cap_std::fs::Dir,
    leaf: &Path,
    file: &mut cap_std::fs::File,
    expected: &[u8],
    synchronize_file: bool,
    after_read: impl FnOnce(),
) -> Result<bool, CheckpointScrollbackArtifactError> {
    const COMPARE_BUFFER_BYTES: usize = 16 * 1024;

    let expected_len = u64::try_from(expected.len()).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact length does not fit u64".to_string(),
        )
    })?;
    let before = validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(leaf)?,
        &file.metadata()?,
        None,
    )?;
    if before.byte_len != expected_len {
        return Ok(false);
    }

    file.seek(SeekFrom::Start(0))?;
    let mut buffer = [0_u8; COMPARE_BUFFER_BYTES];
    let mut offset = 0_usize;
    let mut exact = true;
    while offset < expected.len() {
        let remaining = expected.len() - offset;
        let chunk_len = remaining.min(COMPARE_BUFFER_BYTES);
        let read = file.read(&mut buffer[..chunk_len])?;
        if read == 0 {
            exact = false;
            break;
        }
        let end = offset.checked_add(read).ok_or_else(|| {
            CheckpointScrollbackArtifactError::ResourceLimit(
                "artifact comparison offset overflow".to_string(),
            )
        })?;
        if buffer[..read] != expected[offset..end] {
            exact = false;
        }
        offset = end;
    }
    after_read();
    if synchronize_file {
        file.sync_all()?;
    }
    let after = validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(leaf)?,
        &file.metadata()?,
        Some(expected_len),
    )?;
    if before != after {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact changed while it was compared".to_string(),
        ));
    }
    Ok(exact && offset == expected.len())
}

fn checkpoint_artifact_existing_target_matches(
    parent: &cap_std::fs::Dir,
    leaf: &Path,
    bytes: &[u8],
) -> Result<bool, CheckpointScrollbackArtifactError> {
    match parent.symlink_metadata(leaf) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = parent.open_with(leaf, &options)?;
    if !checkpoint_artifact_open_file_matches_expected(parent, leaf, &mut file, bytes, true)? {
        return Err(CheckpointScrollbackArtifactError::AlreadyExists);
    }
    sync_checkpoint_artifact_directory(parent)?;
    Ok(true)
}

/// Compare a retained staging inode with the exact requested payload prefix.
///
/// The return value contains the stable prefix length and its pre-append file
/// snapshot. `None` means the residue is conflicting or overlong; callers must
/// preserve it byte-for-byte. Comparison is streaming and bounded by the
/// already-admitted payload length.
fn checkpoint_artifact_existing_stage_prefix(
    parent: &cap_std::fs::Dir,
    staging: &Path,
    file: &mut cap_std::fs::File,
    expected: &[u8],
) -> Result<Option<(usize, CheckpointArtifactFileSnapshot)>, CheckpointScrollbackArtifactError> {
    const COMPARE_BUFFER_BYTES: usize = 16 * 1024;

    let expected_len = u64::try_from(expected.len()).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact length does not fit u64".to_string(),
        )
    })?;
    let before = validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(staging)?,
        &file.metadata()?,
        None,
    )?;
    if before.byte_len > expected_len {
        let after = validate_checkpoint_artifact_file_metadata(
            &parent.symlink_metadata(staging)?,
            &file.metadata()?,
            Some(before.byte_len),
        )?;
        if before != after {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "artifact staging residue changed while it was inspected".to_string(),
            ));
        }
        return Ok(None);
    }
    let prefix_len = usize::try_from(before.byte_len).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact staging length does not fit this platform".to_string(),
        )
    })?;

    file.seek(SeekFrom::Start(0))?;
    let mut buffer = [0_u8; COMPARE_BUFFER_BYTES];
    let mut offset = 0_usize;
    let mut exact_prefix = true;
    while offset < prefix_len {
        let remaining = prefix_len - offset;
        let chunk_len = remaining.min(COMPARE_BUFFER_BYTES);
        let read = file.read(&mut buffer[..chunk_len])?;
        if read == 0 {
            exact_prefix = false;
            break;
        }
        let end = offset.checked_add(read).ok_or_else(|| {
            CheckpointScrollbackArtifactError::ResourceLimit(
                "artifact staging comparison offset overflow".to_string(),
            )
        })?;
        if buffer[..read] != expected[offset..end] {
            exact_prefix = false;
        }
        offset = end;
    }
    let after = validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(staging)?,
        &file.metadata()?,
        Some(before.byte_len),
    )?;
    if before != after {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact staging residue changed while it was compared".to_string(),
        ));
    }
    if exact_prefix && offset == prefix_len {
        Ok(Some((prefix_len, after)))
    } else {
        Ok(None)
    }
}

fn open_or_resume_checkpoint_artifact_staging(
    parent: &cap_std::fs::Dir,
    staging: &Path,
    bytes: &[u8],
    fault: CheckpointArtifactPublicationFault,
) -> Result<cap_std::fs::File, CheckpointScrollbackArtifactError> {
    let expected_len = u64::try_from(bytes.len()).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact length does not fit u64".to_string(),
        )
    })?;
    let mut create = cap_std::fs::OpenOptions::new();
    create
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;

        create.mode(0o600);
    }
    let (mut file, created) = match parent.open_with(staging, &create) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = cap_std::fs::OpenOptions::new();
            existing.read(true).write(true).follow(FollowSymlinks::No);
            (parent.open_with(staging, &existing)?, false)
        }
        Err(error) => return Err(error.into()),
    };
    let opened_snapshot = validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(staging)?,
        &file.metadata()?,
        created.then_some(0),
    )?;

    let (prefix_len, stable_prefix_snapshot) = if created {
        (0_usize, opened_snapshot)
    } else {
        checkpoint_artifact_existing_stage_prefix(parent, staging, &mut file, bytes)?
            .ok_or(CheckpointScrollbackArtifactError::StagingConflict)?
    };
    if prefix_len == bytes.len() {
        file.sync_all()?;
        let synchronized = validate_checkpoint_artifact_file_metadata(
            &parent.symlink_metadata(staging)?,
            &file.metadata()?,
            Some(expected_len),
        )?;
        if stable_prefix_snapshot != synchronized {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "artifact staging residue changed while it was synchronized".to_string(),
            ));
        }
        return Ok(file);
    }

    file.seek(SeekFrom::Start(u64::try_from(prefix_len).map_err(
        |_| {
            CheckpointScrollbackArtifactError::ResourceLimit(
                "artifact staging append offset does not fit u64".to_string(),
            )
        },
    )?))?;
    let missing = &bytes[prefix_len..];
    if let Some(partial_len) = fault.partial_staging_append_len(missing.len()) {
        file.write_all(&missing[..partial_len])?;
        file.sync_all()?;
        let partial_total_len = prefix_len.checked_add(partial_len).ok_or_else(|| {
            CheckpointScrollbackArtifactError::ResourceLimit(
                "artifact staging partial append length overflow".to_string(),
            )
        })?;
        validate_checkpoint_artifact_file_metadata(
            &parent.symlink_metadata(staging)?,
            &file.metadata()?,
            Some(u64::try_from(partial_total_len).map_err(|_| {
                CheckpointScrollbackArtifactError::ResourceLimit(
                    "artifact staging partial length does not fit u64".to_string(),
                )
            })?),
        )?;
        if checkpoint_artifact_existing_stage_prefix(parent, staging, &mut file, bytes)?.is_none() {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "partially completed artifact staging bytes are not an exact payload prefix"
                    .to_string(),
            ));
        }
        return Err(CheckpointScrollbackArtifactError::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "test interruption after a partial artifact staging append",
        )));
    }
    file.write_all(missing)?;
    file.sync_all()?;
    validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(staging)?,
        &file.metadata()?,
        Some(expected_len),
    )?;
    if !checkpoint_artifact_open_file_matches_expected(parent, staging, &mut file, bytes, false)? {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "completed artifact staging bytes differ from the requested payload".to_string(),
        ));
    }
    Ok(file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointArtifactPublicationOutcome {
    Published,
    AlreadyApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointArtifactPublicationFault {
    None,
    #[cfg(test)]
    AfterPartialStagingAppend,
    #[cfg(test)]
    AfterRenameBeforeDirectorySync,
}

impl CheckpointArtifactPublicationFault {
    fn partial_staging_append_len(self, missing_len: usize) -> Option<usize> {
        #[cfg(not(test))]
        let _ = missing_len;
        match self {
            #[cfg(test)]
            Self::AfterPartialStagingAppend if missing_len > 1 => {
                Some((missing_len / 2).clamp(1, missing_len - 1))
            }
            Self::None => None,
            #[cfg(test)]
            Self::AfterPartialStagingAppend | Self::AfterRenameBeforeDirectorySync => None,
        }
    }

    fn interrupts_after_rename_before_directory_sync(self) -> bool {
        match self {
            Self::None => false,
            #[cfg(test)]
            Self::AfterPartialStagingAppend => false,
            #[cfg(test)]
            Self::AfterRenameBeforeDirectorySync => true,
        }
    }
}

fn publish_checkpoint_artifact_bytes(
    path: &Path,
    bytes: &[u8],
) -> Result<CheckpointArtifactPublicationOutcome, CheckpointScrollbackArtifactError> {
    publish_checkpoint_artifact_bytes_with_fault(
        path,
        bytes,
        CheckpointArtifactPublicationFault::None,
    )
}

fn publish_checkpoint_artifact_bytes_with_fault(
    path: &Path,
    bytes: &[u8],
    fault: CheckpointArtifactPublicationFault,
) -> Result<CheckpointArtifactPublicationOutcome, CheckpointScrollbackArtifactError> {
    let (parent, leaf, parent_path) = checkpoint_artifact_parent_and_leaf(path, true)?;
    let _publication_lock = acquire_checkpoint_artifact_publication_lock(&parent)?;
    revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
    if checkpoint_artifact_existing_target_matches(&parent, &leaf, bytes)? {
        revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
        return Ok(CheckpointArtifactPublicationOutcome::AlreadyApplied);
    }

    let staging_name = checkpoint_artifact_staging_name(&leaf);
    let staging = Path::new(&staging_name);
    let mut file = open_or_resume_checkpoint_artifact_staging(&parent, staging, bytes, fault)?;
    match publish_checkpoint_artifact_noreplace(&parent, staging, &leaf) {
        Ok(()) => {}
        Err(CheckpointScrollbackArtifactError::AlreadyExists) => {
            if checkpoint_artifact_existing_target_matches(&parent, &leaf, bytes)? {
                revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
                return Ok(CheckpointArtifactPublicationOutcome::AlreadyApplied);
            }
            return Err(CheckpointScrollbackArtifactError::AlreadyExists);
        }
        Err(error) => return Err(error),
    }
    if fault.interrupts_after_rename_before_directory_sync() {
        return Err(CheckpointScrollbackArtifactError::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "test interruption after artifact rename and before directory sync",
        )));
    }
    sync_checkpoint_artifact_directory(&parent)?;
    revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
    validate_checkpoint_artifact_file_metadata(
        &parent.symlink_metadata(&leaf)?,
        &file.metadata()?,
        Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
    )?;
    if !checkpoint_artifact_open_file_matches_expected(&parent, &leaf, &mut file, bytes, true)? {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "published artifact bytes differ from the synchronized staging inode".to_string(),
        ));
    }
    Ok(CheckpointArtifactPublicationOutcome::Published)
}

struct PreparedCheckpointArtifactPublication {
    bytes: Vec<u8>,
    checkpoint_id: i64,
    session_id: String,
    checkpoint_at: u64,
    checkpoint_role: String,
    checkpoint_state_hash: String,
    pane_count: usize,
    payload_sha256: String,
    artifact_sha256: String,
    artifact_bytes: u64,
}

fn prepare_checkpoint_scrollback_artifact_publication(
    db_path: &str,
    checkpoint_id: i64,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<PreparedCheckpointArtifactPublication, CheckpointScrollbackArtifactError> {
    let payload = build_checkpoint_scrollback_payload(db_path, checkpoint_id, limits)?;
    validate_checkpoint_scrollback_payload(&payload, limits)?;
    let session_id = payload.checkpoint.session_id.clone();
    let checkpoint_at = payload.checkpoint.checkpoint_at;
    let checkpoint_role = payload.checkpoint.checkpoint_role.clone();
    let checkpoint_state_hash = payload.checkpoint.state_hash.clone();
    let pane_count = payload.summary.pane_count;
    let payload_sha256 = hash_checkpoint_artifact_json(&payload, limits.max_artifact_bytes)?;
    let artifact = CheckpointScrollbackArtifact {
        schema: CHECKPOINT_SCROLLBACK_ARTIFACT_SCHEMA.to_string(),
        publication_state: "complete".to_string(),
        payload_sha256: payload_sha256.clone(),
        payload,
    };
    let bytes = serialize_checkpoint_artifact(&artifact, limits.max_artifact_bytes)?;
    let artifact_sha256 = checkpoint_artifact_sha256(&bytes);
    let artifact_bytes = u64::try_from(bytes.len()).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact length does not fit u64".to_string(),
        )
    })?;

    // The potentially content-heavy producer tree is not allowed to survive
    // into publication.  The returned owner contains only one artifact-sized
    // allocation plus scalar identities needed for the independent reread.
    drop(artifact);
    Ok(PreparedCheckpointArtifactPublication {
        bytes,
        checkpoint_id,
        session_id,
        checkpoint_at,
        checkpoint_role,
        checkpoint_state_hash,
        pane_count,
        payload_sha256,
        artifact_sha256,
        artifact_bytes,
    })
}

fn checkpoint_artifact_identity_matches_expected(
    checkpoint_id: i64,
    session_id: &str,
    checkpoint_at: u64,
    checkpoint_role: &str,
    checkpoint_state_hash: &str,
    pane_count: usize,
    expected: &CheckpointScrollbackArtifactExpectedIdentity,
) -> bool {
    checkpoint_id == expected.checkpoint_id
        && session_id == expected.session_id
        && checkpoint_at == expected.checkpoint_at
        && checkpoint_role == expected.checkpoint_role
        && checkpoint_state_hash == expected.checkpoint_state_hash
        && pane_count == expected.pane_count
}

fn require_checkpoint_artifact_identity_matches_expected(
    checkpoint_id: i64,
    session_id: &str,
    checkpoint_at: u64,
    checkpoint_role: &str,
    checkpoint_state_hash: &str,
    pane_count: usize,
    expected: &CheckpointScrollbackArtifactExpectedIdentity,
) -> Result<(), CheckpointScrollbackArtifactError> {
    if checkpoint_artifact_identity_matches_expected(
        checkpoint_id,
        session_id,
        checkpoint_at,
        checkpoint_role,
        checkpoint_state_hash,
        pane_count,
        expected,
    ) {
        Ok(())
    } else {
        Err(CheckpointScrollbackArtifactError::CheckpointIdentityMismatch)
    }
}

fn require_checkpoint_artifact_receipt_matches_expected(
    receipt: &CheckpointScrollbackArtifactReceipt,
    expected: &CheckpointScrollbackArtifactExpectedIdentity,
) -> Result<(), CheckpointScrollbackArtifactError> {
    require_checkpoint_artifact_identity_matches_expected(
        receipt.checkpoint_id,
        &receipt.session_id,
        receipt.created_at_epoch_ms,
        &receipt.checkpoint_role,
        &receipt.checkpoint_state_hash,
        receipt.pane_count,
        expected,
    )
}

fn write_checkpoint_scrollback_artifact_with_hook_and_expected(
    db_path: &str,
    checkpoint_id: i64,
    output_path: &Path,
    limits: CheckpointScrollbackArtifactLimits,
    expected: Option<&CheckpointScrollbackArtifactExpectedIdentity>,
    after_publication_buffer_drop: impl FnOnce(),
) -> Result<
    (
        CheckpointScrollbackArtifactReceipt,
        CheckpointArtifactPublicationOutcome,
    ),
    CheckpointScrollbackArtifactError,
> {
    let limits = limits.validate()?;
    let prepared =
        prepare_checkpoint_scrollback_artifact_publication(db_path, checkpoint_id, limits)?;
    if let Some(expected) = expected {
        require_checkpoint_artifact_identity_matches_expected(
            prepared.checkpoint_id,
            &prepared.session_id,
            prepared.checkpoint_at,
            &prepared.checkpoint_role,
            &prepared.checkpoint_state_hash,
            prepared.pane_count,
            expected,
        )?;
    }
    let publication = publish_checkpoint_artifact_bytes(output_path, &prepared.bytes)?;
    let expected_checkpoint_id = prepared.checkpoint_id;
    let expected_payload_sha256 = prepared.payload_sha256;
    let expected_artifact_sha256 = prepared.artifact_sha256;
    let expected_artifact_bytes = prepared.artifact_bytes;
    drop(prepared.bytes);
    after_publication_buffer_drop();

    let mut receipt = verify_checkpoint_scrollback_artifact(output_path, limits)?;
    if receipt.payload_sha256 != expected_payload_sha256
        || receipt.artifact_sha256 != expected_artifact_sha256
        || receipt.artifact_bytes != expected_artifact_bytes
        || receipt.checkpoint_id != expected_checkpoint_id
    {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "published artifact reread differs from the constructed source".to_string(),
        ));
    }
    if let Some(expected) = expected {
        require_checkpoint_artifact_receipt_matches_expected(&receipt, expected)?;
    }
    receipt.durability = "file_and_parent_directory_synced_then_offline_verified";
    Ok((receipt, publication))
}

fn write_checkpoint_scrollback_artifact_with_hook(
    db_path: &str,
    checkpoint_id: i64,
    output_path: &Path,
    limits: CheckpointScrollbackArtifactLimits,
    after_publication_buffer_drop: impl FnOnce(),
) -> Result<CheckpointScrollbackArtifactReceipt, CheckpointScrollbackArtifactError> {
    write_checkpoint_scrollback_artifact_with_hook_and_expected(
        db_path,
        checkpoint_id,
        output_path,
        limits,
        None,
        after_publication_buffer_drop,
    )
    .map(|(receipt, _)| receipt)
}

/// Construct, atomically publish, reread, and independently verify one artifact.
///
/// The source database is observed through one pinned read-only SQLite
/// transaction. The final path is never overwritten: bytes are synchronized in
/// a private sibling staging file and published with a no-replace rename before
/// the parent directory is synchronized.
///
/// The producer's memory envelope contains at most the payload tree plus one
/// bounded serialized buffer during construction.  The tree is dropped before
/// publication, and that serialized buffer is dropped before the independent
/// offline verifier rereads the file.
pub fn write_checkpoint_scrollback_artifact(
    db_path: &str,
    checkpoint_id: i64,
    output_path: &Path,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackArtifactReceipt, CheckpointScrollbackArtifactError> {
    write_checkpoint_scrollback_artifact_with_hook(
        db_path,
        checkpoint_id,
        output_path,
        limits,
        || {},
    )
}

fn verify_checkpoint_scrollback_artifact_bytes_with_hook(
    bytes: Vec<u8>,
    limits: CheckpointScrollbackArtifactLimits,
    after_admitted_bytes_drop: impl FnOnce(),
) -> Result<CheckpointScrollbackArtifactReceipt, CheckpointScrollbackArtifactError> {
    let artifact_bytes = u64::try_from(bytes.len()).map_err(|_| {
        CheckpointScrollbackArtifactError::ResourceLimit(
            "artifact length does not fit u64".to_string(),
        )
    })?;
    let artifact_sha256 = checkpoint_artifact_sha256(&bytes);
    verify_checkpoint_artifact_json_structure(&bytes)?;
    let artifact: CheckpointScrollbackArtifact = serde_json::from_slice(&bytes)
        .map_err(|error| checkpoint_artifact_untrusted_json_error("JSON schema", &error))?;
    if !checkpoint_artifact_has_canonical_encoding(&artifact, &bytes, limits.max_artifact_bytes)? {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact is not the one canonical pretty JSON encoding".to_string(),
        ));
    }

    // Canonical comparison is streamed against the admitted slice.  Once it
    // succeeds, the original file buffer is no longer needed: deep semantic,
    // redaction, hash, gap, and topology validation retain only the decoded
    // bounded tree rather than two artifact-sized owners.
    drop(bytes);
    after_admitted_bytes_drop();

    let CheckpointScrollbackArtifact {
        schema,
        publication_state,
        payload_sha256: declared_payload_sha256,
        payload,
    } = artifact;
    if schema != CHECKPOINT_SCROLLBACK_ARTIFACT_SCHEMA || publication_state != "complete" {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact schema or publication state is unsupported".to_string(),
        ));
    }
    let payload_sha256 = hash_checkpoint_artifact_json(&payload, limits.max_artifact_bytes)?;
    if declared_payload_sha256 != payload_sha256 {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact payload checksum mismatch".to_string(),
        ));
    }
    validate_checkpoint_scrollback_payload(&payload, limits)?;
    Ok(CheckpointScrollbackArtifactReceipt {
        schema,
        capabilities: payload.capabilities,
        checkpoint_id: payload.checkpoint.checkpoint_id,
        session_id: payload.checkpoint.session_id.clone(),
        checkpoint_role: payload.checkpoint.checkpoint_role.clone(),
        checkpoint_state_hash: payload.checkpoint.state_hash.clone(),
        created_at_epoch_ms: payload.created_at_epoch_ms,
        payload_sha256,
        artifact_sha256,
        artifact_bytes,
        pane_count: payload.summary.pane_count,
        segment_count: payload.summary.segment_count,
        content_bytes: payload.summary.content_bytes,
        complete_pane_count: payload.summary.complete_pane_count,
        durability: "private_regular_file_verified_offline",
    })
}

/// Verify a private artifact without opening the source database or touching a mux.
///
/// Peak verifier ownership is the bounded admitted file buffer plus the decoded
/// tree. Canonical equality is checked by a streaming serializer comparator,
/// never by constructing a third artifact-sized buffer; the file buffer is
/// then dropped before semantic validation.
pub fn verify_checkpoint_scrollback_artifact(
    path: &Path,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackArtifactReceipt, CheckpointScrollbackArtifactError> {
    let limits = limits.validate()?;
    let bytes = read_checkpoint_artifact_bounded(path, limits.max_artifact_bytes)?;
    verify_checkpoint_scrollback_artifact_bytes_with_hook(bytes, limits, || {})
}

/// Recover one already-published target without consulting the source database.
///
/// The publication lock serializes this durability recovery with cooperating
/// publishers. A target that exists but is malformed, unsafe, or unverifiable
/// is an error rather than an invitation to rebuild or overwrite it.
fn recover_existing_checkpoint_scrollback_artifact(
    path: &Path,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<Option<CheckpointScrollbackArtifactReceipt>, CheckpointScrollbackArtifactError> {
    let (parent, leaf, parent_path) = match checkpoint_artifact_parent_and_leaf(path, false) {
        Ok(authority) => authority,
        Err(CheckpointScrollbackArtifactError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let _publication_lock = acquire_checkpoint_artifact_publication_lock(&parent)?;
    revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
    match parent.symlink_metadata(&leaf) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }

    let bytes = read_checkpoint_artifact_from_parent_bounded(
        &parent,
        &leaf,
        limits.max_artifact_bytes,
        true,
    )?;
    sync_checkpoint_artifact_directory(&parent)?;
    revalidate_checkpoint_artifact_parent(&parent_path, &parent)?;
    let mut receipt = verify_checkpoint_scrollback_artifact_bytes_with_hook(bytes, limits, || {})?;
    receipt.durability = "file_and_parent_directory_synced_then_offline_verified";
    Ok(Some(receipt))
}

/// Derive the canonical production leaf name for a verified checkpoint identity.
pub fn checkpoint_scrollback_artifact_file_name(
    checkpoint_at: u64,
    checkpoint_id: i64,
    state_hash: &str,
) -> Result<String, CheckpointScrollbackArtifactError> {
    let digest = state_hash
        .strip_prefix(SNAPSHOT_WITNESS_PREFIX)
        .ok_or_else(|| {
            CheckpointScrollbackArtifactError::InvalidArtifact(
                "checkpoint state hash is not a v2 snapshot witness".to_string(),
            )
        })?;
    if !checkpoint_artifact_is_lower_hex_digest(digest) || checkpoint_id <= 0 {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "checkpoint identity cannot form a canonical artifact name".to_string(),
        ));
    }
    Ok(format!(
        "checkpoint-{checkpoint_at}-{checkpoint_id}-{digest}{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
    ))
}

/// Publish or recover the canonical durable artifact for an expected identity.
///
/// Recovery is attempted before the source database is opened, so retrying a
/// lost publication reply still succeeds after the source checkpoint or its
/// scrollback rows are unavailable. An existing target must verify offline and
/// match the requested checkpoint ID, session ID, timestamp, role, v2 witness,
/// and pane count.
/// When no target exists, the same identity is checked against the pinned source
/// projection before no-clobber publication and again after the independent
/// reread. The returned receipt exposes complete and incomplete pane coverage;
/// callers that require a complete export must enforce `scrollback_complete()`.
pub fn publish_or_recover_checkpoint_scrollback_artifact_for_identity(
    db_path: &str,
    expected: &CheckpointScrollbackArtifactExpectedIdentity,
    artifact_directory: &Path,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackArtifactPublication, CheckpointScrollbackArtifactError> {
    let leaf = checkpoint_scrollback_artifact_file_name(
        expected.checkpoint_at,
        expected.checkpoint_id,
        &expected.checkpoint_state_hash,
    )?;
    let path = artifact_directory.join(leaf);
    publish_or_recover_checkpoint_scrollback_artifact_at_path_for_identity(
        db_path, expected, &path, limits,
    )
}

/// Publish or recover an exact output path for an expected checkpoint identity.
///
/// Unlike the canonical-directory API, this accepts an operator-selected leaf
/// path. It retains the same fail-closed recovery contract: an existing target
/// is synchronized, verified offline, and matched against all six expected
/// identity fields before the source database is consulted. An absent target is
/// matched against the pinned source projection before no-clobber publication
/// and again after the independent reread.
pub fn publish_or_recover_checkpoint_scrollback_artifact_at_path_for_identity(
    db_path: &str,
    expected: &CheckpointScrollbackArtifactExpectedIdentity,
    output_path: &Path,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackArtifactPublication, CheckpointScrollbackArtifactError> {
    let limits = limits.validate()?;
    let path = output_path.to_path_buf();

    if let Some(receipt) = recover_existing_checkpoint_scrollback_artifact(&path, limits)? {
        require_checkpoint_artifact_receipt_matches_expected(&receipt, expected)?;
        return Ok(CheckpointScrollbackArtifactPublication {
            path,
            resolution: CheckpointScrollbackArtifactResolution::RecoveredExisting,
            receipt,
        });
    }

    let (receipt, outcome) = write_checkpoint_scrollback_artifact_with_hook_and_expected(
        db_path,
        expected.checkpoint_id,
        &path,
        limits,
        Some(expected),
        || {},
    )?;
    let resolution = match outcome {
        CheckpointArtifactPublicationOutcome::Published => {
            CheckpointScrollbackArtifactResolution::Published
        }
        CheckpointArtifactPublicationOutcome::AlreadyApplied => {
            CheckpointScrollbackArtifactResolution::RecoveredExisting
        }
    };
    Ok(CheckpointScrollbackArtifactPublication {
        path,
        resolution,
        receipt,
    })
}

/// Publish or recover the canonical durable artifact for a live capture result.
///
/// This convenience wrapper derives the exact expected identity, including the
/// snapshot role and mux-session binding, then delegates to
/// [`publish_or_recover_checkpoint_scrollback_artifact_for_identity`].
pub fn publish_or_recover_checkpoint_scrollback_artifact(
    db_path: &str,
    snapshot: &SnapshotResult,
    artifact_directory: &Path,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackArtifactPublication, CheckpointScrollbackArtifactError> {
    let expected = CheckpointScrollbackArtifactExpectedIdentity::from_snapshot_result(snapshot);
    publish_or_recover_checkpoint_scrollback_artifact_for_identity(
        db_path,
        &expected,
        artifact_directory,
        limits,
    )
}

fn checkpoint_artifact_is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Inventory and independently verify a bounded dedicated artifact directory.
///
/// Every directory entry consumes the inventory budget, including unrelated or
/// retained staging names, so junk cannot make enumeration unbounded. Only
/// canonical-suffix files are interpreted as published artifacts; any such
/// file that fails verification makes the inventory fail closed.
pub fn inventory_checkpoint_scrollback_artifacts(
    directory: &Path,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<Vec<CheckpointScrollbackInventoryEntry>, CheckpointScrollbackArtifactError> {
    let limits = limits.validate()?;
    let pinned = open_checkpoint_artifact_directory_nofollow(directory)?;
    let pinned_metadata = pinned.dir_metadata()?;
    if !pinned_metadata.is_dir() {
        return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
            "artifact inventory root is not a directory".to_string(),
        ));
    }
    let mut observed_entries = 0_usize;
    let mut artifacts = Vec::new();
    let mut checkpoint_identities = HashSet::new();
    let mut artifact_identities = HashSet::new();
    for entry in pinned.entries()? {
        let entry = entry?;
        observed_entries = observed_entries.checked_add(1).ok_or_else(|| {
            CheckpointScrollbackArtifactError::ResourceLimit(
                "artifact inventory entry count overflow".to_string(),
            )
        })?;
        if observed_entries > limits.max_inventory_entries {
            return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
                "artifact directory exceeds the {} entry inventory limit",
                limits.max_inventory_entries
            )));
        }
        let file_name = entry.file_name();
        let Some(file_name_utf8) = file_name.to_str() else {
            continue;
        };
        if !file_name_utf8.ends_with(CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX) {
            continue;
        }
        let bytes = read_checkpoint_artifact_from_parent_bounded(
            &pinned,
            Path::new(file_name.as_os_str()),
            limits.max_artifact_bytes,
            false,
        )?;
        let receipt = verify_checkpoint_scrollback_artifact_bytes_with_hook(bytes, limits, || {})?;
        let canonical_file_name = checkpoint_scrollback_artifact_file_name(
            receipt.created_at_epoch_ms,
            receipt.checkpoint_id,
            &receipt.checkpoint_state_hash,
        )?;
        if file_name_utf8 != canonical_file_name {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "verified artifact does not use its canonical checkpoint leaf name".to_string(),
            ));
        }
        if !checkpoint_identities.insert(receipt.checkpoint_id) {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "artifact inventory contains a duplicate or forked checkpoint identity".to_string(),
            ));
        }
        if !artifact_identities.insert(receipt.artifact_sha256.clone()) {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "artifact inventory contains a duplicate file identity".to_string(),
            ));
        }
        artifacts.push(CheckpointScrollbackInventoryEntry {
            file_name: PathBuf::from(file_name),
            created_at_epoch_ms: receipt.created_at_epoch_ms,
            checkpoint_id: receipt.checkpoint_id,
            checkpoint_state_hash: receipt.checkpoint_state_hash,
            artifact_bytes: receipt.artifact_bytes,
            artifact_sha256: receipt.artifact_sha256,
        });
    }
    revalidate_checkpoint_artifact_parent(directory, &pinned)?;
    artifacts.sort_unstable_by(|left, right| {
        right
            .created_at_epoch_ms
            .cmp(&left.created_at_epoch_ms)
            .then_with(|| right.checkpoint_id.cmp(&left.checkpoint_id))
            .then_with(|| left.file_name.cmp(&right.file_name))
    });
    Ok(artifacts)
}

/// Build a bounded, side-effect-free newest-first retention plan.
///
/// Once the newest prefix would exceed either the count or byte budget, that
/// entry and every older entry are marked for retirement. Applying deletion is
/// intentionally a separate production wiring step so planning can be audited
/// before any artifact is removed.
pub fn plan_checkpoint_scrollback_artifact_retention(
    entries: &[CheckpointScrollbackInventoryEntry],
    retention_count: usize,
    max_retained_bytes: u64,
    limits: CheckpointScrollbackArtifactLimits,
) -> Result<CheckpointScrollbackRetentionPlan, CheckpointScrollbackArtifactError> {
    let limits = limits.validate()?;
    if retention_count == 0 || max_retained_bytes == 0 {
        return Err(CheckpointScrollbackArtifactError::InvalidLimits(
            "retention must preserve at least one recovery artifact".to_string(),
        ));
    }
    if entries.len() > limits.max_inventory_entries {
        return Err(CheckpointScrollbackArtifactError::ResourceLimit(format!(
            "retention input has {} entries, limit {}",
            entries.len(),
            limits.max_inventory_entries
        )));
    }
    let mut names = HashSet::with_capacity(entries.len());
    let mut checkpoint_identities = HashSet::with_capacity(entries.len());
    let mut artifact_identities = HashSet::with_capacity(entries.len());
    for entry in entries {
        let canonical_file_name = checkpoint_scrollback_artifact_file_name(
            entry.created_at_epoch_ms,
            entry.checkpoint_id,
            &entry.checkpoint_state_hash,
        )?;
        if entry.file_name != Path::new(&canonical_file_name)
            || entry.artifact_bytes == 0
            || !checkpoint_artifact_is_lower_hex_digest(&entry.artifact_sha256)
        {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "retention entry identity is not canonical".to_string(),
            ));
        }
        if entry.artifact_bytes > limits.max_artifact_bytes {
            return Err(CheckpointScrollbackArtifactError::ResourceLimit(
                "retention entry exceeds the verifier artifact-byte limit".to_string(),
            ));
        }
        if !names.insert(entry.file_name.clone())
            || !checkpoint_identities.insert(entry.checkpoint_id)
            || !artifact_identities.insert(entry.artifact_sha256.clone())
        {
            return Err(CheckpointScrollbackArtifactError::InvalidArtifact(
                "retention inventory contains duplicate or forked identities".to_string(),
            ));
        }
    }

    let mut sorted = entries.to_vec();
    sorted.sort_unstable_by(|left, right| {
        right
            .created_at_epoch_ms
            .cmp(&left.created_at_epoch_ms)
            .then_with(|| right.checkpoint_id.cmp(&left.checkpoint_id))
            .then_with(|| left.file_name.cmp(&right.file_name))
    });
    if sorted
        .first()
        .is_some_and(|newest| newest.artifact_bytes > max_retained_bytes)
    {
        return Err(CheckpointScrollbackArtifactError::InvalidLimits(
            "retained-byte budget cannot preserve the newest recovery artifact".to_string(),
        ));
    }

    let mut retain = Vec::new();
    let mut retire = Vec::new();
    let mut retained_bytes = 0_u64;
    let mut newest_prefix_open = true;
    for entry in sorted {
        let next_bytes = retained_bytes.checked_add(entry.artifact_bytes);
        let retain_entry = newest_prefix_open
            && retain.len() < retention_count
            && next_bytes.is_some_and(|bytes| bytes <= max_retained_bytes);
        if retain_entry {
            retained_bytes = next_bytes.expect("checked by retain predicate");
            retain.push(entry.file_name);
        } else {
            newest_prefix_open = false;
            retire.push(entry.file_name);
        }
    }
    Ok(CheckpointScrollbackRetentionPlan {
        retain,
        retire,
        retained_bytes,
    })
}

#[cfg(test)]
fn cleanup_sync(
    db_path: &str,
    retention_count: usize,
    retention_days: u64,
) -> std::result::Result<usize, rusqlite::Error> {
    cleanup_authoritatively_sync(db_path, retention_count, retention_days)
        .map_err(SnapshotAuthorityDbError::into_primary_source)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder, sleep, timeout};
    use crate::wezterm::PaneSize;

    fn complete_recovery_evidence() -> SnapshotRecoveryEvidence {
        SnapshotRecoveryEvidence {
            artifact_kind: SnapshotRecoveryArtifactKind::WholeMuxRecoveryImage,
            artifact_validity: SnapshotRecoveryVerdict::Verified,
            repair_status: SnapshotRecoveryRepairStatus::NotRepaired,
            semantics: SnapshotRecoverySemantics::WholeMuxComplete,
            compatibility: SnapshotRecoveryVerdict::Verified,
            topology_authority: SnapshotRecoveryVerdict::Verified,
            guardian_census: SnapshotRecoveryVerdict::Verified,
            lease_replay_input_authority: SnapshotRecoveryVerdict::Verified,
            process_replacement_approval: SnapshotRecoveryVerdict::Verified,
            durability: SnapshotRecoveryDurabilityGrade::OffsiteVerified,
            freshness: SnapshotRecoveryFreshness::Verified,
            scrub_coverage: SnapshotRecoveryScrubCoverage::Current,
            drill_currency: SnapshotRecoveryDrillCurrency::Current,
            client_state: SnapshotRecoveryClientStateDisposition::PreservedAndVerified,
        }
    }

    #[test]
    fn snapshot_recovery_policy_keeps_unproven_objectives_unset() {
        let policy = SnapshotRecoveryPolicy::default().validate().unwrap();
        assert_eq!(
            policy.periodic_interval_secs,
            SnapshotConfig::default().interval_seconds
        );
        assert_eq!(policy.target_local_rpo_secs, policy.periodic_interval_secs);
        assert_eq!(policy.target_replica_rpo_secs, None);
        assert_eq!(policy.max_full_anchor_age_secs, None);
        assert_eq!(policy.target_interactive_safe_rto_secs, None);
        assert_eq!(policy.target_complete_rto_secs, None);
        assert_eq!(policy.max_freshness_witness_age_secs, None);
        assert_eq!(policy.max_shallow_scrub_age_secs, None);
        assert_eq!(policy.max_deep_scrub_age_secs, None);
        assert_eq!(policy.max_disaster_drill_age_secs, None);

        let mut invalid = policy;
        invalid.target_local_rpo_secs = invalid.periodic_interval_secs - 1;
        assert_eq!(
            invalid.validate(),
            Err(SnapshotRecoveryPolicyError::LocalRpoBelowInterval)
        );
        invalid = policy;
        invalid.target_replica_rpo_secs = Some(invalid.target_local_rpo_secs - 1);
        assert_eq!(
            invalid.validate(),
            Err(SnapshotRecoveryPolicyError::ReplicaRpoBelowLocalRpo)
        );
        invalid = policy;
        invalid.target_interactive_safe_rto_secs = Some(60);
        invalid.target_complete_rto_secs = Some(59);
        assert_eq!(
            invalid.validate(),
            Err(SnapshotRecoveryPolicyError::CompleteRtoBelowInteractiveRto)
        );
    }

    #[test]
    fn snapshot_recovery_contract_matrix_is_total_and_nonclaiming() {
        let mut cells = HashSet::new();
        for failure in SnapshotRecoveryFailureClass::ALL {
            let row = snapshot_recovery_failure_contract(failure);
            assert_eq!(row.failure, failure);
            assert!(!row.recoverable_point.is_empty());
            assert!(!row.rpo_scope.is_empty());
            assert!(!row.rto_scope.is_empty());
            assert!(!row.automation.is_empty());
            assert!(!row.mutation.is_empty());
            assert!(!row.required_evidence.is_empty());
            assert!(!row.terminal_outcome.is_empty());
            assert!(!row.nonclaim.is_empty());
            serde_json::to_value(row).expect("failure row must remain serializable");

            for capability in SnapshotRecoveryCapability::ALL {
                let cell = snapshot_recovery_contract_cell(failure, capability);
                assert_eq!(cell.failure, failure);
                assert_eq!(cell.capability, capability);
                assert!(!cell.nonclaim.is_empty());
                assert!(cells.insert((failure, capability)));
                serde_json::to_value(cell).expect("contract cell must remain serializable");
            }
        }
        assert_eq!(
            cells.len(),
            SnapshotRecoveryFailureClass::ALL.len() * SnapshotRecoveryCapability::ALL.len()
        );

        let mux_crash = snapshot_recovery_failure_contract(
            SnapshotRecoveryFailureClass::MuxCrash,
        );
        let host_power_loss = snapshot_recovery_failure_contract(
            SnapshotRecoveryFailureClass::FullHostPowerLoss,
        );
        assert_ne!(mux_crash.required_evidence, host_power_loss.required_evidence);
        assert!(mux_crash.nonclaim.contains("not host power loss"));
        assert!(host_power_loss.nonclaim.contains("do not execute"));
        assert_eq!(
            snapshot_recovery_contract_cell(
                SnapshotRecoveryFailureClass::FullHostPowerLoss,
                SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
            )
            .availability,
            SnapshotRecoveryCapabilityAvailability::Forbidden
        );
    }

    #[test]
    fn snapshot_recovery_forensic_artifacts_cannot_be_promoted() {
        for evidence in [
            SnapshotRecoveryEvidence::verified_mux_forensic_dump(false),
            SnapshotRecoveryEvidence::verified_mux_forensic_dump(true),
            SnapshotRecoveryEvidence::verified_checkpoint_scrollback_export(false),
            SnapshotRecoveryEvidence::verified_checkpoint_scrollback_export(true),
        ] {
            let receipt = evidence
                .validate_claim(
                    SnapshotRecoveryCapability::ForensicContentExport,
                    SnapshotRecoveryReadiness::Candidate,
                )
                .unwrap();
            assert_eq!(
                receipt.capability(),
                SnapshotRecoveryCapability::ForensicContentExport
            );
            assert_eq!(receipt.readiness(), SnapshotRecoveryReadiness::Candidate);
            assert!(!receipt.mutation_permitted());

            for capability in [
                SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
                SnapshotRecoveryCapability::TopologyLayoutRecreation,
                SnapshotRecoveryCapability::PolicyGatedProcessReplacement,
            ] {
                assert_eq!(
                    evidence.validate_claim(capability, SnapshotRecoveryReadiness::Candidate),
                    Err(SnapshotRecoveryClaimError::ForensicPromotionForbidden)
                );
            }
            for readiness in [
                SnapshotRecoveryReadiness::InteractiveSafe,
                SnapshotRecoveryReadiness::Complete,
            ] {
                assert_eq!(
                    evidence.validate_claim(
                        SnapshotRecoveryCapability::ForensicContentExport,
                        readiness,
                    ),
                    Err(SnapshotRecoveryClaimError::ForensicPromotionForbidden)
                );
            }
        }

        let mut repaired = SnapshotRecoveryEvidence::verified_mux_forensic_dump(true);
        repaired.repair_status = SnapshotRecoveryRepairStatus::RepairedUnverified;
        assert_eq!(
            repaired.validate_claim(
                SnapshotRecoveryCapability::ForensicContentExport,
                SnapshotRecoveryReadiness::Candidate,
            ),
            Err(SnapshotRecoveryClaimError::RepairNotReverified)
        );
    }

    #[test]
    fn snapshot_recovery_claim_guard_rejects_each_missing_independent_fact() {
        let baseline = complete_recovery_evidence();
        let receipt = baseline
            .validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            )
            .unwrap();
        assert_eq!(receipt.readiness(), SnapshotRecoveryReadiness::Complete);
        assert!(receipt.mutation_permitted());

        let mut changed = baseline;
        changed.artifact_validity = SnapshotRecoveryVerdict::Unknown;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::ArtifactNotVerified)
        );
        changed = baseline;
        changed.repair_status = SnapshotRecoveryRepairStatus::RepairedUnverified;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::RepairNotReverified)
        );
        changed = baseline;
        changed.compatibility = SnapshotRecoveryVerdict::Unknown;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::CompatibilityNotVerified)
        );
        changed = baseline;
        changed.semantics = SnapshotRecoverySemantics::TerminalStateComplete;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::WholeMuxSemanticsIncomplete)
        );
        changed = baseline;
        changed.durability = SnapshotRecoveryDurabilityGrade::Unverified;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::DurabilityNotVerified)
        );
        for freshness in [
            SnapshotRecoveryFreshness::Unknown,
            SnapshotRecoveryFreshness::Stale,
            SnapshotRecoveryFreshness::Conflict,
        ] {
            changed = baseline;
            changed.freshness = freshness;
            assert_eq!(
                changed.validate_release_claim(
                    SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
                ),
                Err(SnapshotRecoveryClaimError::FreshnessNotVerified)
            );
        }
        changed = baseline;
        changed.topology_authority = SnapshotRecoveryVerdict::Unknown;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::WholeMuxSemanticsIncomplete)
        );
        changed = baseline;
        changed.scrub_coverage = SnapshotRecoveryScrubCoverage::Overdue;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::ScrubCoverageNotCurrent)
        );
        changed = baseline;
        changed.drill_currency = SnapshotRecoveryDrillCurrency::Overdue;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::DisasterDrillNotCurrent)
        );

        changed = baseline;
        changed.guardian_census = SnapshotRecoveryVerdict::Unknown;
        assert_eq!(
            changed.validate_claim(
                SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
                SnapshotRecoveryReadiness::InteractiveSafe,
            ),
            Err(SnapshotRecoveryClaimError::GuardianCensusNotVerified)
        );
        changed = baseline;
        changed.lease_replay_input_authority = SnapshotRecoveryVerdict::Unknown;
        assert_eq!(
            changed.validate_claim(
                SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
                SnapshotRecoveryReadiness::InteractiveSafe,
            ),
            Err(SnapshotRecoveryClaimError::MutationAuthorityNotVerified)
        );
        changed = baseline;
        changed.process_replacement_approval = SnapshotRecoveryVerdict::Unknown;
        assert_eq!(
            changed.validate_claim(
                SnapshotRecoveryCapability::PolicyGatedProcessReplacement,
                SnapshotRecoveryReadiness::Complete,
            ),
            Err(SnapshotRecoveryClaimError::ProcessReplacementNotApproved)
        );

        changed = baseline;
        changed.client_state = SnapshotRecoveryClientStateDisposition::Conflict;
        let serialized = serde_json::to_value(changed).unwrap();
        assert_eq!(serialized["client_state"], "conflict");
        assert!(changed
            .validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            )
            .is_ok());
        assert_eq!(changed.client_state, SnapshotRecoveryClientStateDisposition::Conflict);
    }

    #[test]
    fn snapshot_recovery_contract_proof_manifest_is_self_consistent() {
        let mut invariant_ids = HashSet::new();
        let mut filters = HashSet::new();
        for entry in SNAPSHOT_RECOVERY_CONTRACT_PROOF_MANIFEST {
            assert_eq!(entry.owner_bead, SNAPSHOT_RECOVERY_CONTRACT_OWNER);
            assert!(invariant_ids.insert(entry.invariant_id));
            assert!(filters.insert(entry.exact_filter_or_scenario));
            for field in [
                entry.fixture_or_oracle,
                entry.assertion,
                entry.package_or_script,
                entry.exact_filter_or_scenario,
                entry.test_layer,
                entry.platform,
                entry.required_artifacts,
                entry.causal_fault_or_mutation,
            ] {
                assert!(!field.is_empty());
            }
            serde_json::to_value(entry).expect("proof entry must remain serializable");
        }
        assert_eq!(SNAPSHOT_RECOVERY_CONTRACT_PROOF_MANIFEST.len(), 5);
        assert!(filters.contains("snapshot_contract_clean_host_progressive_recovery"));
    }

    type TestPaneProviderFuture = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>>
                + Send,
        >,
    >;

    #[test]
    fn sqlite_integer_conversions_reject_wrapping_values_and_corrupt_rows() {
        let sqlite_max_as_u64 = u64::try_from(i64::MAX).unwrap();
        assert_eq!(u64_to_sqlite_integer(sqlite_max_as_u64).unwrap(), i64::MAX);
        assert!(u64_to_sqlite_integer(u64::MAX).is_err());
        assert!(sqlite_integer_to_u64(0, -1).is_err());
        assert_eq!(
            sqlite_integer_to_u64(0, i64::MAX).unwrap(),
            sqlite_max_as_u64
        );

        #[cfg(target_pointer_width = "64")]
        {
            assert!(usize_to_sqlite_integer(usize::MAX).is_err());
        }
    }

    #[test]
    fn scrollback_loader_rejects_corrupt_summary_metadata_and_oversized_pane_ids() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap();
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE pane_scrollback_summary (
                 pane_id INTEGER NOT NULL,
                 retained_segment_count INTEGER NOT NULL,
                 first_seq INTEGER,
                 last_seq INTEGER,
                 first_captured_at INTEGER,
                 last_captured_at INTEGER
             );
             INSERT INTO pane_scrollback_summary (
                 pane_id, retained_segment_count, first_seq, last_seq,
                 first_captured_at, last_captured_at
             ) VALUES (7, 2, 0, 1, -1, 2);",
        )
        .unwrap();
        drop(conn);

        let negative_timestamp = load_latest_scrollback_refs_sync(db_path, &[7])
            .expect_err("a negative first_captured_at must not wrap to u64");
        assert!(negative_timestamp.contains("first_capture_at=-1"));

        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE pane_scrollback_summary
             SET first_captured_at = 1, first_seq = -1
             WHERE pane_id = 7;",
            [],
        )
        .unwrap();
        drop(conn);
        let negative_sequence = load_latest_scrollback_refs_sync(db_path, &[7])
            .expect_err("a masked negative scrollback sequence must fail closed");
        assert!(negative_sequence.contains("first_seq=-1"));

        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE pane_scrollback_summary
             SET retained_segment_count = 0, first_seq = 0
             WHERE pane_id = 7",
            [],
        )
        .unwrap();
        drop(conn);
        let impossible_empty = load_latest_scrollback_refs_sync(db_path, &[7])
            .expect_err("a zero-count summary with bounds must fail closed");
        assert!(impossible_empty.contains("invalid empty scrollback summary"));

        let oversized_pane_id = load_latest_scrollback_refs_sync(db_path, &[u64::MAX])
            .expect_err("an unrepresentable pane id must not wrap to a SQLite integer");
        assert!(oversized_pane_id.contains("exceeds sqlite integer range"));
    }

    #[test]
    fn observation_connection_never_creates_a_missing_database() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("observation-must-not-create.db");
        assert!(!missing.exists());
        assert!(open_snapshot_query_conn(missing.to_str().unwrap()).is_err());
        assert!(
            !missing.exists(),
            "an observation-only open must not materialize a database file"
        );
    }

    #[test]
    fn observation_connection_cannot_mutate_an_existing_database() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap();
        let writer = Connection::open(db_path).unwrap();
        writer
            .execute_batch("CREATE TABLE observation_guard (value INTEGER NOT NULL);")
            .unwrap();
        drop(writer);

        let reader = open_snapshot_query_conn(db_path).unwrap();
        assert!(
            reader
                .execute("INSERT INTO observation_guard (value) VALUES (1)", [])
                .is_err(),
            "snapshot observation connections must be enforced read-only by SQLite"
        );
        let count: i64 = reader
            .query_row("SELECT COUNT(*) FROM observation_guard", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn checkpoint_deletion_rejects_oversized_durable_identity_before_delete() {
        for corrupt_field in ["session_id", "checkpoint_role", "state_hash"] {
            let (_tmp, db_path) = setup_test_db();
            let session_id = if corrupt_field == "session_id" {
                "s".repeat(MAX_CHECKPOINT_SESSION_ID_BYTES + 1)
            } else {
                "sess-bounded-delete".to_string()
            };
            create_session_sync(
                db_path.as_str(),
                &session_id,
                1_000,
                r#"{"version":"initial"}"#,
                crate::VERSION,
            )
            .unwrap();
            let checkpoint_id = insert_checkpoint_fixture(
                db_path.as_str(),
                &session_id,
                2_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "snp2:fixture",
                Some(r#"{"version":"initial"}"#),
                0,
            );
            let conn = Connection::open(db_path.as_str()).unwrap();
            match corrupt_field {
                "checkpoint_role" => {
                    conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
                        .unwrap();
                    conn.execute(
                        "UPDATE session_checkpoints SET checkpoint_role = ?2 WHERE id = ?1",
                        rusqlite::params![checkpoint_id, "r".repeat(MAX_CHECKPOINT_ROLE_BYTES + 1)],
                    )
                    .unwrap();
                    conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
                        .unwrap();
                }
                "state_hash" => {
                    conn.execute(
                        "UPDATE session_checkpoints SET state_hash = ?2 WHERE id = ?1",
                        rusqlite::params![
                            checkpoint_id,
                            "h".repeat(MAX_CHECKPOINT_STATE_HASH_BYTES + 1)
                        ],
                    )
                    .unwrap();
                }
                "session_id" => {}
                _ => unreachable!("fixture field is exhaustive"),
            }
            drop(conn);

            delete_checkpoint_authoritatively_sync(
                db_path.as_str(),
                &SnapshotDeleteTarget::Id(checkpoint_id),
            )
            .expect_err("oversized durable identity must fail before destructive DML");
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                1,
                "corrupt {corrupt_field} must leave the checkpoint intact"
            );
        }
    }

    #[test]
    fn cleanup_rejects_oversized_session_identity_before_delete() {
        let (_tmp, db_path) = setup_test_db();
        let oversized_session_id = "s".repeat(MAX_CHECKPOINT_SESSION_ID_BYTES + 1);
        create_session_sync(
            db_path.as_str(),
            &oversized_session_id,
            1_000,
            r#"{"version":"initial"}"#,
            crate::VERSION,
        )
        .unwrap();
        insert_checkpoint_fixture(
            db_path.as_str(),
            &oversized_session_id,
            2_000,
            CHECKPOINT_ROLE_SNAPSHOT,
            "snp2:fixture",
            Some(r#"{"version":"initial"}"#),
            0,
        );

        cleanup_authoritatively_sync(db_path.as_str(), 0, 365)
            .expect_err("cleanup must reject oversized affected-session identity before DML");
        assert_eq!(checkpoint_count(db_path.as_str()), 1);
    }

    #[test]
    fn auxiliary_snapshot_queries_chunk_large_pane_sets() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap();
        let mut conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                 id INTEGER PRIMARY KEY,
                 pane_id INTEGER NOT NULL,
                 rule_id TEXT NOT NULL,
                 agent_type TEXT NOT NULL,
                 extracted TEXT,
                 detected_at INTEGER NOT NULL
             );
             CREATE TABLE pane_scrollback_summary (
                 pane_id INTEGER NOT NULL,
                 retained_segment_count INTEGER NOT NULL,
                 first_seq INTEGER,
                 last_seq INTEGER,
                 first_captured_at INTEGER,
                 last_captured_at INTEGER
             );",
        )
        .unwrap();
        let pane_count = SNAPSHOT_SQLITE_IN_LIST_CHUNK + 17;
        let tx = conn.transaction().unwrap();
        for pane_id in 0..pane_count {
            let pane_id = i64::try_from(pane_id).unwrap();
            tx.execute(
                "INSERT INTO events
                 (pane_id, rule_id, agent_type, extracted, detected_at)
                 VALUES (?1, 'codex-working', 'codex', '{}', 10)",
                [pane_id],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO pane_scrollback_summary (
                     pane_id, retained_segment_count, first_seq, last_seq,
                     first_captured_at, last_captured_at
                 ) VALUES (?1, 1, 0, 0, 10, 10)",
                [pane_id],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        drop(conn);

        let pane_ids: Vec<u64> = (0..u64::try_from(pane_count).unwrap()).collect();
        let detections = load_latest_detections_by_pane_sync(db_path, &pane_ids, 0).unwrap();
        let scrollback = load_latest_scrollback_refs_sync(db_path, &pane_ids).unwrap();
        assert_eq!(detections.len(), pane_count);
        assert_eq!(scrollback.len(), pane_count);
        assert!(
            scrollback
                .values()
                .all(|reference| reference.output_segments_seq == 0)
        );
        assert!(
            scrollback
                .values()
                .all(|reference| reference.retained_segment_count == 1)
        );
    }

    #[test]
    fn detection_projection_skips_oversized_identity_and_bounds_extracted_json() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap();
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                 id INTEGER PRIMARY KEY,
                 pane_id INTEGER NOT NULL,
                 rule_id TEXT NOT NULL,
                 agent_type TEXT NOT NULL,
                 extracted,
                 detected_at INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events
             (pane_id, rule_id, agent_type, extracted, detected_at)
             VALUES (1, 'codex.working', 'codex', '{\"session_id\":\"kept\"}', 10)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events
             (pane_id, rule_id, agent_type, extracted, detected_at)
             VALUES (1, ?1, 'codex', '{}', 20)",
            rusqlite::params!["r".repeat(SNAPSHOT_DETECTION_RULE_ID_BYTES + 1)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events
             (pane_id, rule_id, agent_type, extracted, detected_at)
             VALUES (2, 'gemini.working', 'gemini', ?1, 30)",
            rusqlite::params!["x".repeat(SNAPSHOT_DETECTION_EXTRACTED_BYTES + 1)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events
             (pane_id, rule_id, agent_type, extracted, detected_at)
             VALUES (3, 'unknown.working', 'not-a-provider', '{}', 40)",
            [],
        )
        .unwrap();
        drop(conn);

        let detections = load_latest_detections_by_pane_sync(db_path, &[1, 2, 3], 0).unwrap();
        let pane_one = &detections[&1];
        assert_eq!(pane_one.rule_id, "codex.working");
        assert_eq!(pane_one.extracted["session_id"], "kept");
        assert_eq!(detections[&2].extracted, Value::Null);
        assert!(
            !detections.contains_key(&3),
            "unknown provider rows cannot contribute snapshot agent authority"
        );
    }

    #[test]
    fn sqlite_snapshot_mutations_require_one_authoritative_row() {
        assert!(require_exactly_one_changed_row(1).is_ok());
        assert!(matches!(
            require_exactly_one_changed_row(0),
            Err(rusqlite::Error::StatementChangedRows(0))
        ));
        assert!(matches!(
            require_exactly_one_changed_row(2),
            Err(rusqlite::Error::StatementChangedRows(2))
        ));

        let (_tmp, db_path) = setup_test_db();
        let error = mark_shutdown_sync(db_path.as_str(), "missing-session", 1, 1, "missing")
            .expect_err("a missing session cannot be reported as cleanly shut down");
        assert!(matches!(error, rusqlite::Error::StatementChangedRows(0)));
    }

    #[test]
    fn create_session_rejects_authority_triggers_before_insert() {
        let (_tmp, db_path) = setup_test_db();
        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER observe_session_insert
             AFTER INSERT ON MuX_sEsSiOnS
             BEGIN
                 SELECT 1;
             END;",
        )
        .unwrap();
        drop(conn);

        let error = create_session_sync(
            db_path.as_str(),
            "sess-trigger-create",
            1_000,
            r#"{"version":"trigger"}"#,
            crate::VERSION,
        )
        .expect_err("an unaudited session trigger must fail before creation");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));

        let session_count: i64 = Connection::open(db_path.as_str())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(session_count, 0);
    }

    #[test]
    fn mark_shutdown_rejects_authority_triggers_before_update() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-trigger-shutdown",
            1_000,
            r#"{"version":"trigger"}"#,
            crate::VERSION,
        )
        .unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER observe_shutdown_update
             AFTER UPDATE ON MUX_SESSIONS
             BEGIN
                 SELECT 1;
             END;",
        )
        .unwrap();
        drop(conn);

        let error = mark_shutdown_sync(db_path.as_str(), "sess-trigger-shutdown", 1, 1, "missing")
            .expect_err("an unaudited session trigger must fail before shutdown marking");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));

        let shutdown_clean: i64 = Connection::open(db_path.as_str())
            .unwrap()
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions
                 WHERE session_id = 'sess-trigger-shutdown'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(shutdown_clean, 0);
    }

    #[test]
    fn snapshot_lock_errors_preserve_finite_failure_identity() {
        let cases = [
            (LockAcquireError::Cancelled, SnapshotError::Cancelled),
            (
                LockAcquireError::DeadlineExceeded,
                SnapshotError::DeadlineExceeded,
            ),
            (
                LockAcquireError::PollQuotaExhausted,
                SnapshotError::PollQuotaExhausted,
            ),
            (
                LockAcquireError::CostBudgetExhausted,
                SnapshotError::CostBudgetExhausted,
            ),
            (
                LockAcquireError::ContextFailure,
                SnapshotError::ContextFailure,
            ),
            (
                LockAcquireError::TimedOut { deadline_nanos: 73 },
                SnapshotError::LockTimedOut { deadline_nanos: 73 },
            ),
            (LockAcquireError::Poisoned, SnapshotError::LockPoisoned),
            (
                LockAcquireError::PolledAfterCompletion,
                SnapshotError::LockPolledAfterCompletion,
            ),
        ];

        for (lock_error, expected_error) in cases {
            let actual_error = snapshot_lock_error(lock_error);
            assert_eq!(
                std::mem::discriminant(&actual_error),
                std::mem::discriminant(&expected_error)
            );
            if let (
                SnapshotError::LockTimedOut {
                    deadline_nanos: actual,
                },
                SnapshotError::LockTimedOut {
                    deadline_nanos: expected,
                },
            ) = (actual_error, expected_error)
            {
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn snapshot_authority_operation_codes_and_labels_are_stable_and_exhaustive() {
        let cases = [
            (
                SnapshotAuthorityOperation::CheckpointCommit,
                1,
                "checkpoint_commit",
            ),
            (
                SnapshotAuthorityOperation::CheckpointCleanup,
                2,
                "checkpoint_cleanup",
            ),
            (
                SnapshotAuthorityOperation::SessionRetentionCleanup,
                3,
                "session_retention_cleanup",
            ),
            (SnapshotAuthorityOperation::ShutdownMark, 4, "shutdown_mark"),
            (
                SnapshotAuthorityOperation::CheckpointDelete,
                5,
                "checkpoint_delete",
            ),
            (
                SnapshotAuthorityOperation::RestoreReceiptCommit,
                6,
                "restore_receipt_commit",
            ),
            (
                SnapshotAuthorityOperation::RestoreCleanMark,
                7,
                "restore_clean_mark",
            ),
            (
                SnapshotAuthorityOperation::SessionDelete,
                8,
                "session_delete",
            ),
            (
                SnapshotAuthorityOperation::RestoreIntentCommit,
                9,
                "restore_intent_commit",
            ),
        ];

        for (operation, code, label) in cases {
            assert_eq!(operation.code(), code);
            assert_eq!(operation.as_str(), label);
            assert_eq!(operation.to_string(), label);
            assert_eq!(SnapshotAuthorityOperation::from_code(code), Some(operation));
        }
        assert_eq!(SnapshotAuthorityOperation::from_code(0), None);
        assert_eq!(SnapshotAuthorityOperation::from_code(10), None);
        assert_eq!(SnapshotAuthorityOperation::from_code(u8::MAX), None);
    }

    #[test]
    fn authority_blocking_failure_classification_preserves_retry_safety_boundary() {
        let before_handoff = classify_snapshot_authority_blocking_failure(
            SnapshotAuthorityOperation::CheckpointCommit,
            crate::runtime_async::SpawnBlockingWithCxError::CancelledBeforeSpawn { kind: None },
            false,
        );
        assert!(matches!(&before_handoff, SnapshotError::Cancelled));
        assert!(!before_handoff.requires_reconciliation());

        let suppressed_failures = [
            (
                crate::runtime_async::SpawnBlockingWithCxError::CancelledMidFlight { kind: None },
                SnapshotError::Cancelled,
            ),
            (
                crate::runtime_async::SpawnBlockingWithCxError::RuntimeFailure,
                SnapshotError::BlockingRuntimeFailure,
            ),
            (
                crate::runtime_async::SpawnBlockingWithCxError::CancellationWatcherTimerFailure,
                SnapshotError::ContextFailure,
            ),
        ];
        for (failure, expected) in suppressed_failures {
            let error = classify_snapshot_authority_blocking_failure(
                SnapshotAuthorityOperation::ShutdownMark,
                failure,
                false,
            );
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected)
            );
            assert!(!error.requires_reconciliation());
        }

        for failure in [
            crate::runtime_async::SpawnBlockingWithCxError::CancelledMidFlight { kind: None },
            crate::runtime_async::SpawnBlockingWithCxError::RuntimeFailure,
            crate::runtime_async::SpawnBlockingWithCxError::CancellationWatcherTimerFailure,
        ] {
            let error = classify_snapshot_authority_blocking_failure(
                SnapshotAuthorityOperation::ShutdownMark,
                failure,
                true,
            );
            assert!(matches!(
                &error,
                SnapshotError::IndeterminateAuthorityMutation {
                    operation: SnapshotAuthorityOperation::ShutdownMark
                }
            ));
            assert!(error.requires_reconciliation());
        }
    }

    #[test]
    fn pure_preparation_blocking_failures_never_claim_indeterminate_mutation() {
        let cases = [
            (
                crate::runtime_async::SpawnBlockingWithCxError::CancelledBeforeSpawn { kind: None },
                SnapshotError::Cancelled,
            ),
            (
                crate::runtime_async::SpawnBlockingWithCxError::CancelledMidFlight { kind: None },
                SnapshotError::Cancelled,
            ),
            (
                crate::runtime_async::SpawnBlockingWithCxError::RuntimeFailure,
                SnapshotError::BlockingRuntimeFailure,
            ),
            (
                crate::runtime_async::SpawnBlockingWithCxError::CancellationWatcherTimerFailure,
                SnapshotError::ContextFailure,
            ),
        ];

        for (failure, expected) in cases {
            let actual = classify_snapshot_pure_blocking_failure(failure);
            assert_eq!(
                std::mem::discriminant(&actual),
                std::mem::discriminant(&expected)
            );
            assert!(!actual.requires_reconciliation());
        }
    }

    #[test]
    fn abandoned_authority_attempt_latches_reconciliation_monotonically() {
        let authority = Arc::new(SnapshotAuthorityState::new(None));
        authority.in_progress.store(true, Ordering::Release);

        {
            let _abandoned_before_handoff = SnapshotAuthorityAttemptGuard {
                authority: Arc::clone(&authority),
                operation: SnapshotAuthorityOperation::CheckpointCommit,
                handoff_state: Arc::new(AtomicU8::new(AUTHORITY_HANDOFF_PENDING)),
                settled: false,
            };
        }
        assert!(!authority.in_progress.load(Ordering::Acquire));
        assert!(!authority.reconciliation_is_required());

        authority.in_progress.store(true, Ordering::Release);
        let settled_after_handoff = SnapshotAuthorityAttemptGuard {
            authority: Arc::clone(&authority),
            operation: SnapshotAuthorityOperation::CheckpointCleanup,
            handoff_state: Arc::new(AtomicU8::new(AUTHORITY_HANDOFF_STARTED)),
            settled: false,
        };
        settled_after_handoff.settle();
        assert!(!authority.in_progress.load(Ordering::Acquire));
        assert!(!authority.reconciliation_is_required());

        authority.in_progress.store(true, Ordering::Release);
        {
            let _abandoned = SnapshotAuthorityAttemptGuard {
                authority: Arc::clone(&authority),
                operation: SnapshotAuthorityOperation::ShutdownMark,
                handoff_state: Arc::new(AtomicU8::new(AUTHORITY_HANDOFF_STARTED)),
                settled: false,
            };
        }
        assert!(!authority.in_progress.load(Ordering::Acquire));
        assert!(authority.reconciliation_is_required());
        assert_eq!(
            authority.first_latched_operation(),
            Some(SnapshotAuthorityOperation::ShutdownMark)
        );

        authority.in_progress.store(true, Ordering::Release);
        SnapshotAuthorityAttemptGuard {
            authority: Arc::clone(&authority),
            operation: SnapshotAuthorityOperation::CheckpointCommit,
            handoff_state: Arc::new(AtomicU8::new(AUTHORITY_HANDOFF_PENDING)),
            settled: false,
        }
        .settle();
        assert!(!authority.in_progress.load(Ordering::Acquire));
        assert!(
            authority.reconciliation_is_required(),
            "a later settled attempt must not clear historical observation loss"
        );
        assert_eq!(
            authority.first_latched_operation(),
            Some(SnapshotAuthorityOperation::ShutdownMark),
            "the first latched operation is immutable"
        );
    }

    #[test]
    fn authority_handoff_suppresses_queued_work_before_start_without_latching() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let authority = Arc::new(SnapshotAuthorityState::new(None));
        authority.in_progress.store(true, Ordering::Release);
        let guard = SnapshotAuthorityAttemptGuard {
            authority: Arc::clone(&authority),
            operation: SnapshotAuthorityOperation::CheckpointCommit,
            handoff_state: Arc::new(AtomicU8::new(AUTHORITY_HANDOFF_PENDING)),
            settled: false,
        };
        let handoff_state = guard.handoff_state();
        drop(guard);

        let calls = AtomicUsize::new(0);
        let outcome = run_authority_work_if_started(&handoff_state, || {
            calls.fetch_add(1, AtomicOrdering::Relaxed);
            Ok::<(), SnapshotAuthorityDbError>(())
        });
        assert!(matches!(outcome, AuthorityBlockingOutcome::Suppressed));
        assert_eq!(calls.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(
            handoff_state.load(Ordering::Acquire),
            AUTHORITY_HANDOFF_SUPPRESSED
        );
        assert!(!authority.reconciliation_is_required());
        assert!(!authority.in_progress.load(Ordering::Acquire));
    }

    #[test]
    fn authority_handoff_started_then_abandoned_latches_before_releasing_admission() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
        let guard = engine
            .try_begin_snapshot_authority(SnapshotAuthorityOperation::CheckpointCommit)
            .expect("authority admission");
        let handoff_state = guard.handoff_state();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let worker = std::thread::spawn(move || {
            run_authority_work_if_started(&handoff_state, || {
                worker_entered.wait();
                worker_release.wait();
                Ok::<(), SnapshotAuthorityDbError>(())
            })
        });

        entered.wait();
        drop(guard);
        assert!(engine.authority_reconciliation_is_required());
        let blocked = engine
            .try_begin_snapshot_authority(SnapshotAuthorityOperation::ShutdownMark)
            .expect_err("latch must publish before admission is released");
        assert!(matches!(
            blocked,
            SnapshotError::AuthorityReconciliationRequired {
                first_indeterminate_operation: Some(SnapshotAuthorityOperation::CheckpointCommit),
                ..
            }
        ));

        release.wait();
        assert!(matches!(
            worker.join().expect("authority worker"),
            AuthorityBlockingOutcome::Executed(Ok(()))
        ));
    }

    #[test]
    fn authority_handoff_completed_but_unpublished_result_still_latches() {
        let authority = Arc::new(SnapshotAuthorityState::new(None));
        authority.in_progress.store(true, Ordering::Release);
        let guard = SnapshotAuthorityAttemptGuard {
            authority: Arc::clone(&authority),
            operation: SnapshotAuthorityOperation::CheckpointCleanup,
            handoff_state: Arc::new(AtomicU8::new(AUTHORITY_HANDOFF_PENDING)),
            settled: false,
        };
        let handoff_state = guard.handoff_state();
        let outcome =
            run_authority_work_if_started(
                &handoff_state,
                || Ok::<(), SnapshotAuthorityDbError>(()),
            );
        assert!(matches!(
            outcome,
            AuthorityBlockingOutcome::Executed(Ok(()))
        ));
        assert_eq!(
            handoff_state.load(Ordering::Acquire),
            AUTHORITY_HANDOFF_COMPLETED
        );
        drop(guard);
        assert!(authority.reconciliation_is_required());
        assert_eq!(
            authority.first_latched_operation(),
            Some(SnapshotAuthorityOperation::CheckpointCleanup)
        );
    }

    #[test]
    fn synchronous_authority_panic_latches_started_handoff() {
        let (_tmp, db_path) = setup_test_db();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run_checkpoint_authority_sync::<(), SnapshotAuthorityDbError, _>(
                Arc::clone(&db_path),
                SnapshotAuthorityOperation::CheckpointCommit,
                |_| -> std::result::Result<(), SnapshotAuthorityDbError> {
                    panic!("synchronous authority panic fixture")
                },
            );
        }));
        assert!(panic.is_err());

        let replacement = SnapshotEngine::new(db_path, SnapshotConfig::default());
        assert!(replacement.authority_reconciliation_is_required());
        assert_eq!(
            replacement.snapshot_authority.first_latched_operation(),
            Some(SnapshotAuthorityOperation::CheckpointCommit)
        );
    }

    #[cfg(unix)]
    #[test]
    fn established_filesystem_object_replacement_fails_closed() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
        {
            let mut identities = engine
                .snapshot_authority
                .registry_identities
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let object_identity = identities
                .iter_mut()
                .find(|identity| identity.starts_with(SNAPSHOT_AUTHORITY_OBJECT_IDENTITY_PREFIX))
                .expect("existing database object identity");
            *object_identity =
                "sqlite-file-object-unix:18446744073709551615:18446744073709551615".to_string();
        }

        let blocked = engine
            .try_begin_snapshot_authority(SnapshotAuthorityOperation::ShutdownMark)
            .expect_err("replacement identity must latch reconciliation");
        assert!(matches!(
            blocked,
            SnapshotError::AuthorityReconciliationRequired {
                first_indeterminate_operation: Some(SnapshotAuthorityOperation::ShutdownMark),
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn multiple_registry_identity_matches_latch_every_authority_state() {
        let (_tmp, db_path) = setup_test_db();
        let primary = SnapshotEngine::new(Arc::clone(&db_path), SnapshotConfig::default());
        let object_identity = snapshot_authority_file_identities(db_path.as_str())
            .into_iter()
            .find(|identity| identity.starts_with(SNAPSHOT_AUTHORITY_OBJECT_IDENTITY_PREFIX))
            .expect("filesystem object identity");
        let rogue = Arc::new(SnapshotAuthorityState::new_with_registry_identities(
            vec![object_identity.clone()],
            Some(db_path.as_str().to_string()),
        ));
        snapshot_authority_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                object_identity,
                SnapshotAuthorityRegistryEntry::Live(Arc::downgrade(&rogue)),
            );

        let observed = SnapshotEngine::new(db_path, SnapshotConfig::default());
        assert!(Arc::ptr_eq(
            &observed.snapshot_authority,
            &primary.snapshot_authority
        ));
        assert!(primary.authority_reconciliation_is_required());
        assert!(rogue.reconciliation_is_required());
    }

    #[test]
    fn authority_database_error_disposition_controls_latching() {
        run_async_test(async {
            let (_retry_tmp, retry_db) = setup_test_db();
            let retry_engine = SnapshotEngine::new(retry_db, SnapshotConfig::default());
            let cx = crate::cx::for_testing();
            let retry_error = retry_engine
                .spawn_blocking_authority_with_cx(
                    &cx,
                    SnapshotAuthorityOperation::CheckpointCommit,
                    || {
                        Err::<(), _>(SnapshotAuthorityDbError::RetrySafe {
                            source: rusqlite::Error::InvalidQuery,
                        })
                    },
                )
                .await
                .expect_err("retry-safe error");
            assert!(matches!(retry_error, SnapshotError::Database(_)));
            assert!(!retry_engine.authority_reconciliation_is_required());
            retry_engine
                .try_begin_snapshot_authority(SnapshotAuthorityOperation::ShutdownMark)
                .expect("retry-safe failure releases authority")
                .settle();

            for indeterminate in [
                SnapshotAuthorityDbError::IndeterminateCommit {
                    source: rusqlite::Error::InvalidQuery,
                },
                SnapshotAuthorityDbError::IndeterminateRollback {
                    source: rusqlite::Error::InvalidQuery,
                    rollback: Box::new(rusqlite::Error::ExecuteReturnedResults),
                },
            ] {
                let (_tmp, db_path) = setup_test_db();
                let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
                let error = engine
                    .spawn_blocking_authority_with_cx(
                        &cx,
                        SnapshotAuthorityOperation::CheckpointCleanup,
                        move || Err::<(), _>(indeterminate),
                    )
                    .await
                    .expect_err("indeterminate database error");
                assert!(matches!(
                    error,
                    SnapshotError::IndeterminateAuthorityMutation {
                        operation: SnapshotAuthorityOperation::CheckpointCleanup
                    }
                ));
                assert!(engine.authority_reconciliation_is_required());
                assert!(matches!(
                    engine.try_begin_snapshot_authority(SnapshotAuthorityOperation::ShutdownMark),
                    Err(SnapshotError::AuthorityReconciliationRequired { .. })
                ));
            }
        });
    }

    #[test]
    fn snapshot_authority_contention_is_retry_safe_and_releases_on_settlement() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());

        let first = engine
            .try_begin_snapshot_authority(SnapshotAuthorityOperation::CheckpointCommit)
            .expect("first mutation should acquire exclusive authority");
        let contention = engine
            .try_begin_snapshot_authority(SnapshotAuthorityOperation::ShutdownMark)
            .expect_err("a concurrent mutation must not acquire authority");
        assert!(matches!(
            &contention,
            SnapshotError::AuthorityMutationInProgress {
                operation: SnapshotAuthorityOperation::ShutdownMark
            }
        ));
        assert!(!contention.requires_reconciliation());

        first.settle();
        engine
            .try_begin_snapshot_authority(SnapshotAuthorityOperation::ShutdownMark)
            .expect("typed settlement must release authority")
            .settle();
    }

    #[test]
    fn snapshot_authority_registry_unifies_file_aliases_but_not_memory_databases() {
        let (_tmp, db_path) = setup_test_db();
        let canonical_engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let path = Path::new(db_path.as_str());
        let alias = path
            .parent()
            .expect("database parent")
            .join(".")
            .join(path.file_name().expect("database file name"));
        let alias_engine = SnapshotEngine::new(
            Arc::new(alias.to_string_lossy().into_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &canonical_engine.snapshot_authority,
            &alias_engine.snapshot_authority
        ));
        let uri_engine = SnapshotEngine::new(
            Arc::new(format!("file:{}?mode=rwc", db_path.as_str())),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &canonical_engine.snapshot_authority,
            &uri_engine.snapshot_authority
        ));

        let memory_a =
            SnapshotEngine::new(Arc::new(":memory:".to_owned()), SnapshotConfig::default());
        let memory_b =
            SnapshotEngine::new(Arc::new(":memory:".to_owned()), SnapshotConfig::default());
        assert!(!Arc::ptr_eq(
            &memory_a.snapshot_authority,
            &memory_b.snapshot_authority
        ));

        for uri in [
            "file:",
            "file://localhost",
            "file:?mode=memory&cache=shared",
        ] {
            let temporary_a =
                SnapshotEngine::new(Arc::new(uri.to_owned()), SnapshotConfig::default());
            let temporary_b =
                SnapshotEngine::new(Arc::new(uri.to_owned()), SnapshotConfig::default());
            assert!(
                !Arc::ptr_eq(
                    &temporary_a.snapshot_authority,
                    &temporary_b.snapshot_authority
                ),
                "empty URI filename {uri:?} must remain connection-private"
            );
        }

        let shared_memory_a = SnapshotEngine::new(
            Arc::new("file:authority-a?mode=memory&cache=shared".to_owned()),
            SnapshotConfig::default(),
        );
        let shared_memory_alias = SnapshotEngine::new(
            Arc::new("file:authority-a?cache=shared&mode=memory".to_owned()),
            SnapshotConfig::default(),
        );
        let shared_memory_b = SnapshotEngine::new(
            Arc::new("file:authority-b?mode=memory&cache=shared".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &shared_memory_a.snapshot_authority,
            &shared_memory_alias.snapshot_authority
        ));
        assert!(!Arc::ptr_eq(
            &shared_memory_a.snapshot_authority,
            &shared_memory_b.snapshot_authority
        ));
        #[cfg(any(unix, windows))]
        {
            let platform_default_vfs = if cfg!(windows) { "win32" } else { "unix" };
            let shared_memory_explicit_default = SnapshotEngine::new(
                Arc::new(format!(
                    "file:authority-a?mode=memory&cache=shared&vfs={platform_default_vfs}"
                )),
                SnapshotConfig::default(),
            );
            assert!(Arc::ptr_eq(
                &shared_memory_a.snapshot_authority,
                &shared_memory_explicit_default.snapshot_authority
            ));
        }
        let shared_memory_alternate_vfs = SnapshotEngine::new(
            Arc::new("file:authority-a?mode=memory&cache=shared&vfs=unix-dotfile".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(!Arc::ptr_eq(
            &shared_memory_a.snapshot_authority,
            &shared_memory_alternate_vfs.snapshot_authority
        ));

        let memdb_a = SnapshotEngine::new(
            Arc::new("file:/authority-memdb?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        let memdb_alias = SnapshotEngine::new(
            Arc::new("file:/authority-memdb?cache=private&vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        let memdb_b = SnapshotEngine::new(
            Arc::new("file:/authority-memdb-other?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &memdb_a.snapshot_authority,
            &memdb_alias.snapshot_authority
        ));
        let memdb_mode_memory_private = SnapshotEngine::new(
            Arc::new("file:/authority-memdb?mode=memory&cache=private&vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        let memdb_mode_memory_shared = SnapshotEngine::new(
            Arc::new("file:/authority-memdb?mode=memory&cache=shared&vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &memdb_a.snapshot_authority,
            &memdb_mode_memory_private.snapshot_authority
        ));
        assert!(Arc::ptr_eq(
            &memdb_a.snapshot_authority,
            &memdb_mode_memory_shared.snapshot_authority
        ));
        assert!(!Arc::ptr_eq(
            &memdb_a.snapshot_authority,
            &memdb_b.snapshot_authority
        ));
        let private_relative_memdb_a = SnapshotEngine::new(
            Arc::new("file:authority-private-memdb?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        let private_relative_memdb_b = SnapshotEngine::new(
            Arc::new("file:authority-private-memdb?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(!Arc::ptr_eq(
            &private_relative_memdb_a.snapshot_authority,
            &private_relative_memdb_b.snapshot_authority
        ));
        let shared_relative_memdb_a = SnapshotEngine::new(
            Arc::new("file:authority-shared-memdb?vfs=memdb&cache=shared".to_owned()),
            SnapshotConfig::default(),
        );
        let shared_relative_memdb_b = SnapshotEngine::new(
            Arc::new("file:authority-shared-memdb?mode=memory&vfs=memdb&cache=shared".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &shared_relative_memdb_a.snapshot_authority,
            &shared_relative_memdb_b.snapshot_authority
        ));
        let single_slash_memdb_a = SnapshotEngine::new(
            Arc::new("file:/?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        let single_slash_memdb_b = SnapshotEngine::new(
            Arc::new("file:/?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(!Arc::ptr_eq(
            &single_slash_memdb_a.snapshot_authority,
            &single_slash_memdb_b.snapshot_authority
        ));
        let backslash_memdb_a = SnapshotEngine::new(
            Arc::new(r"file:\authority-memdb?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        let backslash_memdb_b = SnapshotEngine::new(
            Arc::new(r"file:\authority-memdb?vfs=memdb".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &backslash_memdb_a.snapshot_authority,
            &backslash_memdb_b.snapshot_authority
        ));
        let private_memory_a = SnapshotEngine::new(
            Arc::new("file:authority-private?mode=memory&cache=private".to_owned()),
            SnapshotConfig::default(),
        );
        let private_memory_b = SnapshotEngine::new(
            Arc::new("file:authority-private?mode=memory&cache=private".to_owned()),
            SnapshotConfig::default(),
        );
        assert!(!Arc::ptr_eq(
            &private_memory_a.snapshot_authority,
            &private_memory_b.snapshot_authority
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_authority_registry_unifies_hard_links_and_propagates_latches() {
        let (_tmp, db_path) = setup_test_db();
        let path = Path::new(db_path.as_str());
        let source_name = path
            .file_name()
            .expect("temporary database must have a file name")
            .to_string_lossy();
        let hard_link = path
            .parent()
            .expect("database parent")
            .join(format!("{source_name}.snapshot-authority-hard-link.db"));
        std::fs::hard_link(path, &hard_link).expect("create database hard-link alias");

        let primary = SnapshotEngine::new(db_path, SnapshotConfig::default());
        let alias = SnapshotEngine::new(
            Arc::new(hard_link.to_string_lossy().into_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &primary.snapshot_authority,
            &alias.snapshot_authority
        ));
        assert_eq!(
            primary.db_path, alias.db_path,
            "hard-link aliases must reuse one SQLite locator and one WAL sidecar"
        );

        primary
            .snapshot_authority
            .latch_reconciliation(SnapshotAuthorityOperation::CheckpointCommit);
        assert!(alias.authority_reconciliation_is_required());
        assert!(matches!(
            alias.try_begin_snapshot_authority(SnapshotAuthorityOperation::ShutdownMark),
            Err(SnapshotError::AuthorityReconciliationRequired {
                first_indeterminate_operation: Some(SnapshotAuthorityOperation::CheckpointCommit),
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_authority_refreshes_inode_after_missing_database_is_created() {
        let directory = tempfile::tempdir().expect("authority tempdir");
        let primary_path = directory.path().join("created-after-engine.db");
        assert!(!primary_path.exists());

        let primary = SnapshotEngine::new(
            Arc::new(primary_path.to_string_lossy().into_owned()),
            SnapshotConfig::default(),
        );
        let connection = Connection::open(&primary_path).expect("create database after engine");
        connection
            .execute_batch("CREATE TABLE creation_probe (id INTEGER PRIMARY KEY);")
            .expect("initialize created database");
        drop(connection);

        refresh_snapshot_authority_file_identities(
            primary.db_path.as_str(),
            &primary.snapshot_authority,
            SnapshotAuthorityOperation::CheckpointCommit,
        )
        .expect("publish created database identity");

        let hard_link = directory.path().join("created-after-engine-hard-link.db");
        std::fs::hard_link(&primary_path, &hard_link).expect("create late hard-link alias");
        let alias = SnapshotEngine::new(
            Arc::new(hard_link.to_string_lossy().into_owned()),
            SnapshotConfig::default(),
        );
        assert!(Arc::ptr_eq(
            &primary.snapshot_authority,
            &alias.snapshot_authority
        ));
        assert_eq!(primary.db_path, alias.db_path);
    }

    #[test]
    fn frozen_filesystem_locator_is_independent_of_later_base_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first_base = temp.path().join("first-base");
        let later_base = temp.path().join("later-base");
        std::fs::create_dir_all(first_base.join("state")).expect("first state directory");
        std::fs::create_dir_all(later_base.join("state")).expect("later state directory");

        let frozen = freeze_snapshot_db_locator_from_base("state/ft.db", &first_base);
        let resolved_again = freeze_snapshot_db_locator_from_base(&frozen, &later_base);
        assert_eq!(resolved_again, frozen);

        let frozen_uri = freeze_snapshot_db_locator_from_base(
            "file:state/ft.db?mode=rwc&cache=private",
            &first_base,
        );
        assert_eq!(
            freeze_snapshot_db_locator_from_base(&frozen_uri, &later_base),
            frozen_uri
        );
        assert!(frozen_uri.ends_with("?mode=rwc&cache=private"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_uri_drive_path_matches_ordinary_drive_path() {
        assert_eq!(
            sqlite_windows_uri_drive_path("/C:/data/frankenterm.db"),
            "C:/data/frankenterm.db"
        );
        assert_eq!(
            sqlite_windows_uri_drive_path("/z:/data/frankenterm.db"),
            "z:/data/frankenterm.db"
        );
        assert_eq!(sqlite_windows_uri_drive_path("/D:"), "D:");
        assert_eq!(
            sqlite_windows_uri_drive_path("/server/share.db"),
            "/server/share.db"
        );
        assert_eq!(
            snapshot_authority_file_identity("file:///C:/data/frankenterm.db"),
            snapshot_authority_file_identity("C:/data/frankenterm.db")
        );
    }

    #[test]
    fn snapshot_authority_uri_identity_matches_sqlite_component_rules() {
        let (_tmp, db_path) = setup_test_db();
        let path = Path::new(db_path.as_str());
        let parent = path.parent().expect("database parent");
        let percent_path = parent.join("authority%.db");
        let percent_path = percent_path.to_string_lossy();

        assert_eq!(
            snapshot_authority_file_identity("file:authority-a?mode=memory&ca%63he=sh%61red"),
            snapshot_authority_file_identity("file:authority-a?mode=memory&cache=shared"),
            "percent-decoded query names and values identify the same shared memory DB"
        );
        assert_ne!(
            snapshot_authority_file_identity("file:authority-a?MODE=memory&CACHE=shared"),
            snapshot_authority_file_identity("file:authority-a?mode=memory&cache=shared"),
            "SQLite URI option names are case-sensitive"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file:{}#ignored", db_path.as_str())),
            snapshot_authority_file_identity(&format!("file:{}", db_path.as_str())),
            "raw URI fragments do not participate in SQLite file identity"
        );
        assert_ne!(
            snapshot_authority_file_identity(db_path.as_str()),
            snapshot_authority_file_identity(&format!(" {} ", db_path.as_str())),
            "ordinary filename whitespace is data and must not be trimmed"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file:{percent_path}")),
            snapshot_authority_file_identity(&format!("file:{}", percent_path.replace('%', "%25"))),
            "literal invalid percent escapes and encoded percent bytes alias like SQLite"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file:{}%00ignored", db_path.as_str())),
            snapshot_authority_file_identity(&format!("file:{}", db_path.as_str())),
            "SQLite's bundled default terminates a URI component at percent-zero"
        );
        assert_ne!(
            snapshot_authority_file_identity(&format!(
                "file:{}%23not-a-fragment",
                db_path.as_str()
            )),
            snapshot_authority_file_identity(&format!("file:{}#fragment", db_path.as_str())),
            "an encoded hash remains filename data while a raw hash starts the fragment"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file://local%68ost{}", db_path.as_str())),
            None,
            "SQLite rejects an encoded spelling of the literal localhost authority"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file:{}?vfs=", db_path.as_str())),
            None,
            "bundled SQLite rejects an explicitly empty VFS name"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file:{}?mode=bogus", db_path.as_str())),
            None,
            "invalid access modes are rejected before SQLite opens a VFS"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file:{}?cache=bogus", db_path.as_str())),
            None,
            "invalid cache modes are rejected before SQLite opens a VFS"
        );

        #[cfg(unix)]
        {
            let reference_identity = snapshot_authority_file_identity(&format!(
                "file:{}/authority%FF.db",
                parent.to_string_lossy()
            ));
            let hex_case_alias = snapshot_authority_file_identity(&format!(
                "file:{}/authority%ff.db",
                parent.to_string_lossy()
            ));
            let neighboring_byte_identity = snapshot_authority_file_identity(&format!(
                "file:{}/authority%FE.db",
                parent.to_string_lossy()
            ));
            assert_eq!(
                reference_identity, hex_case_alias,
                "hex-digit case cannot split authority for the same non-UTF-8 Unix filename"
            );
            assert_ne!(
                reference_identity, neighboring_byte_identity,
                "lossy path display must not couple distinct non-UTF-8 Unix filenames"
            );
        }
    }

    #[test]
    fn latched_file_authority_survives_last_engine_drop_in_same_process() {
        let (_tmp, db_path) = setup_test_db();
        {
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            engine
                .snapshot_authority
                .latch_reconciliation(SnapshotAuthorityOperation::CheckpointCleanup);
        }

        let replacement = SnapshotEngine::new(db_path, SnapshotConfig::default());
        assert!(replacement.authority_reconciliation_is_required());
        assert_eq!(
            replacement.snapshot_authority.first_latched_operation(),
            Some(SnapshotAuthorityOperation::CheckpointCleanup)
        );
        assert!(matches!(
            replacement.try_begin_snapshot_authority(SnapshotAuthorityOperation::CheckpointCommit),
            Err(SnapshotError::AuthorityReconciliationRequired {
                first_indeterminate_operation: Some(SnapshotAuthorityOperation::CheckpointCleanup),
                ..
            })
        ));
    }

    #[test]
    fn capture_lifecycle_waits_for_active_owner_then_runs_one_terminal_capture() {
        run_async_test(async {
            use std::sync::atomic::AtomicBool as StdAtomicBool;

            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(db_path, SnapshotConfig::default()));
            engine
                .capture_lifecycle
                .store(CAPTURE_LIFECYCLE_OPEN_ACTIVE, Ordering::Release);
            let acquired = Arc::new(StdAtomicBool::new(false));
            let release = Arc::new(StdAtomicBool::new(false));
            let task_engine = Arc::clone(&engine);
            let task_acquired = Arc::clone(&acquired);
            let task_release = Arc::clone(&release);
            let waiter = crate::runtime_async::task::spawn(async move {
                let cx = crate::cx::for_testing();
                let _reservation = task_engine
                    .reserve_capture_lifecycle(&cx)
                    .await
                    .expect("shutdown reservation");
                task_acquired.store(true, Ordering::Release);
                while !task_release.load(Ordering::Acquire) {
                    sleep(Duration::from_millis(1)).await;
                }
            });

            sleep(Duration::from_millis(10)).await;
            assert!(!acquired.load(Ordering::Acquire));
            assert_eq!(
                engine.capture_lifecycle.load(Ordering::Acquire),
                CAPTURE_LIFECYCLE_SHUTDOWN_PENDING_OWNED,
                "shutdown intent fences later ordinary captures immediately"
            );

            // Simulate the exact active capture guard releasing its claim.
            engine
                .capture_lifecycle
                .store(CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED, Ordering::Release);
            while !acquired.load(Ordering::Acquire) {
                sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(
                engine.capture_lifecycle.load(Ordering::Acquire),
                CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED
            );
            release.store(true, Ordering::Release);
            waiter.await.expect("reservation waiter");
            assert_eq!(
                engine.capture_lifecycle.load(Ordering::Acquire),
                CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE,
                "an abandoned reservation keeps admission fenced and retryable"
            );
            engine
                .capture_lifecycle
                .compare_exchange(
                    CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE,
                    CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .expect("retry adopts abandoned shutdown intent");

            let reservation = CaptureShutdownReservation {
                lifecycle: &engine.capture_lifecycle,
            };
            let cx = crate::cx::for_testing();
            engine
                .capture_with_options_and_shutdown_admission(
                    &cx,
                    &[make_test_pane(1, 24, 80)],
                    SnapshotTrigger::Shutdown,
                    SnapshotCaptureOptions::default(),
                    Some(&reservation),
                )
                .await
                .expect("reserved final capture");
            assert_eq!(
                engine.capture_lifecycle.load(Ordering::Acquire),
                CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED,
                "final capture guard returns ownership to the reservation"
            );
            reservation
                .complete()
                .expect("terminal lifecycle completion");
            assert_eq!(
                engine.capture_lifecycle.load(Ordering::Acquire),
                CAPTURE_LIFECYCLE_SHUTDOWN_COMPLETE
            );
            let ordinary_error = engine
                .capture(&[make_test_pane(2, 24, 80)], SnapshotTrigger::Manual)
                .await
                .expect_err("ordinary capture must stay closed after completion");
            assert!(matches!(ordinary_error, SnapshotError::ShuttingDown));
        });
    }

    #[test]
    fn snapshot_authority_blocking_future_remains_send() {
        fn assert_send<T: Send>(_: T) {}

        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
        let cx = crate::cx::for_testing();
        let future = engine.spawn_blocking_authority_with_cx(
            &cx,
            SnapshotAuthorityOperation::CheckpointCommit,
            || Ok::<(), SnapshotAuthorityDbError>(()),
        );
        assert_send(future);
    }

    #[test]
    fn scheduler_capture_treats_authority_contention_as_retry_safe_skip() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
            let owner = engine
                .try_begin_snapshot_authority(SnapshotAuthorityOperation::CheckpointCleanup)
                .expect("checkpoint cleanup should acquire authority");
            let cx = crate::cx::for_testing();
            let provider_calls = Arc::new(AtomicU64::new(0));
            let provider = {
                let provider_calls = Arc::clone(&provider_calls);
                move || {
                    provider_calls.fetch_add(1, Ordering::Relaxed);
                    async { Ok(vec![make_test_pane(1, 24, 80)]) }
                }
            };

            let outcome = engine
                .capture_from_provider_with_cx(&cx, &provider, SnapshotTrigger::Periodic)
                .await
                .expect("authority contention is a retry-safe scheduler skip");
            assert_eq!(
                outcome,
                SchedulerCaptureOutcome::Deferred(SchedulerCaptureDeferredReason::Busy)
            );
            assert_eq!(
                provider_calls.load(Ordering::Relaxed),
                0,
                "known authority contention must skip expensive pane discovery and assembly"
            );
            assert!(!engine.authority_reconciliation_is_required());

            drop(owner);
        });
    }

    #[test]
    fn scheduler_defers_capacity_admission_and_can_capture_after_pane_count_drops() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
            let cx = crate::cx::for_testing();
            let over_limit_provider = || async {
                Ok((0..=MAX_TOPOLOGY_PANES)
                    .map(|pane_id| {
                        make_test_pane(
                            u64::try_from(pane_id).expect("test pane id fits u64"),
                            24,
                            80,
                        )
                    })
                    .collect::<Vec<_>>())
            };

            let outcome = engine
                .capture_from_provider_with_cx(&cx, &over_limit_provider, SnapshotTrigger::Periodic)
                .await
                .expect("capacity admission should defer rather than terminate the scheduler");
            assert_eq!(
                outcome,
                SchedulerCaptureOutcome::Deferred(
                    SchedulerCaptureDeferredReason::CapacityAdmission
                )
            );

            let recovered_provider = || async { Ok(vec![make_test_pane(1, 24, 80)]) };
            let recovered = engine
                .capture_from_provider_with_cx(&cx, &recovered_provider, SnapshotTrigger::Periodic)
                .await
                .expect("scheduler should remain usable after capacity recovers");
            assert_eq!(recovered, SchedulerCaptureOutcome::Captured);
        });
    }

    #[test]
    fn periodic_scheduler_survives_retry_safe_startup_database_failure() {
        run_async_test_isolated(|| async {
            let invalid_db_directory = tempfile::tempdir().expect("temporary directory");
            let db_path = Arc::new(invalid_db_directory.path().to_string_lossy().into_owned());
            let engine = Arc::new(SnapshotEngine::new(
                db_path,
                SnapshotConfig {
                    interval_seconds: 30,
                    ..SnapshotConfig::default()
                },
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let scheduler_engine = Arc::clone(&engine);
            let scheduler = crate::runtime_async::task::spawn(async move {
                scheduler_engine
                    .run_periodic(shutdown_rx, counting_pane_provider())
                    .await
            });

            let deadline = Instant::now() + Duration::from_secs(10);
            while engine.telemetry().snapshot().captures_attempted == 0 && Instant::now() < deadline
            {
                sleep(Duration::from_millis(20)).await;
            }
            assert!(
                engine.telemetry().snapshot().captures_attempted > 0,
                "scheduler must attempt its startup checkpoint"
            );

            let _ = shutdown_tx.send(true);
            let scheduler_result = scheduler.await.expect("scheduler task join");
            assert!(
                scheduler_result.is_ok(),
                "a retry-safe database failure must not terminate the scheduler: {scheduler_result:?}"
            );
            assert!(!engine.authority_reconciliation_is_required());
        });
    }

    #[test]
    fn intelligent_scheduler_retries_contended_immediate_trigger_without_losing_it() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(100.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let task_engine = Arc::clone(&engine);
            let scheduler = crate::runtime_async::task::spawn(async move {
                task_engine
                    .run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            await_checkpoint_count(db_path.as_str(), 1, "startup capture").await;
            let owner = engine
                .try_begin_snapshot_authority(SnapshotAuthorityOperation::CheckpointCleanup)
                .expect("test authority owner");
            assert!(engine.emit_trigger(SnapshotTrigger::HazardThreshold));

            sleep(SCHEDULER_URGENT_CAPTURE_RETRY_DELAY + Duration::from_millis(50)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                1,
                "contention must not fabricate a completed immediate capture"
            );

            drop(owner);
            await_checkpoint_count(
                db_path.as_str(),
                2,
                "deferred immediate capture after authority release",
            )
            .await;
            shutdown_tx.send(true).expect("shutdown send");
            scheduler.await.expect("scheduler task");
        });
    }

    #[test]
    fn authority_reconciliation_latch_suppresses_later_mutations() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
            engine
                .snapshot_authority
                .latch_reconciliation(SnapshotAuthorityOperation::CheckpointCommit);

            let cx = crate::cx::for_testing();
            let shutdown_checkpoint_error = engine
                .shutdown_checkpoint_with_cx(
                    &cx,
                    &[make_test_pane(1, 24, 80)],
                    Duration::from_secs(1),
                )
                .await
                .expect_err("a latched engine must suppress the shutdown checkpoint");
            assert!(matches!(
                &shutdown_checkpoint_error,
                SnapshotError::AuthorityReconciliationRequired {
                    operation: SnapshotAuthorityOperation::CheckpointCommit,
                    ..
                }
            ));
            assert!(shutdown_checkpoint_error.requires_reconciliation());

            let dummy_receipt = SnapshotResult {
                session_id: "unpublished".to_string(),
                checkpoint_id: 1,
                checkpoint_at: 1,
                state_hash: "snp2:unpublished".to_string(),
                pane_count: 0,
                total_bytes: 0,
                persisted_text_bytes: 0,
                truncated_pane_count: 0,
                trigger: SnapshotTrigger::Shutdown,
            };
            let shutdown_error = engine
                .close_after_checkpoint_with_cx(&cx, &dummy_receipt)
                .await
                .expect_err("a latched engine with no in-memory ID must not report shutdown");
            assert!(matches!(
                &shutdown_error,
                SnapshotError::AuthorityReconciliationRequired {
                    operation: SnapshotAuthorityOperation::ShutdownMark,
                    ..
                }
            ));
            assert!(shutdown_error.requires_reconciliation());

            let error = engine
                .cleanup_with_cx(&cx)
                .await
                .expect_err("a latched engine must suppress checkpoint cleanup");
            assert!(matches!(
                &error,
                SnapshotError::AuthorityReconciliationRequired {
                    operation: SnapshotAuthorityOperation::CheckpointCleanup,
                    ..
                }
            ));
            assert!(error.requires_reconciliation());
        });
    }

    async fn recv_trigger(rx: &mut mpsc::Receiver<SnapshotTrigger>) -> SnapshotTrigger {
        let cx = crate::cx::for_testing();
        rx.recv(&cx)
            .await
            .expect("snapshot trigger recv should succeed")
    }

    #[test]
    fn checkpoint_cleanup_cadence_is_monotonic_bounded_and_retry_safe() {
        let base = Instant::now();
        let interval = checkpoint_cleanup_interval(120);
        assert_eq!(
            checkpoint_cleanup_interval(0),
            CHECKPOINT_CLEANUP_MIN_INTERVAL
        );
        assert_eq!(interval, Duration::from_secs(120));
        assert_eq!(
            checkpoint_cleanup_interval(u64::MAX),
            CHECKPOINT_CLEANUP_MAX_INTERVAL
        );

        let mut cadence = CheckpointCleanupCadence::new();
        assert!(checkpoint_cleanup_due(&cadence, interval, base));
        assert_eq!(
            checkpoint_cleanup_wait_duration(&cadence, interval, base),
            Duration::ZERO,
            "a database without an authoritative cleanup receipt is due immediately"
        );

        cadence.last_authoritative_success = Some(base);
        let just_before_interval = interval
            .checked_sub(Duration::from_nanos(1))
            .expect("cleanup interval fixture is nonzero");
        let before_due = base
            .checked_add(just_before_interval)
            .expect("cleanup cadence boundary fits in Instant");
        assert!(!checkpoint_cleanup_due(&cadence, interval, before_due));
        assert_eq!(
            checkpoint_cleanup_wait_duration(&cadence, interval, before_due),
            Duration::from_nanos(1)
        );
        let at_due = base
            .checked_add(interval)
            .expect("cleanup cadence boundary fits in Instant");
        assert!(checkpoint_cleanup_due(&cadence, interval, at_due));

        cadence.retry_deferred_at = Some(at_due);
        assert!(!checkpoint_cleanup_due(&cadence, interval, at_due));
        assert_eq!(
            checkpoint_cleanup_wait_duration(&cadence, interval, at_due),
            CHECKPOINT_CLEANUP_RETRY_DELAY
        );
        let at_retry = at_due
            .checked_add(CHECKPOINT_CLEANUP_RETRY_DELAY)
            .expect("cleanup retry boundary fits in Instant");
        assert!(checkpoint_cleanup_due(&cadence, interval, at_retry));

        cadence.retry_deferred_at = None;
        cadence.last_authoritative_success = Some(at_due);
        assert!(
            !checkpoint_cleanup_due(&cadence, interval, base),
            "a synthetic backwards observation must read as zero elapsed"
        );
        assert_eq!(
            checkpoint_cleanup_wait_duration(&cadence, interval, base),
            interval
        );
    }

    #[test]
    fn checkpoint_cleanup_admission_is_shared_and_drop_defers_retry() {
        let (_tmp, db_path) = setup_test_db();
        let first = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let second = SnapshotEngine::new(db_path, SnapshotConfig::default());
        assert!(Arc::ptr_eq(
            &first.snapshot_authority,
            &second.snapshot_authority
        ));

        let claimed_at = Instant::now();
        let abandoned = first
            .try_begin_automatic_checkpoint_cleanup(claimed_at)
            .expect("the shared startup cleanup should be due");
        assert!(
            second
                .try_begin_automatic_checkpoint_cleanup(claimed_at)
                .is_none(),
            "a peer engine must not duplicate an admitted cleanup scan"
        );
        drop(abandoned);

        let retry_deferred_at = {
            let cadence = first
                .snapshot_authority
                .checkpoint_cleanup_cadence
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(!cadence.in_progress, "drop must release shared admission");
            cadence
                .retry_deferred_at
                .expect("an unfinished attempt must publish retry deferral")
        };
        let just_before_retry = CHECKPOINT_CLEANUP_RETRY_DELAY
            .checked_sub(Duration::from_nanos(1))
            .expect("checkpoint cleanup retry fixture is nonzero");
        let before_retry = retry_deferred_at
            .checked_add(just_before_retry)
            .expect("cleanup retry boundary fits in Instant");
        assert!(
            second
                .try_begin_automatic_checkpoint_cleanup(before_retry)
                .is_none(),
            "retry deferral must suppress a hot loop"
        );
        let at_retry = retry_deferred_at
            .checked_add(CHECKPOINT_CLEANUP_RETRY_DELAY)
            .expect("cleanup retry boundary fits in Instant");
        let retry = second
            .try_begin_automatic_checkpoint_cleanup(at_retry)
            .expect("an abandoned cleanup must become admissible at the retry boundary");
        drop(retry);
    }

    #[test]
    fn automatic_checkpoint_cleanup_telemetry_counts_scans_not_cadence_checks() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let first = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let second = SnapshotEngine::new(db_path, SnapshotConfig::default());
            let cx = crate::cx::for_testing();

            first
                .maybe_run_checkpoint_cleanup_with_cx(&cx)
                .await
                .expect("shared startup cleanup");
            assert_eq!(first.telemetry().snapshot().cleanup_runs, 1);

            second
                .maybe_run_checkpoint_cleanup_with_cx(&cx)
                .await
                .expect("cadence skip");
            assert_eq!(
                second.telemetry().snapshot().cleanup_runs,
                0,
                "a shared-cadence skip must not be reported as a full cleanup scan"
            );

            {
                let mut cadence = second
                    .snapshot_authority
                    .checkpoint_cleanup_cadence
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cadence.last_authoritative_success = None;
                cadence.retry_deferred_at = None;
            }
            second
                .maybe_run_checkpoint_cleanup_with_cx(&cx)
                .await
                .expect("forced due cleanup");
            assert_eq!(second.telemetry().snapshot().cleanup_runs, 1);
            assert_eq!(
                first.telemetry().snapshot().cleanup_runs,
                1,
                "each engine's counter must report only its own executed cleanup attempts"
            );
        });
    }

    // ft-0yuxe: the scheduler drives `[snapshots.session_retention]` cleanup on
    // the configured cadence. Pin startup, retry deferral, and authoritative
    // completion semantics independently of wall time or SQLite.
    #[test]
    fn session_cleanup_due_retries_safely_then_honors_success_cadence() {
        let base = Instant::now();
        let mut schedule = SessionCleanupSchedule::default();

        // Startup remains due until an authoritative success is recorded.
        assert!(session_cleanup_due(&schedule, 0, base));
        assert!(session_cleanup_due(&schedule, 24, base));

        // Admission contention and retry-safe errors defer attempts without
        // fabricating a successful startup cleanup, including interval=0.
        schedule.defer_retry(base);
        assert!(schedule.last_authoritative_success.is_none());
        assert!(!session_cleanup_due(&schedule, 0, base));
        assert_eq!(
            session_cleanup_wait_duration(&schedule, 0, base),
            Some(SESSION_CLEANUP_RETRY_DELAY),
            "periodic scheduling must expose the independent retry deadline"
        );
        let just_before_retry = SESSION_CLEANUP_RETRY_DELAY
            .checked_sub(Duration::from_nanos(1))
            .expect("session cleanup retry fixture is nonzero");
        let before_retry = base
            .checked_add(just_before_retry)
            .expect("retry boundary fits in Instant");
        assert!(!session_cleanup_due(&schedule, 24, before_retry));
        assert_eq!(
            session_cleanup_wait_duration(&schedule, 24, before_retry),
            Some(Duration::from_nanos(1))
        );
        let at_retry = base
            .checked_add(SESSION_CLEANUP_RETRY_DELAY)
            .expect("retry boundary fits in Instant");
        assert!(session_cleanup_due(&schedule, 0, at_retry));
        assert!(session_cleanup_due(&schedule, 24, at_retry));
        assert_eq!(
            session_cleanup_wait_duration(&schedule, 24, at_retry),
            Some(Duration::ZERO)
        );

        schedule.record_authoritative_success(base);
        assert!(schedule.retry_deferred_at.is_none());

        // Build `now` far ahead of `base` so the elapsed-time subtractions are
        // safe regardless of system uptime (Instant is monotonic from boot).
        let now = base
            .checked_add(Duration::from_secs(100 * 3600))
            .expect("base + 100h fits in Instant");
        let one_hour_ago = now
            .checked_sub(Duration::from_secs(3600))
            .expect("now - 1h fits in Instant");

        // interval_hours == 0 => only-on-startup: never due again after an
        // authoritative success.
        assert!(
            !session_cleanup_due(&schedule, 0, now),
            "interval=0 must not rerun after startup"
        );
        assert_eq!(session_cleanup_wait_duration(&schedule, 0, now), None);

        // A configured interval reruns once that many hours have elapsed.
        assert!(
            session_cleanup_due(&schedule, 24, now),
            "100h elapsed >= 24h interval must be due"
        );
        let mut recent_success = SessionCleanupSchedule::default();
        recent_success.record_authoritative_success(one_hour_ago);
        assert!(
            !session_cleanup_due(&recent_success, 24, now),
            "1h elapsed < 24h interval must not be due"
        );
        assert_eq!(
            session_cleanup_wait_duration(&recent_success, 24, now),
            Some(Duration::from_secs(23 * 3600))
        );
    }

    #[test]
    fn periodic_reconciliation_latch_waits_for_snapshot_instead_of_hot_looping() {
        let now = Instant::now();
        let schedule = SessionCleanupSchedule::default();
        let snapshot_interval = Duration::from_secs(30);

        assert_eq!(
            periodic_scheduler_wait_duration(
                &schedule,
                0,
                false,
                Some(now),
                None,
                snapshot_interval,
                now,
            ),
            Duration::ZERO,
            "an unlatch startup cleanup is immediately due"
        );
        assert_eq!(
            periodic_scheduler_wait_duration(
                &schedule,
                0,
                true,
                Some(now),
                None,
                snapshot_interval,
                now,
            ),
            snapshot_interval,
            "a reconciliation latch must suppress cleanup's zero deadline"
        );
    }

    #[test]
    fn periodic_capture_deferral_preserves_due_cadence_and_bounds_retry() {
        let now = Instant::now();
        let mut retry_state = SchedulerCaptureRetryState::default();
        let retry_at = retry_state.retry_deadline(
            now,
            SnapshotTrigger::Periodic,
            SchedulerCaptureDeferredReason::Busy,
        );
        let schedule = SessionCleanupSchedule::default();
        let snapshot_interval = Duration::from_secs(30);

        assert_eq!(
            periodic_scheduler_wait_duration(
                &schedule,
                0,
                true,
                None,
                Some(retry_at),
                snapshot_interval,
                now,
            ),
            SCHEDULER_BACKGROUND_CAPTURE_RETRY_DELAY,
            "a deferred due snapshot waits only for its bounded retry deadline"
        );
        assert_eq!(
            periodic_scheduler_wait_duration(
                &schedule,
                0,
                true,
                None,
                Some(retry_at),
                snapshot_interval,
                retry_at,
            ),
            Duration::ZERO,
            "the original cadence remains due at the retry boundary"
        );
    }

    #[test]
    fn retry_safe_capture_backoff_is_bounded_and_resets_after_settlement() {
        let base = Instant::now();
        let mut retry_state = SchedulerCaptureRetryState::default();
        for expected_seconds in [1, 2, 4, 8, 16, 30, 30] {
            let retry_at = retry_state.retry_deadline(
                base,
                SnapshotTrigger::Periodic,
                SchedulerCaptureDeferredReason::RetrySafeFailure,
            );
            assert_eq!(
                retry_at.saturating_duration_since(base),
                Duration::from_secs(expected_seconds)
            );
        }

        let busy_retry = retry_state.retry_deadline(
            base,
            SnapshotTrigger::HazardThreshold,
            SchedulerCaptureDeferredReason::Busy,
        );
        assert_eq!(
            busy_retry.saturating_duration_since(base),
            SCHEDULER_URGENT_CAPTURE_RETRY_DELAY,
            "admission contention retains its prompt priority-aware retry"
        );
        let still_backed_off = retry_state.retry_deadline(
            base,
            SnapshotTrigger::HazardThreshold,
            SchedulerCaptureDeferredReason::RetrySafeFailure,
        );
        assert_eq!(
            still_backed_off.saturating_duration_since(base),
            SCHEDULER_RETRY_SAFE_CAPTURE_MAX_DELAY,
            "contention alone must not fabricate recovery from a transient failure streak"
        );

        retry_state.record_settled();
        let reset_retry = retry_state.retry_deadline(
            base,
            SnapshotTrigger::Periodic,
            SchedulerCaptureDeferredReason::RetrySafeFailure,
        );
        assert_eq!(
            reset_retry.saturating_duration_since(base),
            SCHEDULER_RETRY_SAFE_CAPTURE_MIN_DELAY
        );
    }

    #[test]
    fn due_intelligent_retry_preempts_trigger_polling() {
        let now = Instant::now();
        let retry_at = now
            .checked_sub(Duration::from_millis(1))
            .expect("one millisecond before now is representable");
        assert_eq!(
            due_intelligent_scheduler_retry(
                Some(SnapshotTrigger::HazardThreshold),
                Some(retry_at),
                now,
            ),
            Some(SnapshotTrigger::HazardThreshold),
            "a due retained capture must run before polling even continuously ready ingress",
        );
        assert_eq!(
            due_intelligent_scheduler_retry(
                Some(SnapshotTrigger::HazardThreshold),
                now.checked_add(Duration::from_secs(1)),
                now,
            ),
            None,
        );
    }

    #[test]
    fn scheduler_retry_classification_excludes_indeterminate_and_capability_failures() {
        assert!(SnapshotError::Database("busy".to_string()).is_retry_safe_scheduler_failure());
        assert!(
            !SnapshotError::Serialization("deterministic projection failure".to_string())
                .is_retry_safe_scheduler_failure()
        );
        assert!(SnapshotError::BlockingRuntimeFailure.is_retry_safe_scheduler_failure());
        assert!(
            SnapshotError::LockTimedOut { deadline_nanos: 1 }.is_retry_safe_scheduler_failure()
        );
        assert!(
            !SnapshotError::IndeterminateAuthorityMutation {
                operation: SnapshotAuthorityOperation::CheckpointCommit,
            }
            .is_retry_safe_scheduler_failure()
        );
        assert!(!SnapshotError::Cancelled.is_retry_safe_scheduler_failure());
        assert!(!SnapshotError::ContextFailure.is_retry_safe_scheduler_failure());
    }

    #[test]
    fn preparation_resource_limits_preserve_finite_capacity_classification() {
        let cases = [
            (
                SnapshotPreparationError::MetadataResourceLimit {
                    bytes: 101,
                    limit: 100,
                },
                SnapshotProjectionResource::MetadataBytes,
                101,
                100,
            ),
            (
                SnapshotPreparationError::MetadataShapeResourceLimit {
                    resource: SnapshotProjectionResource::MetadataDepth,
                    observed: 65,
                    limit: 64,
                },
                SnapshotProjectionResource::MetadataDepth,
                65,
                64,
            ),
            (
                SnapshotPreparationError::MetadataShapeResourceLimit {
                    resource: SnapshotProjectionResource::MetadataNodes,
                    observed: 65_537,
                    limit: 65_536,
                },
                SnapshotProjectionResource::MetadataNodes,
                65_537,
                65_536,
            ),
            (
                SnapshotPreparationError::PaneTextResourceLimit {
                    pane_id: 42,
                    bytes: 201,
                    limit: 200,
                },
                SnapshotProjectionResource::PaneTextBytes,
                201,
                200,
            ),
            (
                SnapshotPreparationError::CheckpointTextResourceLimit {
                    bytes: 301,
                    limit: 300,
                },
                SnapshotProjectionResource::CheckpointTextBytes,
                301,
                300,
            ),
        ];

        for (preparation_error, expected_resource, expected_observed, expected_limit) in cases {
            let error = SnapshotError::from(preparation_error);
            assert!(error.is_capacity_admission_failure());
            assert!(!error.is_retry_safe_scheduler_failure());
            assert!(matches!(
                error,
                SnapshotError::ProjectionResourceLimit {
                    resource,
                    observed,
                    limit,
                } if resource == expected_resource
                    && observed == expected_observed
                    && limit == expected_limit
            ));
        }

        let topology_error = SnapshotError::Topology(TopologySnapshotError::ResourceLimit {
            resource: "panes",
            count: 2,
            limit: 1,
        });
        assert!(topology_error.is_capacity_admission_failure());
        assert!(!topology_error.is_retry_safe_scheduler_failure());

        let topology_json_error = SnapshotError::Topology(TopologySnapshotError::Json(
            serde_json::from_str::<Value>("{").expect_err("fixture is invalid JSON"),
        ));
        assert!(
            !topology_json_error.is_capacity_admission_failure(),
            "serializer/parser failures are not live resource-pressure evidence"
        );
    }

    #[test]
    fn capacity_admission_deferral_uses_bounded_backoff() {
        let base = Instant::now();
        let mut retry_state = SchedulerCaptureRetryState::default();
        for expected_seconds in [1, 2, 4, 8, 16, 30, 30] {
            let retry_at = retry_state.retry_deadline(
                base,
                SnapshotTrigger::Periodic,
                SchedulerCaptureDeferredReason::CapacityAdmission,
            );
            assert_eq!(
                retry_at.saturating_duration_since(base),
                Duration::from_secs(expected_seconds)
            );
        }
    }

    #[test]
    fn session_cleanup_reconciliation_latch_is_monotonic() {
        use crate::session_retention::{SessionCleanupError, SessionCleanupIndeterminatePhase};

        let engine =
            SnapshotEngine::new(Arc::new(":memory:".to_owned()), SnapshotConfig::default());
        for retry_safe in [
            SessionCleanupError::CancelledBeforeHandoff,
            SessionCleanupError::DatabaseOpen,
            SessionCleanupError::DatabasePreparation,
        ] {
            assert!(!engine.latch_session_cleanup_reconciliation(retry_safe));
        }

        assert!(engine.latch_session_cleanup_reconciliation(
            SessionCleanupError::IndeterminateCleanup {
                phase: SessionCleanupIndeterminatePhase::BlockingTaskSettlement,
            }
        ));
        let later_retry_safe = SessionCleanupError::DatabaseOpen;
        assert!(!later_retry_safe.requires_reconciliation());
        assert!(engine.latch_session_cleanup_reconciliation(later_retry_safe));
        assert!(
            engine
                .snapshot_authority
                .session_cleanup_reconciliation_required
                .load(Ordering::Acquire),
            "a same-engine scheduler restart must observe the sticky latch"
        );
        assert!(
            engine
                .snapshot_authority
                .reconciliation_required
                .load(Ordering::Acquire),
            "session-cleanup observation loss must suppress every authority mutation"
        );
    }

    #[test]
    fn session_cleanup_reconciliation_latch_survives_same_engine_scheduler_restart() {
        run_async_test(async {
            use crate::session_retention::{SessionCleanupError, SessionCleanupIndeterminatePhase};

            let engine =
                SnapshotEngine::new(Arc::new(":memory:".to_owned()), SnapshotConfig::default());
            assert!(engine.latch_session_cleanup_reconciliation(
                SessionCleanupError::IndeterminateCleanup {
                    phase: SessionCleanupIndeterminatePhase::CleanupExecution,
                }
            ));

            // A restarted scheduler begins with fresh cadence state. The
            // engine-owned latch must still stop cleanup before touching that
            // state or opening the database.
            let cx = crate::cx::for_testing();
            let mut restarted_scheduler_schedule = SessionCleanupSchedule::default();
            engine
                .maybe_run_session_cleanup(&cx, &mut restarted_scheduler_schedule)
                .await;
            assert_eq!(
                restarted_scheduler_schedule,
                SessionCleanupSchedule::default(),
                "the sticky latch must stop cleanup before cadence state changes"
            );
        });
    }

    #[test]
    fn session_cleanup_admission_contention_defers_without_claiming_startup_success() {
        run_async_test(async {
            let mut config = SnapshotConfig::default();
            config.session_retention.cleanup_interval_hours = 0;
            let engine = SnapshotEngine::new(Arc::new(":memory:".to_owned()), config);
            let owner = engine
                .try_begin_session_cleanup()
                .expect("first scheduler owns cleanup admission");
            let mut competing_schedule = SessionCleanupSchedule::default();
            let cx = crate::cx::for_testing();

            engine
                .maybe_run_session_cleanup(&cx, &mut competing_schedule)
                .await;

            assert!(competing_schedule.last_authoritative_success.is_none());
            assert!(
                competing_schedule.retry_deferred_at.is_some(),
                "admission contention must schedule a bounded retry"
            );
            assert!(
                !engine
                    .snapshot_authority
                    .session_cleanup_reconciliation_required
                    .load(Ordering::Acquire),
                "known admission contention has no unknown durable effect"
            );
            drop(owner);
        });
    }

    #[test]
    fn session_cleanup_retry_safe_failure_defers_interval_zero_startup_retry() {
        run_async_test(async {
            let mut config = SnapshotConfig::default();
            config.session_retention.cleanup_interval_hours = 0;
            let engine = SnapshotEngine::new(Arc::new(":memory:".to_owned()), config);
            let mut schedule = SessionCleanupSchedule::default();
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("session cleanup retry-safe scheduling test"),
            );

            engine.maybe_run_session_cleanup(&cx, &mut schedule).await;

            assert!(schedule.last_authoritative_success.is_none());
            assert!(schedule.retry_deferred_at.is_some());
            assert!(
                !session_cleanup_due(&schedule, 0, Instant::now()),
                "retry-safe failure must not hot-loop on the intelligent scheduler"
            );
            assert!(
                !engine
                    .snapshot_authority
                    .session_cleanup_reconciliation_required
                    .load(Ordering::Acquire),
                "cancelled-before-handoff is retry-safe and must not latch reconciliation"
            );
            assert!(
                !engine.session_cleanup_in_progress.load(Ordering::Acquire),
                "typed retry-safe completion must release cleanup admission"
            );
        });
    }

    #[test]
    fn session_cleanup_authoritative_success_advances_interval_zero_cadence() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let mut config = SnapshotConfig::default();
            config.session_retention.cleanup_interval_hours = 0;
            let engine = SnapshotEngine::new(db_path, config);
            let mut schedule = SessionCleanupSchedule::default();
            let cx = crate::cx::for_testing();

            engine.maybe_run_session_cleanup(&cx, &mut schedule).await;

            let first_success = schedule
                .last_authoritative_success
                .expect("successful cleanup must advance normal cadence");
            assert!(schedule.retry_deferred_at.is_none());
            assert!(!session_cleanup_due(&schedule, 0, Instant::now()));

            engine.maybe_run_session_cleanup(&cx, &mut schedule).await;
            assert_eq!(
                schedule.last_authoritative_success,
                Some(first_success),
                "interval=0 must not run again after authoritative startup success"
            );
        });
    }

    #[test]
    fn session_cleanup_pending_reconciliation_defers_interval_zero_success() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let conn = Connection::open(db_path.as_str()).expect("open cleanup fixture");
            for index in 0..5_i64 {
                conn.execute(
                    "INSERT INTO mux_sessions (
                         session_id, created_at, shutdown_clean,
                         topology_json, ft_version
                     ) VALUES (?1, ?2, 0, '{}', '0.1.0')",
                    rusqlite::params![format!("pending-active-{index}"), index + 1],
                )
                .expect("insert pending recovery-authority fixture");
            }
            drop(conn);

            let mut config = SnapshotConfig::default();
            config.session_retention.cleanup_interval_hours = 0;
            let engine = SnapshotEngine::new(db_path, config);
            let mut schedule = SessionCleanupSchedule::default();
            let cx = crate::cx::for_testing();

            engine.maybe_run_session_cleanup(&cx, &mut schedule).await;

            assert!(
                schedule.last_authoritative_success.is_none(),
                "bounded reconciliation is not a completed startup cleanup"
            );
            assert!(
                schedule.retry_deferred_at.is_some(),
                "bounded reconciliation must schedule a prompt continuation"
            );

            schedule.retry_deferred_at = None;
            engine.maybe_run_session_cleanup(&cx, &mut schedule).await;
            assert!(
                schedule.last_authoritative_success.is_some(),
                "the prompt continuation must finish interval-zero startup cleanup"
            );
            assert!(schedule.retry_deferred_at.is_none());
        });
    }

    #[test]
    fn session_cleanup_admission_is_exclusive_and_drop_only_releases_scheduler_flag() {
        let engine =
            SnapshotEngine::new(Arc::new(":memory:".to_owned()), SnapshotConfig::default());

        let attempt = engine
            .try_begin_session_cleanup()
            .expect("first scheduler owns cleanup admission");
        assert!(
            engine.try_begin_session_cleanup().is_none(),
            "a concurrent scheduler must not enter cleanup"
        );
        drop(attempt);

        {
            let _abandoned_attempt = engine
                .try_begin_session_cleanup()
                .expect("settlement releases cleanup admission");
        }
        assert!(
            !engine.authority_reconciliation_is_required(),
            "scheduler-only admission drop has no durable handoff to reconcile"
        );
        engine
            .try_begin_session_cleanup()
            .expect("scheduler-only admission drop releases the flag");
    }

    #[test]
    fn scheduler_admission_is_exclusive_and_released_on_shutdown() {
        run_async_test_isolated(|| async {
            use std::sync::atomic::AtomicUsize;

            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path,
                SnapshotConfig {
                    interval_seconds: 30,
                    ..SnapshotConfig::default()
                },
            ));
            let provider_calls = Arc::new(AtomicUsize::new(0));
            let first_provider_calls = Arc::clone(&provider_calls);
            let (first_shutdown_tx, first_shutdown_rx) = watch::channel(false);
            let first_engine = Arc::clone(&engine);
            let first_cx = crate::cx::for_testing();
            let first_task = crate::runtime_async::task::spawn(async move {
                first_engine
                    .run_periodic_with_cx(&first_cx, first_shutdown_rx, move || {
                        let provider_calls = Arc::clone(&first_provider_calls);
                        async move {
                            provider_calls.fetch_add(1, Ordering::Relaxed);
                            Ok(vec![make_test_pane(1, 24, 80)])
                        }
                    })
                    .await
            });

            crate::runtime_async::timeout(Duration::from_secs(5), async {
                while provider_calls.load(Ordering::Relaxed) == 0 {
                    crate::runtime_async::yield_now().await;
                }
            })
            .await
            .expect("first scheduler must acquire admission and capture startup state");

            let (_second_shutdown_tx, second_shutdown_rx) = watch::channel(false);
            let second = engine
                .run_periodic_with_cx(&crate::cx::for_testing(), second_shutdown_rx, || async {
                    Ok(vec![make_test_pane(2, 24, 80)])
                })
                .await;
            assert!(matches!(second, Err(SnapshotError::SchedulerInProgress)));

            first_shutdown_tx.send(true).unwrap();
            crate::runtime_async::timeout(Duration::from_secs(5), first_task)
                .await
                .expect("first scheduler must observe shutdown")
                .expect("first scheduler task must not panic")
                .expect("first scheduler must stop cleanly");

            let (_restart_shutdown_tx, restart_shutdown_rx) = watch::channel(true);
            engine
                .run_periodic_with_cx(&crate::cx::for_testing(), restart_shutdown_rx, || async {
                    panic!("initially stopped restart must not call provider")
                })
                .await
                .expect("normal scheduler exit must release admission for restart");
            assert!(!engine.scheduler_in_progress.load(Ordering::Acquire));
        });
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("snapshot test runtime should build");
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

    /// Like `run_async_test` but spawns a dedicated thread so the test gets
    /// a pristine TLS state. Prevents interference when 25 000+ tests run
    /// in parallel and stomp each other's `ASUPERSYNC_HANDLE` thread-local.
    fn run_async_test_isolated<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()>,
    {
        let result = std::thread::Builder::new()
            .name("snapshot-test-isolated".into())
            .spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("snapshot test runtime should build");
                let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(f());
                }));
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(runtime);
                }));
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::runtime_async::clear_runtime_handle();
                }));
                if let Err(payload) = test_result {
                    std::panic::resume_unwind(payload);
                }
            })
            .expect("failed to spawn isolated test thread")
            .join();
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn make_test_pane(id: u64, rows: u32, cols: u32) -> PaneInfo {
        PaneInfo {
            pane_id: id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: Some(PaneSize {
                rows,
                cols,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some(format!("pane-{id}")),
            cwd: Some(format!("file:///home/user/project-{id}")),
            tty_name: None,
            cursor_x: Some(5),
            cursor_y: Some(10),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: id == 0,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    fn setup_test_db() -> (tempfile::NamedTempFile, Arc<String>) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = Arc::new(tmp.path().to_str().unwrap().to_string());

        // Create schema tables
        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch(crate::storage::migrations::mux_sessions_schema_sql().unwrap())
            .expect("snapshot fixture must install the canonical mux_sessions schema");
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS session_checkpoints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                checkpoint_at INTEGER NOT NULL,
                checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
                state_hash TEXT NOT NULL,
                pane_count INTEGER NOT NULL,
                total_bytes INTEGER NOT NULL,
                metadata_json TEXT,
                checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
                    CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt')),
                topology_json TEXT,
                restore_intent_checkpoint_id INTEGER
                    REFERENCES session_checkpoints(id) ON DELETE CASCADE,
                CHECK(checkpoint_role = 'restore_receipt'
                      OR restore_intent_checkpoint_id IS NULL)
            );
            CREATE TABLE IF NOT EXISTS restore_attempt_lifecycle (
                intent_checkpoint_id INTEGER PRIMARY KEY
                    REFERENCES session_checkpoints(id)
                    ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED,
                session_id TEXT NOT NULL
                    REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                source_checkpoint_id INTEGER NOT NULL,
                outcome_checkpoint_id INTEGER
                    REFERENCES session_checkpoints(id) ON DELETE SET NULL,
                status TEXT NOT NULL
                    CHECK(status IN ('intent','outcome_complete','resolved','reconciliation_required')),
                created_at INTEGER NOT NULL,
                resolved_at INTEGER,
                CHECK(intent_checkpoint_id <> source_checkpoint_id),
                CHECK(outcome_checkpoint_id IS NULL
                      OR outcome_checkpoint_id <> intent_checkpoint_id),
                CHECK(outcome_checkpoint_id IS NULL
                      OR outcome_checkpoint_id <> source_checkpoint_id),
                CHECK(created_at >= 0),
                CHECK(resolved_at IS NULL OR resolved_at >= created_at),
                CHECK(
                    (status = 'intent'
                        AND outcome_checkpoint_id IS NULL
                        AND resolved_at IS NULL)
                    OR (status = 'outcome_complete'
                        AND outcome_checkpoint_id IS NOT NULL
                        AND resolved_at IS NULL)
                    OR (status = 'reconciliation_required'
                        AND resolved_at IS NULL)
                    OR (status = 'resolved'
                        AND resolved_at IS NOT NULL)
                )
            );
            CREATE TABLE IF NOT EXISTS mux_pane_state (
                id INTEGER PRIMARY KEY,
                checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
                pane_id INTEGER NOT NULL,
                cwd TEXT,
                command TEXT,
                env_json TEXT,
                terminal_state_json TEXT NOT NULL,
                agent_metadata_json TEXT,
                scrollback_checkpoint_seq INTEGER,
                last_output_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_checkpoints_session
                ON session_checkpoints(session_id, checkpoint_at);
            CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_latest
                ON session_checkpoints(
                    session_id,
                    checkpoint_role,
                    checkpoint_at DESC,
                    id DESC
                );
            CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_causal
                ON session_checkpoints(session_id, checkpoint_role, id DESC);
            CREATE INDEX IF NOT EXISTS idx_checkpoints_global_latest
                ON session_checkpoints(checkpoint_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_checkpoints_global_snapshot_latest
                ON session_checkpoints(checkpoint_at DESC, id DESC)
                WHERE checkpoint_role = 'snapshot';
            CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_restore_intent_outcome
                ON session_checkpoints(restore_intent_checkpoint_id)
                WHERE restore_intent_checkpoint_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_mux_sessions_clean_checkpoint
                ON mux_sessions(clean_checkpoint_id);
            CREATE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_session_status
                ON restore_attempt_lifecycle(session_id, status, intent_checkpoint_id);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_outcome
                ON restore_attempt_lifecycle(outcome_checkpoint_id)
                WHERE outcome_checkpoint_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint
                ON mux_pane_state(checkpoint_id);
            CREATE INDEX IF NOT EXISTS idx_pane_state_pane
                ON mux_pane_state(pane_id);
            PRAGMA foreign_keys = ON;
            ",
        )
        .unwrap();
        conn.execute_batch(crate::storage::migrations::session_retained_size_schema_sql().unwrap())
            .expect("snapshot fixture must install the canonical v40 retained-size authority");
        conn.execute_batch(
            crate::storage::migrations::session_recovery_usability_schema_sql().unwrap(),
        )
        .expect("snapshot fixture must install the canonical v44 recovery-usability authority");

        (tmp, db_path)
    }

    fn install_checkpoint_scrollback_artifact_fixture(
        db_path: &str,
        pane_id: u64,
        contents: &[&str],
    ) {
        let mut conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE pane_scrollback_summary (
                 pane_id INTEGER PRIMARY KEY,
                 retained_segment_count INTEGER NOT NULL,
                 first_seq INTEGER,
                 last_seq INTEGER,
                 first_captured_at INTEGER,
                 last_captured_at INTEGER
             );
             CREATE TABLE output_segments (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 pane_id INTEGER NOT NULL,
                 seq INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 captured_at INTEGER NOT NULL,
                 redaction_catalog_version TEXT,
                 UNIQUE(pane_id, seq)
             );
             CREATE TABLE output_gaps (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 pane_id INTEGER NOT NULL,
                 seq_before INTEGER NOT NULL,
                 seq_after INTEGER NOT NULL,
                 reason TEXT NOT NULL,
                 detected_at INTEGER NOT NULL
             );",
        )
        .unwrap();
        let pane_id = i64::try_from(pane_id).unwrap();
        let tx = conn.transaction().unwrap();
        for (seq, content) in contents.iter().enumerate() {
            let seq = i64::try_from(seq).unwrap();
            tx.execute(
                "INSERT INTO output_segments
                 (pane_id, seq, content, captured_at, redaction_catalog_version)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    pane_id,
                    seq,
                    *content,
                    1_000_i64 + seq,
                    crate::redact_backfill::current_catalog_version(),
                ],
            )
            .unwrap();
        }
        let count = i64::try_from(contents.len()).unwrap();
        let last_seq = count.checked_sub(1).filter(|_| count != 0);
        let first_seq = (!contents.is_empty()).then_some(0_i64);
        let first_captured_at = (!contents.is_empty()).then_some(1_000_i64);
        let last_captured_at = last_seq.map(|seq| 1_000_i64 + seq);
        tx.execute(
            "INSERT INTO pane_scrollback_summary
             (pane_id, retained_segment_count, first_seq, last_seq,
              first_captured_at, last_captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                pane_id,
                count,
                first_seq,
                last_seq,
                first_captured_at,
                last_captured_at,
            ],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn build_checkpoint_scrollback_artifact_fixture_bytes(
        db_path: &str,
        checkpoint_id: i64,
        limits: CheckpointScrollbackArtifactLimits,
    ) -> Vec<u8> {
        let payload = build_checkpoint_scrollback_payload(db_path, checkpoint_id, limits).unwrap();
        let payload_sha256 =
            hash_checkpoint_artifact_json(&payload, limits.max_artifact_bytes).unwrap();
        serialize_checkpoint_artifact(
            &CheckpointScrollbackArtifact {
                schema: CHECKPOINT_SCROLLBACK_ARTIFACT_SCHEMA.to_string(),
                publication_state: "complete".to_string(),
                payload_sha256,
                payload,
            },
            limits.max_artifact_bytes,
        )
        .unwrap()
    }

    async fn capture_checkpoint_scrollback_artifact_fixture(
        db_path: Arc<String>,
        contents: &[&str],
    ) -> SnapshotResult {
        install_checkpoint_scrollback_artifact_fixture(db_path.as_str(), 1, contents);
        SnapshotEngine::new(db_path, SnapshotConfig::default())
            .capture_with_options(
                &[make_test_pane(1, 24, 80)],
                SnapshotTrigger::Manual,
                SnapshotCaptureOptions {
                    include_scrollback: true,
                    metadata: None,
                },
            )
            .await
            .unwrap()
    }

    #[test]
    fn automatic_and_shutdown_captures_link_durable_scrollback_prefixes() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            install_checkpoint_scrollback_artifact_fixture(
                db_path.as_str(),
                1,
                &["alpha\n", "beta\n", "gamma\n"],
            );
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let cx = crate::cx::for_testing();
            let provider = || async { Ok(vec![make_test_pane(1, 24, 80)]) };

            let automatic = engine
                .capture_from_provider_with_cx(&cx, &provider, SnapshotTrigger::Startup)
                .await
                .expect("automatic startup capture must settle");
            assert_eq!(automatic, SchedulerCaptureOutcome::Captured);

            let (automatic_checkpoint_id, automatic_checkpoint_at, automatic_state_hash) = {
                let conn = Connection::open(db_path.as_str()).unwrap();
                let (checkpoint_id, checkpoint_at, state_hash, checkpoint_seq, last_output_at): (
                    i64,
                    i64,
                    String,
                    Option<i64>,
                    Option<i64>,
                ) = conn
                    .query_row(
                        "SELECT p.checkpoint_id, c.checkpoint_at, c.state_hash,
                                p.scrollback_checkpoint_seq, p.last_output_at
                         FROM mux_pane_state AS p
                         JOIN session_checkpoints AS c ON c.id = p.checkpoint_id
                         ORDER BY p.checkpoint_id DESC, p.pane_id ASC
                         LIMIT 1",
                        [],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .unwrap();
                assert_eq!(checkpoint_seq, Some(2));
                assert_eq!(last_output_at, Some(1_002));
                (
                    checkpoint_id,
                    u64::try_from(checkpoint_at).unwrap(),
                    state_hash,
                )
            };

            let automatic_directory = checkpoint_artifact_test_directory();
            let automatic_path = automatic_directory.path().join(
                checkpoint_scrollback_artifact_file_name(
                    automatic_checkpoint_at,
                    automatic_checkpoint_id,
                    &automatic_state_hash,
                )
                .unwrap(),
            );
            let automatic_receipt = write_checkpoint_scrollback_artifact(
                db_path.as_str(),
                automatic_checkpoint_id,
                &automatic_path,
                CheckpointScrollbackArtifactLimits::default(),
            )
            .unwrap();
            assert_eq!(automatic_receipt.complete_pane_count, 1);

            let shutdown = engine
                .shutdown_checkpoint_with_cx(
                    &cx,
                    &[make_test_pane(1, 30, 100)],
                    Duration::from_secs(5),
                )
                .await
                .expect("shutdown capture must settle");
            let conn = Connection::open(db_path.as_str()).unwrap();
            let (checkpoint_seq, last_output_at): (Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT scrollback_checkpoint_seq, last_output_at
                     FROM mux_pane_state
                     WHERE checkpoint_id = ?1 AND pane_id = 1",
                    [shutdown.checkpoint_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(checkpoint_seq, Some(2));
            assert_eq!(last_output_at, Some(1_002));
        });
    }

    fn write_private_checkpoint_artifact_test_file(path: &Path, bytes: &[u8]) {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600);
        }
        let mut file = options.open(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    /// macOS exposes `/tmp` and `/var` through symlinks, while the artifact
    /// authority deliberately refuses every symlinked path component. Keep
    /// these tests under the physical checkout directory so they exercise the
    /// same no-follow chain on Linux and macOS instead of failing at the test
    /// harness's ambient temporary-directory alias.
    fn checkpoint_artifact_test_directory() -> tempfile::TempDir {
        let physical_checkout = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        tempfile::Builder::new()
            .prefix(".ft-checkpoint-artifact-test-")
            .tempdir_in(physical_checkout)
            .unwrap()
    }

    #[test]
    fn checkpoint_scrollback_artifact_roundtrip_retry_and_false_capabilities() {
        run_async_test(async {
            let (db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["alpha\n", "beta\n", "gamma\n"],
            )
            .await;
            let limits = CheckpointScrollbackArtifactLimits::default();
            let directory = checkpoint_artifact_test_directory();
            let leaf = checkpoint_scrollback_artifact_file_name(
                snapshot.checkpoint_at,
                snapshot.checkpoint_id,
                &snapshot.state_hash,
            )
            .unwrap();
            let path = directory.path().join(leaf);

            let first = write_checkpoint_scrollback_artifact(
                db_path.as_str(),
                snapshot.checkpoint_id,
                &path,
                limits,
            )
            .unwrap();
            let first_bytes = std::fs::read(&path).unwrap();
            let second = write_checkpoint_scrollback_artifact(
                db_path.as_str(),
                snapshot.checkpoint_id,
                &path,
                limits,
            )
            .unwrap();
            assert_eq!(first, second, "a lost reply retry must converge exactly");
            assert_eq!(std::fs::read(&path).unwrap(), first_bytes);
            assert_eq!(first.created_at_epoch_ms, snapshot.checkpoint_at);
            assert_eq!(first.complete_pane_count, 1);
            assert_eq!(first.segment_count, 3);

            let artifact: CheckpointScrollbackArtifact =
                serde_json::from_slice(&first_bytes).unwrap();
            assert_eq!(artifact.payload.scrollback.len(), 1);
            assert_eq!(artifact.payload.scrollback[0].segment_count, 3);
            assert_eq!(artifact.payload.scrollback[0].segments.len(), 3);
            assert_eq!(
                artifact.payload.capabilities,
                CheckpointScrollbackCapabilities::V1
            );
            assert!(!artifact.payload.capabilities.executable_restore_image);
            assert!(!artifact.payload.capabilities.terminal_parser_state);
            assert!(!artifact.payload.capabilities.pty_descriptor_state);
            assert!(!artifact.payload.capabilities.process_state);
            assert!(!artifact.payload.capabilities.running_process_continuity);
            assert!(!artifact.payload.capabilities.stable_mux_local_pane_ids);
            assert!(!artifact.payload.capabilities.live_mux_mutation);

            drop(db_file);
            let offline = verify_checkpoint_scrollback_artifact(&path, limits).unwrap();
            assert_eq!(offline.artifact_sha256, first.artifact_sha256);
            assert_eq!(offline.checkpoint_state_hash, snapshot.state_hash);
            let inventory =
                inventory_checkpoint_scrollback_artifacts(directory.path(), limits).unwrap();
            assert_eq!(inventory.len(), 1);
            assert_eq!(inventory[0].artifact_sha256, first.artifact_sha256);

            let alias_directory = checkpoint_artifact_test_directory();
            let alias_path = alias_directory
                .path()
                .join(format!("alias{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"));
            write_private_checkpoint_artifact_test_file(&alias_path, &first_bytes);
            assert!(matches!(
                inventory_checkpoint_scrollback_artifacts(alias_directory.path(), limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(ref message))
                    if message.contains("canonical checkpoint leaf")
            ));
        });
    }

    #[test]
    fn checkpoint_scrollback_publish_or_recover_survives_lost_reply_without_source_db() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["alpha\n", "beta\n", "gamma\n"],
            )
            .await;
            let limits = CheckpointScrollbackArtifactLimits::default();
            let directory = checkpoint_artifact_test_directory();

            let published = publish_or_recover_checkpoint_scrollback_artifact(
                db_path.as_str(),
                &snapshot,
                directory.path(),
                limits,
            )
            .unwrap();
            assert_eq!(
                published.resolution,
                CheckpointScrollbackArtifactResolution::Published
            );
            assert!(published.scrollback_complete());
            assert_eq!(published.incomplete_pane_count(), 0);
            assert_eq!(published.receipt.checkpoint_id, snapshot.checkpoint_id);
            assert_eq!(published.receipt.session_id, snapshot.session_id);
            assert_eq!(published.receipt.checkpoint_role, CHECKPOINT_ROLE_SNAPSHOT);
            assert_eq!(
                published.receipt.created_at_epoch_ms,
                snapshot.checkpoint_at
            );
            assert_eq!(published.receipt.checkpoint_state_hash, snapshot.state_hash);
            assert_eq!(published.receipt.pane_count, snapshot.pane_count);
            let published_bytes = std::fs::read(&published.path).unwrap();

            let unavailable_source = directory.path().join("source-no-longer-available.sqlite");
            let expected =
                CheckpointScrollbackArtifactExpectedIdentity::from_snapshot_result(&snapshot);
            let recovered = publish_or_recover_checkpoint_scrollback_artifact_for_identity(
                unavailable_source.to_str().unwrap(),
                &expected,
                directory.path(),
                limits,
            )
            .unwrap();
            assert_eq!(
                recovered.resolution,
                CheckpointScrollbackArtifactResolution::RecoveredExisting
            );
            assert_eq!(recovered.path, published.path);
            assert_eq!(recovered.receipt, published.receipt);
            assert_eq!(std::fs::read(&recovered.path).unwrap(), published_bytes);
            assert!(!unavailable_source.exists());
        });
    }

    #[test]
    fn checkpoint_scrollback_publish_or_recover_at_exact_path_survives_lost_reply() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["alpha\n", "beta\n", "gamma\n"],
            )
            .await;
            let expected =
                CheckpointScrollbackArtifactExpectedIdentity::from_snapshot_result(&snapshot);
            let limits = CheckpointScrollbackArtifactLimits::default();
            let directory = checkpoint_artifact_test_directory();
            let exact_path = directory.path().join("operator-selected-export.json");

            let published = publish_or_recover_checkpoint_scrollback_artifact_at_path_for_identity(
                db_path.as_str(),
                &expected,
                &exact_path,
                limits,
            )
            .unwrap();
            assert_eq!(published.path, exact_path);
            assert_eq!(
                published.resolution,
                CheckpointScrollbackArtifactResolution::Published
            );
            let published_bytes = std::fs::read(&published.path).unwrap();

            let unavailable_source = directory.path().join("must-not-open.sqlite");
            let recovered = publish_or_recover_checkpoint_scrollback_artifact_at_path_for_identity(
                unavailable_source.to_str().unwrap(),
                &expected,
                &exact_path,
                limits,
            )
            .unwrap();
            assert_eq!(recovered.path, exact_path);
            assert_eq!(
                recovered.resolution,
                CheckpointScrollbackArtifactResolution::RecoveredExisting
            );
            assert_eq!(recovered.receipt, published.receipt);
            assert_eq!(std::fs::read(&recovered.path).unwrap(), published_bytes);
            assert!(!unavailable_source.exists());
        });
    }

    #[test]
    fn checkpoint_scrollback_publish_or_recover_at_exact_path_rejects_identity_mismatch() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["zero\n", "one\n", "two\n"],
            )
            .await;
            let expected =
                CheckpointScrollbackArtifactExpectedIdentity::from_snapshot_result(&snapshot);
            let limits = CheckpointScrollbackArtifactLimits::default();
            let directory = checkpoint_artifact_test_directory();
            let exact_path = directory.path().join("operator-selected-export.json");
            publish_or_recover_checkpoint_scrollback_artifact_at_path_for_identity(
                db_path.as_str(),
                &expected,
                &exact_path,
                limits,
            )
            .unwrap();
            let published_bytes = std::fs::read(&exact_path).unwrap();
            let mut mismatch = expected;
            mismatch.session_id.push_str("-different");
            let unavailable_source = directory.path().join("must-not-open.sqlite");

            assert!(matches!(
                publish_or_recover_checkpoint_scrollback_artifact_at_path_for_identity(
                    unavailable_source.to_str().unwrap(),
                    &mismatch,
                    &exact_path,
                    limits,
                ),
                Err(CheckpointScrollbackArtifactError::CheckpointIdentityMismatch)
            ));
            assert_eq!(std::fs::read(&exact_path).unwrap(), published_bytes);
            assert!(!unavailable_source.exists());
        });
    }

    #[test]
    fn checkpoint_scrollback_publish_or_recover_rejects_every_identity_mismatch() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["zero\n", "one\n", "two\n"],
            )
            .await;
            let limits = CheckpointScrollbackArtifactLimits::default();
            let source_directory = checkpoint_artifact_test_directory();
            let source = publish_or_recover_checkpoint_scrollback_artifact(
                db_path.as_str(),
                &snapshot,
                source_directory.path(),
                limits,
            )
            .unwrap();
            let source_bytes = std::fs::read(&source.path).unwrap();

            let expected =
                CheckpointScrollbackArtifactExpectedIdentity::from_snapshot_result(&snapshot);
            let mut checkpoint_id_mismatch = expected.clone();
            checkpoint_id_mismatch.checkpoint_id =
                checkpoint_id_mismatch.checkpoint_id.checked_add(1).unwrap();
            let mut session_id_mismatch = expected.clone();
            session_id_mismatch.session_id.push_str("-different");
            let mut checkpoint_at_mismatch = expected.clone();
            checkpoint_at_mismatch.checkpoint_at =
                checkpoint_at_mismatch.checkpoint_at.checked_add(1).unwrap();
            let mut checkpoint_role_mismatch = expected.clone();
            checkpoint_role_mismatch.checkpoint_role = "different".to_string();
            let mut state_hash_mismatch = expected.clone();
            let final_hash_byte = state_hash_mismatch.checkpoint_state_hash.len() - 1;
            let replacement = if state_hash_mismatch.checkpoint_state_hash.ends_with('0') {
                "1"
            } else {
                "0"
            };
            state_hash_mismatch
                .checkpoint_state_hash
                .replace_range(final_hash_byte.., replacement);
            let mut pane_count_mismatch = expected.clone();
            pane_count_mismatch.pane_count = pane_count_mismatch.pane_count.checked_add(1).unwrap();

            for mismatch in [
                checkpoint_id_mismatch,
                session_id_mismatch,
                checkpoint_at_mismatch,
                checkpoint_role_mismatch,
                state_hash_mismatch,
                pane_count_mismatch,
            ] {
                let directory = checkpoint_artifact_test_directory();
                let leaf = checkpoint_scrollback_artifact_file_name(
                    mismatch.checkpoint_at,
                    mismatch.checkpoint_id,
                    &mismatch.checkpoint_state_hash,
                )
                .unwrap();
                let path = directory.path().join(leaf);
                write_private_checkpoint_artifact_test_file(&path, &source_bytes);
                let unavailable_source = directory.path().join("must-not-open.sqlite");

                assert!(matches!(
                    publish_or_recover_checkpoint_scrollback_artifact_for_identity(
                        unavailable_source.to_str().unwrap(),
                        &mismatch,
                        directory.path(),
                        limits,
                    ),
                    Err(CheckpointScrollbackArtifactError::CheckpointIdentityMismatch)
                ));
                assert_eq!(std::fs::read(path).unwrap(), source_bytes);
                assert!(!unavailable_source.exists());
            }

            let mut source_mismatch = expected;
            source_mismatch.pane_count = source_mismatch.pane_count.checked_add(1).unwrap();
            let unpublished_directory = checkpoint_artifact_test_directory();
            let unpublished_path = unpublished_directory.path().join(
                checkpoint_scrollback_artifact_file_name(
                    source_mismatch.checkpoint_at,
                    source_mismatch.checkpoint_id,
                    &source_mismatch.checkpoint_state_hash,
                )
                .unwrap(),
            );
            assert!(matches!(
                publish_or_recover_checkpoint_scrollback_artifact_for_identity(
                    db_path.as_str(),
                    &source_mismatch,
                    unpublished_directory.path(),
                    limits,
                ),
                Err(CheckpointScrollbackArtifactError::CheckpointIdentityMismatch)
            ));
            assert!(
                !unpublished_path.exists(),
                "source identity must be checked before canonical publication"
            );
        });
    }

    #[test]
    fn checkpoint_scrollback_publish_or_recover_exposes_incomplete_prefix_truth() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["zero\n", "one\n", "two\n"],
            )
            .await;
            Connection::open(db_path.as_str())
                .unwrap()
                .execute(
                    "INSERT INTO output_gaps
                     (pane_id, seq_before, seq_after, reason, detected_at)
                     VALUES (1, 0, 1, 'explicit capture loss', 1001)",
                    [],
                )
                .unwrap();
            let directory = checkpoint_artifact_test_directory();

            let publication = publish_or_recover_checkpoint_scrollback_artifact(
                db_path.as_str(),
                &snapshot,
                directory.path(),
                CheckpointScrollbackArtifactLimits::default(),
            )
            .unwrap();
            assert_eq!(publication.receipt.pane_count, 1);
            assert_eq!(publication.receipt.complete_pane_count, 0);
            assert!(!publication.scrollback_complete());
            assert_eq!(publication.incomplete_pane_count(), 1);
        });
    }

    #[test]
    fn checkpoint_scrollback_memory_phases_and_untrusted_errors_are_content_free() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["alpha\n", "beta\n", "gamma\n"],
            )
            .await;
            let limits = CheckpointScrollbackArtifactLimits::default();
            let bytes = build_checkpoint_scrollback_artifact_fixture_bytes(
                db_path.as_str(),
                snapshot.checkpoint_id,
                limits,
            );
            let artifact: CheckpointScrollbackArtifact = serde_json::from_slice(&bytes).unwrap();

            assert!(
                checkpoint_artifact_has_canonical_encoding(
                    &artifact,
                    &bytes,
                    limits.max_artifact_bytes,
                )
                .unwrap()
            );
            let compact = serde_json::to_vec(&artifact).unwrap();
            assert!(
                !checkpoint_artifact_has_canonical_encoding(
                    &artifact,
                    &compact,
                    limits.max_artifact_bytes,
                )
                .unwrap()
            );
            let mut trailing = bytes.clone();
            trailing.push(b' ');
            assert!(
                !checkpoint_artifact_has_canonical_encoding(
                    &artifact,
                    &trailing,
                    limits.max_artifact_bytes,
                )
                .unwrap()
            );
            assert!(
                !checkpoint_artifact_has_canonical_encoding(
                    &artifact,
                    &bytes[..bytes.len() - 1],
                    limits.max_artifact_bytes,
                )
                .unwrap()
            );

            let admitted_bytes_dropped = std::cell::Cell::new(false);
            let receipt = verify_checkpoint_scrollback_artifact_bytes_with_hook(
                bytes.clone(),
                limits,
                || admitted_bytes_dropped.set(true),
            )
            .unwrap();
            assert!(admitted_bytes_dropped.get());
            assert_eq!(receipt.checkpoint_id, snapshot.checkpoint_id);

            let publication_directory = checkpoint_artifact_test_directory();
            let publication_path = publication_directory.path().join(format!(
                "phase-order{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
            ));
            let publication_error = write_checkpoint_scrollback_artifact_with_hook(
                db_path.as_str(),
                snapshot.checkpoint_id,
                &publication_path,
                limits,
                || {
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&publication_path)
                        .unwrap();
                    file.seek(SeekFrom::Start(0)).unwrap();
                    file.write_all(b"!").unwrap();
                    file.sync_all().unwrap();
                },
            )
            .expect_err("offline reread must run after the producer buffer-drop hook");
            assert!(matches!(
                publication_error,
                CheckpointScrollbackArtifactError::InvalidArtifact(_)
            ));

            let secret = "sk-memory-envelope-secret-unknown-field-value";
            let encode_value = |value: &Value| {
                let mut encoded = serde_json::to_vec_pretty(value).unwrap();
                encoded.push(b'\n');
                encoded
            };
            let mut unknown_field: Value = serde_json::from_slice(&bytes).unwrap();
            unknown_field
                .as_object_mut()
                .unwrap()
                .insert(secret.to_string(), Value::Bool(true));
            let unknown_error = verify_checkpoint_scrollback_artifact_bytes_with_hook(
                encode_value(&unknown_field),
                limits,
                || {},
            )
            .expect_err("unknown artifact keys must be rejected");
            assert!(!unknown_error.to_string().contains(secret));

            let mut invalid_type: Value = serde_json::from_slice(&bytes).unwrap();
            invalid_type["payload"]["schema_version"] = Value::String(secret.to_string());
            let type_error = verify_checkpoint_scrollback_artifact_bytes_with_hook(
                encode_value(&invalid_type),
                limits,
                || {},
            )
            .expect_err("attacker-controlled type values must be rejected");
            assert!(!type_error.to_string().contains(secret));

            let mut invalid_topology = artifact.clone();
            invalid_topology.payload.checkpoint.topology_json = format!("{{\"{secret}\":true}}");
            invalid_topology.payload.checkpoint.topology_sha256 = checkpoint_artifact_sha256(
                invalid_topology.payload.checkpoint.topology_json.as_bytes(),
            );
            invalid_topology.payload_sha256 =
                hash_checkpoint_artifact_json(&invalid_topology.payload, limits.max_artifact_bytes)
                    .unwrap();
            let topology_error = verify_checkpoint_scrollback_artifact_bytes_with_hook(
                serialize_checkpoint_artifact(&invalid_topology, limits.max_artifact_bytes)
                    .unwrap(),
                limits,
                || {},
            )
            .expect_err("invalid topology must be rejected without reflecting its text");
            assert!(!topology_error.to_string().contains(secret));

            let mut zero_checkpoint = artifact;
            zero_checkpoint.payload.checkpoint.checkpoint_id = 0;
            zero_checkpoint.payload_sha256 =
                hash_checkpoint_artifact_json(&zero_checkpoint.payload, limits.max_artifact_bytes)
                    .unwrap();
            assert!(matches!(
                verify_checkpoint_scrollback_artifact_bytes_with_hook(
                    serialize_checkpoint_artifact(&zero_checkpoint, limits.max_artifact_bytes,)
                        .unwrap(),
                    limits,
                    || {},
                ),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
            ));
            assert!(
                checkpoint_scrollback_artifact_file_name(
                    snapshot.checkpoint_at,
                    0,
                    &snapshot.state_hash,
                )
                .is_err()
            );
        });
    }

    #[test]
    fn checkpoint_scrollback_source_rejects_unbound_and_mutated_durable_rows() {
        run_async_test(async {
            let (_unbound_db_file, unbound_db_path) = setup_test_db();
            install_checkpoint_scrollback_artifact_fixture(
                unbound_db_path.as_str(),
                1,
                &["already durable\n"],
            );
            let unbound_snapshot =
                SnapshotEngine::new(unbound_db_path.clone(), SnapshotConfig::default())
                    .capture(&[make_test_pane(1, 24, 80)], SnapshotTrigger::Manual)
                    .await
                    .unwrap();
            assert!(matches!(
                build_checkpoint_scrollback_payload(
                    unbound_db_path.as_str(),
                    unbound_snapshot.checkpoint_id,
                    CheckpointScrollbackArtifactLimits::default(),
                ),
                Err(CheckpointScrollbackArtifactError::Checkpoint(ref message))
                    if message.contains("omitted a scrollback reference")
            ));

            let (_malformed_db_file, malformed_db_path) = setup_test_db();
            let malformed_snapshot = capture_checkpoint_scrollback_artifact_fixture(
                malformed_db_path.clone(),
                &["zero\n", "one\n", "two\n"],
            )
            .await;
            Connection::open(malformed_db_path.as_str())
                .unwrap()
                .execute(
                    "UPDATE output_segments SET seq = 'not-an-integer' WHERE pane_id = 1 AND seq = 2",
                    [],
                )
                .unwrap();
            assert!(matches!(
                build_checkpoint_scrollback_payload(
                    malformed_db_path.as_str(),
                    malformed_snapshot.checkpoint_id,
                    CheckpointScrollbackArtifactLimits::default(),
                ),
                Err(CheckpointScrollbackArtifactError::Checkpoint(ref message))
                    if message.contains("malformed rows")
            ));

            let (_timestamp_db_file, timestamp_db_path) = setup_test_db();
            let timestamp_snapshot = capture_checkpoint_scrollback_artifact_fixture(
                timestamp_db_path.clone(),
                &["zero\n", "one\n", "two\n"],
            )
            .await;
            Connection::open(timestamp_db_path.as_str())
                .unwrap()
                .execute(
                    "UPDATE output_segments SET captured_at = 999 WHERE pane_id = 1 AND seq = 2",
                    [],
                )
                .unwrap();
            assert!(matches!(
                build_checkpoint_scrollback_payload(
                    timestamp_db_path.as_str(),
                    timestamp_snapshot.checkpoint_id,
                    CheckpointScrollbackArtifactLimits::default(),
                ),
                Err(CheckpointScrollbackArtifactError::Checkpoint(ref message))
                    if message.contains("timestamp witness")
            ));

            let (_extra_pane_db_file, extra_pane_db_path) = setup_test_db();
            let extra_pane_snapshot = capture_checkpoint_scrollback_artifact_fixture(
                extra_pane_db_path.clone(),
                &["zero\n", "one\n", "two\n"],
            )
            .await;
            Connection::open(extra_pane_db_path.as_str())
                .unwrap()
                .execute(
                    "INSERT INTO mux_pane_state
                     (checkpoint_id, pane_id, terminal_state_json)
                     VALUES (?1, 'malformed-pane-id', '{}')",
                    [extra_pane_snapshot.checkpoint_id],
                )
                .unwrap();
            assert!(matches!(
                build_checkpoint_scrollback_payload(
                    extra_pane_db_path.as_str(),
                    extra_pane_snapshot.checkpoint_id,
                    CheckpointScrollbackArtifactLimits::default(),
                ),
                Err(CheckpointScrollbackArtifactError::Checkpoint(ref message))
                    if message.contains("contains 2 pane rows")
            ));

            let (_gap_db_file, gap_db_path) = setup_test_db();
            let gap_snapshot = capture_checkpoint_scrollback_artifact_fixture(
                gap_db_path.clone(),
                &["zero\n", "one\n", "two\n"],
            )
            .await;
            let gap_conn = Connection::open(gap_db_path.as_str()).unwrap();
            gap_conn
                .execute(
                    "INSERT INTO output_gaps
                     (pane_id, seq_before, seq_after, reason, detected_at)
                     VALUES (1, 0, 1, 'later', 20)",
                    [],
                )
                .unwrap();
            gap_conn
                .execute(
                    "INSERT INTO output_gaps
                     (pane_id, seq_before, seq_after, reason, detected_at)
                     VALUES (1, 0, 1, 'earlier', 10)",
                    [],
                )
                .unwrap();
            let ordered_payload = build_checkpoint_scrollback_payload(
                gap_db_path.as_str(),
                gap_snapshot.checkpoint_id,
                CheckpointScrollbackArtifactLimits::default(),
            )
            .unwrap();
            let ordered_gaps = &ordered_payload.scrollback[0].capture_gaps;
            assert_eq!(ordered_gaps.len(), 2);
            assert_eq!(ordered_gaps[0].detected_at, 10);
            assert_eq!(ordered_gaps[0].reason, "earlier");
            assert_eq!(ordered_gaps[1].detected_at, 20);
            assert_eq!(ordered_gaps[1].reason, "later");

            gap_conn
                .execute(
                    "INSERT INTO output_gaps
                     (pane_id, seq_before, seq_after, reason, detected_at)
                     VALUES (1, 0, 1, 'earlier', 10)",
                    [],
                )
                .unwrap();
            assert!(matches!(
                build_checkpoint_scrollback_payload(
                    gap_db_path.as_str(),
                    gap_snapshot.checkpoint_id,
                    CheckpointScrollbackArtifactLimits::default(),
                ),
                Err(CheckpointScrollbackArtifactError::Checkpoint(ref message))
                    if message.contains("duplicate canonical identity")
            ));
        });
    }

    #[test]
    fn checkpoint_scrollback_publication_recovers_staging_and_rename_lost_reply() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["first\n", "second\n", "third\n"],
            )
            .await;
            let limits = CheckpointScrollbackArtifactLimits::default();
            let bytes = build_checkpoint_scrollback_artifact_fixture_bytes(
                db_path.as_str(),
                snapshot.checkpoint_id,
                limits,
            );

            let resumable_residues = [Vec::new(), bytes[..bytes.len() / 2].to_vec(), bytes.clone()];
            for residue in resumable_residues {
                let directory = checkpoint_artifact_test_directory();
                let path = directory
                    .path()
                    .join(format!("fixture{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"));
                let staging_name =
                    checkpoint_artifact_staging_name(Path::new(path.file_name().unwrap()));
                let staging_path = directory.path().join(&staging_name);
                write_private_checkpoint_artifact_test_file(&staging_path, &residue);

                assert_eq!(
                    publish_checkpoint_artifact_bytes(&path, &bytes).unwrap(),
                    CheckpointArtifactPublicationOutcome::Published
                );
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                assert!(
                    !staging_path.exists(),
                    "the one deterministic staging inode must become the target"
                );
            }

            let mut conflicting_prefix = bytes[..bytes.len() / 2].to_vec();
            conflicting_prefix[0] ^= 1;
            let mut conflicting_full = bytes.clone();
            conflicting_full[0] ^= 1;
            let mut overlong = bytes.clone();
            overlong.push(b'!');
            for residue in [conflicting_prefix, conflicting_full, overlong] {
                let directory = checkpoint_artifact_test_directory();
                let path = directory
                    .path()
                    .join(format!("conflict{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"));
                let staging_name =
                    checkpoint_artifact_staging_name(Path::new(path.file_name().unwrap()));
                let staging_path = directory.path().join(&staging_name);
                write_private_checkpoint_artifact_test_file(&staging_path, &residue);
                let before = CheckpointArtifactFileSnapshot::capture_std(
                    &std::fs::metadata(&staging_path).unwrap(),
                )
                .unwrap();

                assert!(matches!(
                    publish_checkpoint_artifact_bytes(&path, &bytes),
                    Err(CheckpointScrollbackArtifactError::StagingConflict)
                ));
                let after = CheckpointArtifactFileSnapshot::capture_std(
                    &std::fs::metadata(&staging_path).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    before, after,
                    "conflicting stage metadata must be preserved"
                );
                assert_eq!(
                    std::fs::read(&staging_path).unwrap(),
                    residue,
                    "conflicting stage bytes must be preserved"
                );
                assert!(!path.exists());
            }

            let second_crash_directory = checkpoint_artifact_test_directory();
            let second_crash_path = second_crash_directory.path().join(format!(
                "second-crash{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
            ));
            let second_crash_staging_name =
                checkpoint_artifact_staging_name(Path::new(second_crash_path.file_name().unwrap()));
            let second_crash_staging_path = second_crash_directory
                .path()
                .join(second_crash_staging_name);
            let initial_prefix_len = bytes.len() / 3;
            write_private_checkpoint_artifact_test_file(
                &second_crash_staging_path,
                &bytes[..initial_prefix_len],
            );
            let interrupted_append = publish_checkpoint_artifact_bytes_with_fault(
                &second_crash_path,
                &bytes,
                CheckpointArtifactPublicationFault::AfterPartialStagingAppend,
            )
            .expect_err("a second crash must interrupt after only part of the missing suffix");
            assert!(matches!(
                interrupted_append,
                CheckpointScrollbackArtifactError::Io(ref error)
                    if error.kind() == std::io::ErrorKind::Interrupted
            ));
            let twice_partial = std::fs::read(&second_crash_staging_path).unwrap();
            assert!(twice_partial.len() > initial_prefix_len);
            assert!(twice_partial.len() < bytes.len());
            let twice_partial_len = twice_partial.len();
            assert_eq!(twice_partial.as_slice(), &bytes[..twice_partial_len]);
            assert!(!second_crash_path.exists());
            assert_eq!(
                publish_checkpoint_artifact_bytes(&second_crash_path, &bytes).unwrap(),
                CheckpointArtifactPublicationOutcome::Published
            );
            assert_eq!(std::fs::read(&second_crash_path).unwrap(), bytes);
            assert!(!second_crash_staging_path.exists());

            let lost_reply_directory = checkpoint_artifact_test_directory();
            let lost_reply_path = lost_reply_directory
                .path()
                .join(format!("lost-reply{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"));
            let interrupted = publish_checkpoint_artifact_bytes_with_fault(
                &lost_reply_path,
                &bytes,
                CheckpointArtifactPublicationFault::AfterRenameBeforeDirectorySync,
            )
            .expect_err("the planted crash window must return before parent fsync");
            assert!(matches!(
                interrupted,
                CheckpointScrollbackArtifactError::Io(ref error)
                    if error.kind() == std::io::ErrorKind::Interrupted
            ));
            assert_eq!(std::fs::read(&lost_reply_path).unwrap(), bytes);

            let receipt = write_checkpoint_scrollback_artifact(
                db_path.as_str(),
                snapshot.checkpoint_id,
                &lost_reply_path,
                limits,
            )
            .unwrap();
            assert_eq!(receipt.artifact_sha256, checkpoint_artifact_sha256(&bytes));
            assert_eq!(receipt.checkpoint_id, snapshot.checkpoint_id);
            assert_eq!(
                publish_checkpoint_artifact_bytes(&lost_reply_path, &bytes).unwrap(),
                CheckpointArtifactPublicationOutcome::AlreadyApplied
            );

            let unrelated_stage_name =
                checkpoint_artifact_staging_name(Path::new(lost_reply_path.file_name().unwrap()));
            let unrelated_stage_path = lost_reply_directory.path().join(unrelated_stage_name);
            let unrelated_stage_bytes = b"retained-unrelated-stage";
            write_private_checkpoint_artifact_test_file(
                &unrelated_stage_path,
                unrelated_stage_bytes,
            );
            let unrelated_before = CheckpointArtifactFileSnapshot::capture_std(
                &std::fs::metadata(&unrelated_stage_path).unwrap(),
            )
            .unwrap();
            assert_eq!(
                publish_checkpoint_artifact_bytes(&lost_reply_path, &bytes).unwrap(),
                CheckpointArtifactPublicationOutcome::AlreadyApplied
            );
            let unrelated_after = CheckpointArtifactFileSnapshot::capture_std(
                &std::fs::metadata(&unrelated_stage_path).unwrap(),
            )
            .unwrap();
            assert_eq!(unrelated_before, unrelated_after);
            assert_eq!(
                std::fs::read(unrelated_stage_path).unwrap(),
                unrelated_stage_bytes
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_scrollback_directory_authority_rejects_writable_final_and_ancestor() {
        use std::os::unix::fs::PermissionsExt as _;

        let final_directory = checkpoint_artifact_test_directory();
        std::fs::set_permissions(
            final_directory.path(),
            std::fs::Permissions::from_mode(0o770),
        )
        .unwrap();
        let final_target = final_directory
            .path()
            .join(format!("final-mode{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"));
        let final_error = publish_checkpoint_artifact_bytes(&final_target, b"artifact-bytes")
            .expect_err("a group-writable final artifact directory must fail closed");
        assert!(matches!(
            final_error,
            CheckpointScrollbackArtifactError::Io(ref error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
        assert!(!final_target.exists());
        assert!(matches!(
            inventory_checkpoint_scrollback_artifacts(
                final_directory.path(),
                CheckpointScrollbackArtifactLimits::default(),
            ),
            Err(CheckpointScrollbackArtifactError::Io(ref error))
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));

        let ancestor_root = checkpoint_artifact_test_directory();
        let writable_parent = ancestor_root.path().join("peer-writable-parent");
        std::fs::create_dir(&writable_parent).unwrap();
        std::fs::set_permissions(&writable_parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        let private_catalog = writable_parent.join("private-catalog");
        std::fs::create_dir(&private_catalog).unwrap();
        std::fs::set_permissions(&private_catalog, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ancestor_target = private_catalog.join(format!(
            "renameable-parent{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
        ));
        let ancestor_error = publish_checkpoint_artifact_bytes(&ancestor_target, b"artifact-bytes")
            .expect_err("a writable non-sticky parent can replace a private catalog");
        assert!(matches!(
            ancestor_error,
            CheckpointScrollbackArtifactError::Io(ref error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
        assert!(!ancestor_target.exists());

        let absent_catalog = writable_parent.join("must-not-be-created");
        let absent_target = absent_catalog.join(format!(
            "unsafe-create{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
        ));
        assert!(matches!(
            publish_checkpoint_artifact_bytes(&absent_target, b"artifact-bytes"),
            Err(CheckpointScrollbackArtifactError::Io(ref error))
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
        assert!(
            !absent_catalog.exists(),
            "unsafe ancestry must be rejected before creating a directory"
        );

        let sticky_root = checkpoint_artifact_test_directory();
        let sticky_parent = sticky_root.path().join("sticky-shared-parent");
        std::fs::create_dir(&sticky_parent).unwrap();
        std::fs::set_permissions(&sticky_parent, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let sticky_catalog = sticky_parent.join("private-catalog");
        std::fs::create_dir(&sticky_catalog).unwrap();
        std::fs::set_permissions(&sticky_catalog, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sticky_target = sticky_catalog.join(format!(
            "sticky-parent{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
        ));
        assert_eq!(
            publish_checkpoint_artifact_bytes(&sticky_target, b"artifact-bytes").unwrap(),
            CheckpointArtifactPublicationOutcome::Published
        );
        assert_eq!(std::fs::read(sticky_target).unwrap(), b"artifact-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_scrollback_read_rejects_same_size_mtime_restored_mutation() {
        use std::os::unix::fs::MetadataExt as _;

        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["one\n", "two\n", "three\n"],
            )
            .await;
            let limits = CheckpointScrollbackArtifactLimits::default();
            let bytes = build_checkpoint_scrollback_artifact_fixture_bytes(
                db_path.as_str(),
                snapshot.checkpoint_id,
                limits,
            );
            let directory = checkpoint_artifact_test_directory();
            let leaf = format!("mutation{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}");
            let path = directory.path().join(&leaf);
            publish_checkpoint_artifact_bytes(&path, &bytes).unwrap();
            let parent = open_checkpoint_artifact_directory_nofollow(directory.path()).unwrap();
            let before_metadata = std::fs::metadata(&path).unwrap();
            let before_snapshot =
                CheckpointArtifactFileSnapshot::capture_std(&before_metadata).unwrap();
            let original_modified = before_metadata.modified().unwrap();
            let mut changed_bytes = bytes.clone();
            changed_bytes[0] ^= 1;

            let error = read_checkpoint_artifact_from_parent_bounded_with_hook(
                &parent,
                Path::new(&leaf),
                limits.max_artifact_bytes,
                false,
                || {
                    std::thread::sleep(Duration::from_millis(5));
                    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                    file.seek(SeekFrom::Start(0)).unwrap();
                    file.write_all(&changed_bytes).unwrap();
                    file.sync_all().unwrap();
                    file.set_times(std::fs::FileTimes::new().set_modified(original_modified))
                        .unwrap();
                },
            )
            .expect_err("ctime must expose same-size mutation with restored mtime");
            assert!(matches!(
                error,
                CheckpointScrollbackArtifactError::InvalidArtifact(ref message)
                    if message.contains("changed while it was read")
            ));
            let after_metadata = std::fs::metadata(&path).unwrap();
            let after_snapshot =
                CheckpointArtifactFileSnapshot::capture_std(&after_metadata).unwrap();
            assert_eq!(before_snapshot.byte_len, after_snapshot.byte_len);
            assert_eq!(before_snapshot.modified, after_snapshot.modified);
            assert_ne!(
                (before_metadata.ctime(), before_metadata.ctime_nsec()),
                (after_metadata.ctime(), after_metadata.ctime_nsec())
            );
            assert_ne!(before_snapshot, after_snapshot);
            assert!(matches!(
                publish_checkpoint_artifact_bytes(&path, &bytes),
                Err(CheckpointScrollbackArtifactError::AlreadyExists)
            ));

            let streaming_directory = checkpoint_artifact_test_directory();
            let streaming_leaf = format!("streaming{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}");
            let streaming_path = streaming_directory.path().join(&streaming_leaf);
            publish_checkpoint_artifact_bytes(&streaming_path, &bytes).unwrap();
            let streaming_parent =
                open_checkpoint_artifact_directory_nofollow(streaming_directory.path()).unwrap();
            let streaming_modified = std::fs::metadata(&streaming_path)
                .unwrap()
                .modified()
                .unwrap();
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut streaming_file = streaming_parent
                .open_with(Path::new(&streaming_leaf), &options)
                .unwrap();
            let comparison_error = checkpoint_artifact_open_file_matches_expected_with_hook(
                &streaming_parent,
                Path::new(&streaming_leaf),
                &mut streaming_file,
                &bytes,
                false,
                || {
                    std::thread::sleep(Duration::from_millis(5));
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&streaming_path)
                        .unwrap();
                    file.seek(SeekFrom::Start(0)).unwrap();
                    file.write_all(&changed_bytes).unwrap();
                    file.sync_all().unwrap();
                    file.set_times(std::fs::FileTimes::new().set_modified(streaming_modified))
                        .unwrap();
                },
            )
            .expect_err("streamed comparisons must bind ctime as well as size and mtime");
            assert!(matches!(
                comparison_error,
                CheckpointScrollbackArtifactError::InvalidArtifact(ref message)
                    if message.contains("changed while it was compared")
            ));
        });
    }

    #[test]
    fn checkpoint_scrollback_unsupported_noreplace_is_side_effect_free() {
        let directory = checkpoint_artifact_test_directory();
        let staging = directory.path().join("staging");
        let target = directory.path().join("target");
        write_private_checkpoint_artifact_test_file(&staging, b"staging-bytes");
        write_private_checkpoint_artifact_test_file(&target, b"target-bytes");

        let error = checkpoint_artifact_noreplace_unsupported()
            .expect_err("an unproven no-replace primitive must fail closed");
        assert!(matches!(
            error,
            CheckpointScrollbackArtifactError::Io(ref error)
                if error.kind() == std::io::ErrorKind::Unsupported
        ));
        assert_eq!(std::fs::read(staging).unwrap(), b"staging-bytes");
        assert_eq!(std::fs::read(target).unwrap(), b"target-bytes");
    }

    #[test]
    fn checkpoint_scrollback_verifier_rejects_mutations_and_resource_overruns() {
        run_async_test(async {
            let (_db_file, db_path) = setup_test_db();
            let snapshot = capture_checkpoint_scrollback_artifact_fixture(
                db_path.clone(),
                &["alpha\n", "beta\n", "gamma\n"],
            )
            .await;
            let limits = CheckpointScrollbackArtifactLimits::default();
            let bytes = build_checkpoint_scrollback_artifact_fixture_bytes(
                db_path.as_str(),
                snapshot.checkpoint_id,
                limits,
            );
            let artifact: CheckpointScrollbackArtifact = serde_json::from_slice(&bytes).unwrap();

            let mut content_mutation = artifact.clone();
            content_mutation.payload.scrollback[0].segments[0].content = "omega\n".to_string();
            content_mutation.payload_sha256 =
                hash_checkpoint_artifact_json(&content_mutation.payload, limits.max_artifact_bytes)
                    .unwrap();
            let mutated_bytes =
                serialize_checkpoint_artifact(&content_mutation, limits.max_artifact_bytes)
                    .unwrap();
            let mutated_directory = checkpoint_artifact_test_directory();
            let mutated_path = mutated_directory.path().join(format!(
                "content-mutation{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
            ));
            publish_checkpoint_artifact_bytes(&mutated_path, &mutated_bytes).unwrap();
            assert!(matches!(
                verify_checkpoint_scrollback_artifact(&mutated_path, limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
            ));

            let mut sequence_mutation = artifact.clone();
            sequence_mutation.payload.scrollback[0].segments[1].seq = 0;
            assert!(matches!(
                validate_checkpoint_scrollback_payload(&sequence_mutation.payload, limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
            ));

            let mut gap_mutation = artifact.clone();
            gap_mutation.payload.scrollback[0].sequence_gaps.push(
                CheckpointScrollbackSequenceGap {
                    first_missing_seq: 1,
                    last_missing_seq: 1,
                },
            );
            assert!(matches!(
                validate_checkpoint_scrollback_payload(&gap_mutation.payload, limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
            ));

            let mut duplicate_capture_gap = artifact.clone();
            let duplicate = CheckpointScrollbackCaptureGap {
                seq_before: 0,
                seq_after: 1,
                reason: "same canonical capture gap".to_string(),
                detected_at: 1,
            };
            duplicate_capture_gap.payload.scrollback[0]
                .capture_gaps
                .extend([duplicate.clone(), duplicate]);
            assert!(matches!(
                validate_checkpoint_scrollback_payload(&duplicate_capture_gap.payload, limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(ref message))
                    if message.contains("out of order")
            ));

            let mut completeness_mutation = artifact.clone();
            completeness_mutation.payload.scrollback[0].complete = false;
            assert!(matches!(
                validate_checkpoint_scrollback_payload(&completeness_mutation.payload, limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
            ));

            let mut redaction_mutation = artifact.clone();
            let secret =
                "sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let prefix = &mut redaction_mutation.payload.scrollback[0];
            let prior_content_bytes = prefix.segments[0].content_bytes;
            prefix.segments[0].content = secret.to_string();
            prefix.segments[0].content_bytes = secret.len();
            prefix.segments[0].content_sha256 = checkpoint_artifact_sha256(secret.as_bytes());
            prefix.content_bytes = prefix
                .content_bytes
                .checked_sub(u64::try_from(prior_content_bytes).unwrap())
                .unwrap()
                .checked_add(u64::try_from(secret.len()).unwrap())
                .unwrap();
            prefix.prefix_sha256 = checkpoint_scrollback_prefix_sha256(prefix).unwrap();
            redaction_mutation.payload.summary.content_bytes = prefix.content_bytes;
            redaction_mutation.payload_sha256 = hash_checkpoint_artifact_json(
                &redaction_mutation.payload,
                limits.max_artifact_bytes,
            )
            .unwrap();
            let redaction_bytes =
                serialize_checkpoint_artifact(&redaction_mutation, limits.max_artifact_bytes)
                    .unwrap();
            let redaction_directory = checkpoint_artifact_test_directory();
            let redaction_path = redaction_directory.path().join(format!(
                "redaction-mutation{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
            ));
            publish_checkpoint_artifact_bytes(&redaction_path, &redaction_bytes).unwrap();
            assert!(matches!(
                verify_checkpoint_scrollback_artifact(&redaction_path, limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(ref message))
                    if message.contains("fixed point")
            ));

            let mut capability_mutation = artifact;
            capability_mutation.payload.capabilities.process_state = true;
            capability_mutation.payload_sha256 = hash_checkpoint_artifact_json(
                &capability_mutation.payload,
                limits.max_artifact_bytes,
            )
            .unwrap();
            let capability_bytes =
                serialize_checkpoint_artifact(&capability_mutation, limits.max_artifact_bytes)
                    .unwrap();
            let capability_directory = checkpoint_artifact_test_directory();
            let capability_path = capability_directory.path().join(format!(
                "capability-mutation{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"
            ));
            publish_checkpoint_artifact_bytes(&capability_path, &capability_bytes).unwrap();
            assert!(matches!(
                verify_checkpoint_scrollback_artifact(&capability_path, limits),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
            ));

            let mut source_limits = limits;
            source_limits.max_segments_per_pane = 2;
            source_limits.max_total_segments = 2;
            assert!(matches!(
                build_checkpoint_scrollback_payload(
                    db_path.as_str(),
                    snapshot.checkpoint_id,
                    source_limits,
                ),
                Err(CheckpointScrollbackArtifactError::ResourceLimit(_))
            ));

            let bounded_directory = checkpoint_artifact_test_directory();
            let bounded_path = bounded_directory
                .path()
                .join(format!("bounded{CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX}"));
            publish_checkpoint_artifact_bytes(&bounded_path, &bytes).unwrap();
            let mut verifier_limits = limits;
            verifier_limits.max_artifact_bytes =
                u64::try_from(bytes.len().saturating_sub(1)).unwrap();
            verifier_limits.max_content_bytes = verifier_limits.max_artifact_bytes;
            assert!(matches!(
                verify_checkpoint_scrollback_artifact(&bounded_path, verifier_limits),
                Err(CheckpointScrollbackArtifactError::ResourceLimit(_))
            ));

            let excessive_depth = CHECKPOINT_ARTIFACT_JSON_STRUCTURE_LIMITS.max_depth + 1;
            let nested_json = format!(
                "{}0{}",
                "[".repeat(excessive_depth),
                "]".repeat(excessive_depth)
            );
            assert!(matches!(
                verify_checkpoint_artifact_json_structure(nested_json.as_bytes()),
                Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
            ));
        });
    }

    #[test]
    fn checkpoint_scrollback_inventory_and_retention_are_bounded_and_prefix_ordered() {
        let directory = checkpoint_artifact_test_directory();
        write_private_checkpoint_artifact_test_file(&directory.path().join("junk-a"), b"a");
        write_private_checkpoint_artifact_test_file(&directory.path().join("junk-b"), b"b");
        let mut one_entry_limits = CheckpointScrollbackArtifactLimits::default();
        one_entry_limits.max_inventory_entries = 1;
        assert!(matches!(
            inventory_checkpoint_scrollback_artifacts(directory.path(), one_entry_limits),
            Err(CheckpointScrollbackArtifactError::ResourceLimit(_))
        ));

        let limits = CheckpointScrollbackArtifactLimits::default();
        let shared_prefix = "0123456789abcdef";
        let first_collision_hash =
            format!("{SNAPSHOT_WITNESS_PREFIX}{shared_prefix}{}", "a".repeat(48));
        let second_collision_hash =
            format!("{SNAPSHOT_WITNESS_PREFIX}{shared_prefix}{}", "b".repeat(48));
        assert_ne!(
            checkpoint_scrollback_artifact_file_name(40, 4, &first_collision_hash).unwrap(),
            checkpoint_scrollback_artifact_file_name(40, 4, &second_collision_hash).unwrap(),
            "canonical leaves must bind the full witness rather than a 64-bit prefix"
        );

        let make_entry = |created_at_epoch_ms: u64,
                          checkpoint_id: i64,
                          witness_digit: char,
                          artifact_digit: char,
                          artifact_bytes: u64| {
            let checkpoint_state_hash = format!(
                "{SNAPSHOT_WITNESS_PREFIX}{}",
                witness_digit.to_string().repeat(64)
            );
            let file_name = checkpoint_scrollback_artifact_file_name(
                created_at_epoch_ms,
                checkpoint_id,
                &checkpoint_state_hash,
            )
            .unwrap();
            CheckpointScrollbackInventoryEntry {
                file_name: PathBuf::from(file_name),
                created_at_epoch_ms,
                checkpoint_id,
                checkpoint_state_hash,
                artifact_bytes,
                artifact_sha256: artifact_digit.to_string().repeat(64),
            }
        };
        let entries = vec![
            make_entry(30, 3, 'c', 'c', 10),
            make_entry(20, 2, 'b', 'b', 10),
            make_entry(10, 1, 'a', 'a', 1),
        ];
        let newest = entries[0].file_name.clone();
        let middle = entries[1].file_name.clone();
        let oldest = entries[2].file_name.clone();
        let plan = plan_checkpoint_scrollback_artifact_retention(&entries, 3, 15, limits).unwrap();
        assert_eq!(plan.retain, vec![newest]);
        assert_eq!(plan.retire, vec![middle, oldest]);
        assert_eq!(plan.retained_bytes, 10);

        let mut duplicates = entries.clone();
        duplicates[2].file_name = duplicates[0].file_name.clone();
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&duplicates, 3, 100, limits),
            Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
        ));
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&duplicates, 3, 100, one_entry_limits),
            Err(CheckpointScrollbackArtifactError::ResourceLimit(_))
        ));

        let mut alias = entries.clone();
        alias[1].file_name =
            PathBuf::from(format!("alias{}", CHECKPOINT_SCROLLBACK_ARTIFACT_SUFFIX));
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&alias, 3, 100, limits),
            Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
        ));

        let mut duplicate_artifact = entries.clone();
        duplicate_artifact[2].artifact_sha256 = duplicate_artifact[0].artifact_sha256.clone();
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&duplicate_artifact, 3, 100, limits,),
            Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
        ));

        let mut forked_checkpoint = entries.clone();
        forked_checkpoint[2].checkpoint_id = forked_checkpoint[0].checkpoint_id;
        forked_checkpoint[2].file_name = PathBuf::from(
            checkpoint_scrollback_artifact_file_name(
                forked_checkpoint[2].created_at_epoch_ms,
                forked_checkpoint[2].checkpoint_id,
                &forked_checkpoint[2].checkpoint_state_hash,
            )
            .unwrap(),
        );
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&forked_checkpoint, 3, 100, limits,),
            Err(CheckpointScrollbackArtifactError::InvalidArtifact(_))
        ));

        let mut oversized = entries.clone();
        oversized[0].artifact_bytes = limits.max_artifact_bytes + 1;
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&oversized, 3, u64::MAX, limits),
            Err(CheckpointScrollbackArtifactError::ResourceLimit(_))
        ));
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&entries, 0, 100, limits),
            Err(CheckpointScrollbackArtifactError::InvalidLimits(_))
        ));
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&entries, 3, 0, limits),
            Err(CheckpointScrollbackArtifactError::InvalidLimits(_))
        ));
        assert!(matches!(
            plan_checkpoint_scrollback_artifact_retention(&entries, 3, 9, limits),
            Err(CheckpointScrollbackArtifactError::InvalidLimits(_))
        ));
    }

    fn prepare_test_snapshot(
        topology_json: &str,
        panes: &[PaneStateSnapshot],
    ) -> PreparedSnapshotPersistence {
        let topology = if panes.is_empty() {
            TopologySnapshot::empty(0)
        } else {
            let leaf = |pane: &PaneStateSnapshot| crate::session_topology::PaneNode::Leaf {
                pane_id: pane.pane_id,
                rows: pane.terminal.rows.max(1),
                cols: pane.terminal.cols.max(1),
                cwd: pane.cwd.clone(),
                title: Some(pane.terminal.title.clone()),
                is_active: false,
            };
            let pane_tree = if panes.len() == 1 {
                leaf(&panes[0])
            } else {
                crate::session_topology::PaneNode::VSplit {
                    children: panes.iter().map(|pane| (1.0, leaf(pane))).collect(),
                }
            };
            TopologySnapshot {
                schema_version: crate::session_topology::TOPOLOGY_SCHEMA_VERSION,
                captured_at: 0,
                workspace_id: None,
                windows: vec![crate::session_topology::WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![crate::session_topology::TabSnapshot {
                        tab_id: 0,
                        title: None,
                        pane_tree,
                        active_pane_id: None,
                    }],
                    active_tab_index: None,
                }],
            }
        };
        let mut prepared = prepare_snapshot_persistence(&topology, panes, None).unwrap();
        let topology: Value = serde_json::from_str(topology_json).unwrap();
        prepared.topology_json = canonical_json_string(&topology).unwrap();
        prepared.persisted_text_bytes = persisted_checkpoint_text_bytes(
            Some(&prepared.topology_json),
            prepared.metadata_json.as_deref(),
            &prepared.panes,
        )
        .unwrap();
        prepared.dedup_hash =
            snapshot_dedup_witness(&prepared.topology_json, &prepared.panes).unwrap();
        prepared
    }

    fn insert_checkpoint_fixture(
        db_path: &str,
        session_id: &str,
        checkpoint_at: i64,
        checkpoint_role: &str,
        state_hash: &str,
        topology_json: Option<&str>,
        total_bytes: i64,
    ) -> i64 {
        let checkpoint_type = if checkpoint_role == CHECKPOINT_ROLE_SNAPSHOT {
            "event"
        } else {
            "startup"
        };
        let conn = Connection::open(db_path).unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role, topology_json)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, NULL, ?6, ?7)",
            rusqlite::params![
                session_id,
                checkpoint_at,
                checkpoint_type,
                state_hash,
                total_bytes,
                checkpoint_role,
                topology_json,
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn set_session_checkpoint_summary(
        db_path: &str,
        session_id: &str,
        checkpoint_at: i64,
        topology_json: &str,
        clean_checkpoint_id: Option<i64>,
    ) {
        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 topology_json = ?3,
                 shutdown_clean = CASE WHEN ?4 IS NULL THEN 0 ELSE 1 END,
                 clean_checkpoint_id = ?4
             WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                checkpoint_at,
                topology_json,
                clean_checkpoint_id,
            ],
        )
        .unwrap();
    }

    #[test]
    fn capture_single_pane() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let result = engine.capture(&panes, SnapshotTrigger::Manual).await;
            assert!(result.is_ok());
            let result = result.unwrap();
            assert_eq!(result.pane_count, 1);
            assert!(result.checkpoint_id > 0);
            assert!(result.session_id.starts_with("sess-"));

            let conn = Connection::open(db_path.as_str()).unwrap();
            let (session_count, checkpoint_count): (i64, i64) = conn
                .query_row(
                    "SELECT
                         (SELECT COUNT(*) FROM mux_sessions),
                         (SELECT COUNT(*) FROM session_checkpoints)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((session_count, checkpoint_count), (1, 1));
            let (last_checkpoint_at, checkpoint_at): (i64, i64) = conn
                .query_row(
                    "SELECT s.last_checkpoint_at, c.checkpoint_at
                     FROM mux_sessions s
                     JOIN session_checkpoints c ON c.session_id = s.session_id
                     WHERE s.session_id = ?1 AND c.id = ?2",
                    rusqlite::params![&result.session_id, result.checkpoint_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(last_checkpoint_at, checkpoint_at);
        });
    }

    #[test]
    fn failed_first_capture_leaves_no_session_or_in_memory_authority() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(u64::MAX, 24, 80)];

            let error = engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .expect_err("an unrepresentable pane ID must abort the first capture");
            assert!(matches!(&error, SnapshotError::Database(_)));
            assert!(!error.requires_reconciliation());
            assert!(
                !engine
                    .snapshot_authority
                    .reconciliation_required
                    .load(Ordering::Acquire),
                "an observed validation failure remains retry-safe"
            );

            let cx = crate::cx::for_testing();
            assert!(
                engine
                    .session_id
                    .read_with_cx(&cx)
                    .await
                    .expect("session publication lock")
                    .is_none(),
                "a failed first capture must not publish an in-memory session ID"
            );
            assert!(
                engine
                    .last_dedup_hash
                    .read_with_cx(&cx)
                    .await
                    .expect("state-hash publication lock")
                    .is_none(),
                "a failed first transaction must not publish the dedup hash"
            );

            let conn = Connection::open(db_path.as_str()).unwrap();
            let session_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
                .unwrap();
            let checkpoint_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(session_count, 0);
            assert_eq!(checkpoint_count, 0);
            assert_eq!(engine.telemetry().snapshot().capture_errors, 1);
        });
    }

    #[test]
    fn failed_first_checkpoint_insert_rolls_back_new_session_transaction() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let conn = Connection::open(db_path.as_str()).unwrap();
            conn.execute_batch(
                "DROP TRIGGER session_checkpoints_retained_size_ai;
                 CREATE TRIGGER session_checkpoints_retained_size_ai
                 BEFORE INSERT ON session_checkpoints
                 BEGIN
                     SELECT RAISE(ABORT, 'synthetic checkpoint failure');
                 END;",
            )
            .unwrap();
            drop(conn);

            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let error = engine
                .capture(&[make_test_pane(17, 24, 80)], SnapshotTrigger::Manual)
                .await
                .expect_err("checkpoint failure must roll back first-session creation");
            assert!(matches!(&error, SnapshotError::Database(_)));
            assert!(!error.requires_reconciliation());
            assert!(
                !engine
                    .snapshot_authority
                    .reconciliation_required
                    .load(Ordering::Acquire),
                "an observed transactional rollback remains retry-safe"
            );

            let cx = crate::cx::for_testing();
            assert!(
                engine
                    .session_id
                    .read_with_cx(&cx)
                    .await
                    .expect("session publication lock")
                    .is_none()
            );
            let conn = Connection::open(db_path.as_str()).unwrap();
            let (session_count, checkpoint_count): (i64, i64) = conn
                .query_row(
                    "SELECT
                         (SELECT COUNT(*) FROM mux_sessions),
                         (SELECT COUNT(*) FROM session_checkpoints)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((session_count, checkpoint_count), (0, 0));
            assert_eq!(engine.telemetry().snapshot().capture_errors, 1);
        });
    }

    #[test]
    fn capture_multiple_panes() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![
                make_test_pane(1, 24, 80),
                make_test_pane(2, 24, 80),
                make_test_pane(3, 30, 120),
            ];

            let result = engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .unwrap();
            assert_eq!(result.pane_count, 3);

            // Verify pane states were written
            let conn = Connection::open(db_path.as_str()).unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
                    [result.checkpoint_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 3);
        });
    }

    #[test]
    fn agent_metadata_persisted_when_detected_from_title() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let mut pane = make_test_pane(1, 24, 80);
            pane.title = Some("claude-code".to_string());

            let result = engine
                .capture(&[pane], SnapshotTrigger::Manual)
                .await
                .unwrap();

            let conn = Connection::open(db_path.as_str()).unwrap();
            let meta_json: Option<String> = conn
                .query_row(
                    "SELECT agent_metadata_json FROM mux_pane_state WHERE checkpoint_id = ?1 AND pane_id = ?2",
                    rusqlite::params![result.checkpoint_id, 1i64],
                    |row| row.get(0),
                )
                .unwrap();

            let meta_json = meta_json.expect("agent_metadata_json should be present");
            let meta: crate::session_pane_state::AgentMetadata =
                serde_json::from_str(&meta_json).unwrap();
            assert_eq!(meta.agent_type, "claude_code");
            assert_eq!(meta.state.as_deref(), Some("active"));
        });
    }

    #[test]
    fn dedup_skips_unchanged_periodic() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            // First capture succeeds
            let r1 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
            assert!(r1.is_ok());

            // Second periodic capture with same data should be skipped
            let r2 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
            assert!(matches!(r2, Err(SnapshotError::NoChanges)));
        });
    }

    #[test]
    fn periodic_metadata_is_never_discarded_by_state_dedup() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];
            engine
                .capture(&panes, SnapshotTrigger::Periodic)
                .await
                .unwrap();

            let metadata = serde_json::json!({"reason": "operator-request"});
            let result = engine
                .capture_with_options(
                    &panes,
                    SnapshotTrigger::Periodic,
                    SnapshotCaptureOptions {
                        include_scrollback: false,
                        metadata: Some(metadata.clone()),
                    },
                )
                .await
                .expect("metadata-bearing periodic capture must persist");
            let persisted_metadata: String = Connection::open(db_path.as_str())
                .unwrap()
                .query_row(
                    "SELECT metadata_json FROM session_checkpoints WHERE id = ?1",
                    [result.checkpoint_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                persisted_metadata,
                canonical_json_string(&metadata).unwrap()
            );
        });
    }

    #[test]
    fn checkpoint_heartbeat_and_shutdown_remain_bound_to_owner_incarnation() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let owner = engine
                .owner_identity
                .clone()
                .expect("supported test worker must expose host/process ownership authority");
            let panes = vec![make_test_pane(1, 24, 80)];
            let first = engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .unwrap();
            let second = engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .unwrap();

            let conn = Connection::open(db_path.as_str()).unwrap();
            let stored: (String, i64, i64, i64, Option<i64>) = conn
                .query_row(
                    "SELECT host_id, owner_pid, owner_process_start,
                            owner_heartbeat_at, recovery_acknowledged_at
                     FROM mux_sessions WHERE session_id = ?1",
                    [&second.session_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(stored.0, owner.host_id);
            assert_eq!(stored.1, owner.pid);
            assert_eq!(stored.2, owner.process_start);
            assert_eq!(stored.3, i64::try_from(second.checkpoint_at).unwrap());
            assert_eq!(stored.4, None);

            engine.close_after_checkpoint(&second).await.unwrap();
            let shutdown_clean: i64 = conn
                .query_row(
                    "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                    [&first.session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(shutdown_clean, 1);
        });
    }

    #[test]
    fn checkpoint_creation_and_heartbeat_without_owner_authority_leave_owner_tuple_null() {
        let (_tmp, db_path) = setup_test_db();
        let pane = make_test_pane(1, 24, 80);
        let (topology, _) = TopologySnapshot::from_panes(std::slice::from_ref(&pane), 100);
        let pane_state = PaneStateSnapshot::from_pane_info(&pane, 100, false);
        let prepared = prepare_snapshot_persistence(&topology, &[pane_state], None).unwrap();
        let new_session = NewSessionMetadata {
            ft_version: crate::VERSION.to_string(),
            host_id: None,
        };

        save_checkpoint_authoritatively_sync(
            db_path.as_str(),
            "owner-unavailable",
            100,
            SnapshotTrigger::Manual.as_db_str(),
            &prepared,
            Some(&new_session),
            None,
        )
        .unwrap();
        save_checkpoint_authoritatively_sync(
            db_path.as_str(),
            "owner-unavailable",
            101,
            SnapshotTrigger::Manual.as_db_str(),
            &prepared,
            None,
            None,
        )
        .expect("a subsequent checkpoint must not fabricate a heartbeat without owner authority");

        let owner_tuple: (Option<String>, Option<i64>, Option<i64>, Option<i64>, i64) =
            Connection::open(db_path.as_str())
                .unwrap()
                .query_row(
                    "SELECT host_id, owner_pid, owner_process_start, owner_heartbeat_at,
                            (SELECT COUNT(*) FROM session_checkpoints
                             WHERE session_id = mux_sessions.session_id)
                     FROM mux_sessions WHERE session_id = 'owner-unavailable'",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .unwrap();
        assert_eq!(owner_tuple, (None, None, None, None, 2));
    }

    #[test]
    fn checkpoint_after_reconciled_clean_session_accepts_exact_v44_trigger_writes() {
        let (_tmp, db_path) = setup_test_db();
        let pane = make_test_pane(1, 24, 80);
        let (topology, _) = TopologySnapshot::from_panes(std::slice::from_ref(&pane), 100);
        let pane_state = PaneStateSnapshot::from_pane_info(&pane, 100, false);
        let prepared = prepare_snapshot_persistence(&topology, &[pane_state], None).unwrap();
        let new_session = NewSessionMetadata {
            ft_version: crate::VERSION.to_string(),
            host_id: None,
        };

        let first = save_checkpoint_authoritatively_sync(
            db_path.as_str(),
            "reconciled-clean-session",
            100,
            SnapshotTrigger::Manual.as_db_str(),
            &prepared,
            Some(&new_session),
            None,
        )
        .unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute(
            "UPDATE session_recovery_usability
             SET state = 'usable', validated_checkpoint_id = ?1
             WHERE session_id = 'reconciled-clean-session'",
            [first.checkpoint_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 clean_checkpoint_id = ?1,
                 recovery_acknowledged_at = 100
             WHERE session_id = 'reconciled-clean-session'",
            [first.checkpoint_id],
        )
        .unwrap();
        drop(conn);

        save_checkpoint_authoritatively_sync(
            db_path.as_str(),
            "reconciled-clean-session",
            101,
            SnapshotTrigger::Manual.as_db_str(),
            &prepared,
            None,
            None,
        )
        .expect("canonical checkpoint and reopen triggers must satisfy the exact DML witness");

        let recovered: (String, Option<i64>, i64, i64) = Connection::open(db_path.as_str())
            .unwrap()
            .query_row(
                "SELECT usability.state, session.recovery_acknowledged_at,
                        session.shutdown_clean,
                        (SELECT COUNT(*) FROM session_checkpoints
                         WHERE session_id = session.session_id)
                 FROM mux_sessions AS session
                 INNER JOIN session_recovery_usability AS usability
                    ON usability.session_id = session.session_id
                 WHERE session.session_id = 'reconciled-clean-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(recovered, ("dirty".to_string(), None, 0, 2));
    }

    #[test]
    fn metadata_admission_precedes_auxiliary_database_reads() {
        run_async_test(async {
            // Deliberately leave this SQLite file without snapshot, event, or
            // scrollback tables. If auxiliary reads run first, the requested
            // scrollback projection fails with a database error instead of
            // the deterministic metadata-admission result asserted below.
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let db_path = Arc::new(tmp.path().to_string_lossy().into_owned());
            let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
            let metadata = Value::String("m".repeat(MAX_CHECKPOINT_METADATA_BYTES));

            let error = engine
                .capture_with_options(
                    &[make_test_pane(1, 24, 80)],
                    SnapshotTrigger::Manual,
                    SnapshotCaptureOptions {
                        include_scrollback: true,
                        metadata: Some(metadata),
                    },
                )
                .await
                .expect_err("oversized metadata must fail before auxiliary database reads");

            assert!(matches!(
                error,
                SnapshotError::ProjectionResourceLimit {
                    resource: SnapshotProjectionResource::MetadataBytes,
                    observed,
                    limit: MAX_CHECKPOINT_METADATA_BYTES,
                } if observed > MAX_CHECKPOINT_METADATA_BYTES
            ));
            assert_eq!(
                engine
                    .auxiliary_projection_read_attempts
                    .load(Ordering::Relaxed),
                0,
                "metadata rejection must not enter the auxiliary read phase"
            );
        });
    }

    #[test]
    fn periodic_dedup_recomputes_persisted_pane_witness_before_skipping() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];
            let first = engine
                .capture(&panes, SnapshotTrigger::Periodic)
                .await
                .unwrap();
            Connection::open(db_path.as_str())
                .unwrap()
                .execute(
                    "UPDATE mux_pane_state
                     SET terminal_state_json = '{}'
                     WHERE checkpoint_id = ?1",
                    [first.checkpoint_id],
                )
                .unwrap();

            let healed = engine
                .capture(&panes, SnapshotTrigger::Periodic)
                .await
                .expect("a corrupt cached row must be replaced, not deduplicated");
            assert_eq!(healed.pane_count, 1);
            assert_eq!(checkpoint_count(db_path.as_str()), 2);
            assert_eq!(engine.telemetry().snapshot().dedup_skips, 0);
        });
    }

    #[test]
    fn dedup_does_not_skip_manual() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let r1 = engine.capture(&panes, SnapshotTrigger::Manual).await;
            assert!(r1.is_ok());

            // Manual capture should NOT be skipped even if unchanged
            let r2 = engine.capture(&panes, SnapshotTrigger::Manual).await;
            assert!(r2.is_ok());
        });
    }

    #[test]
    fn empty_panes_returns_error() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

            let result = engine.capture(&[], SnapshotTrigger::Manual).await;
            assert!(matches!(result, Err(SnapshotError::NoPanes)));
        });
    }

    #[test]
    fn capture_rejects_panes_above_legacy_topology_cap_before_db_mutation() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = (0..=MAX_TOPOLOGY_PANES)
                .map(|pane_id| {
                    make_test_pane(
                        u64::try_from(pane_id).expect("test pane id fits u64"),
                        24,
                        80,
                    )
                })
                .collect::<Vec<_>>();

            let error = engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .expect_err("writer must reject a self-unrestorable topology");
            assert!(matches!(
                error,
                SnapshotError::Topology(TopologySnapshotError::ResourceLimit {
                    resource: "panes",
                    count,
                    limit: MAX_TOPOLOGY_PANES,
                }) if count == MAX_TOPOLOGY_PANES + 1
            ));

            let conn = Connection::open(db_path.as_str()).expect("open verification database");
            let sessions: i64 = conn
                .query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
                .expect("count sessions");
            let checkpoints: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .expect("count checkpoints");
            assert_eq!((sessions, checkpoints), (0, 0));
        });
    }

    #[test]
    fn capture_rejects_duplicate_pane_ids_before_db_mutation() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(0, 24, 80), make_test_pane(0, 24, 80)];

            let error = engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .expect_err("writer must reject duplicate pane identities");
            assert!(matches!(
                error,
                SnapshotError::Topology(TopologySnapshotError::InvalidStructure {
                    reason: "duplicate pane id",
                })
            ));

            let conn = Connection::open(db_path.as_str()).expect("open verification database");
            let sessions: i64 = conn
                .query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
                .expect("count sessions");
            let checkpoints: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .expect("count checkpoints");
            assert_eq!((sessions, checkpoints), (0, 0));
        });
    }

    #[test]
    fn session_reused_across_captures() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

            let panes1 = vec![make_test_pane(1, 24, 80)];
            let panes2 = vec![make_test_pane(1, 30, 120)]; // changed size

            let r1 = engine
                .capture(&panes1, SnapshotTrigger::Startup)
                .await
                .unwrap();
            let r2 = engine
                .capture(&panes2, SnapshotTrigger::Periodic)
                .await
                .unwrap();

            // Same session, different checkpoints
            assert_eq!(r1.session_id, r2.session_id);
            assert_ne!(r1.checkpoint_id, r2.checkpoint_id);
        });
    }

    #[test]
    fn cleanup_removes_old_checkpoints() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                retention_count: 2,
                retention_days: 365, // don't prune by age in this test
                ..SnapshotConfig::default()
            };
            let engine = SnapshotEngine::new(db_path.clone(), config);

            // Create 4 snapshots with different pane data
            for i in 0..4u64 {
                let panes = vec![make_test_pane(i, 24 + i as u32, 80)];
                engine
                    .capture(&panes, SnapshotTrigger::Manual)
                    .await
                    .unwrap();
            }

            // Should have 4 checkpoints
            let conn = Connection::open(db_path.as_str()).unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 4);

            // Cleanup should remove 2 (keep latest 2)
            let deleted = engine.cleanup().await.unwrap();
            assert_eq!(deleted, 2);

            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 2);
        });
    }

    /// [ft-rfpk6] cleanup_sync calls `DELETE FROM session_checkpoints`
    /// and relies on the schema's `ON DELETE CASCADE` to remove the
    /// referencing mux_pane_state rows. Before this fix, open_conn
    /// omitted `PRAGMA foreign_keys=ON`, so the DELETE succeeded but
    /// child rows were silently orphaned — growing forever across
    /// every cleanup run.
    #[test]
    fn ft_rfpk6_cleanup_cascades_to_mux_pane_state() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                retention_count: 1,
                retention_days: 365,
                ..SnapshotConfig::default()
            };
            let engine = SnapshotEngine::new(db_path.clone(), config);

            // Capture 3 snapshots with a pane each → 3 checkpoints and
            // at least 3 mux_pane_state rows.
            for i in 0..3u64 {
                let panes = vec![make_test_pane(i, 24 + i as u32, 80)];
                engine
                    .capture(&panes, SnapshotTrigger::Manual)
                    .await
                    .unwrap();
            }

            let conn = Connection::open(db_path.as_str()).unwrap();
            let cp_before: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |r| r.get(0))
                .unwrap();
            let ps_before: i64 = conn
                .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |r| r.get(0))
                .unwrap();
            assert_eq!(cp_before, 3, "precondition: 3 checkpoints");
            assert!(ps_before >= 3, "precondition: ≥3 pane-state rows");

            let deleted = engine.cleanup().await.unwrap();
            assert_eq!(deleted, 2, "retention_count=1 keeps 1, deletes 2");

            let cp_after: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |r| r.get(0))
                .unwrap();
            assert_eq!(cp_after, 1);

            // With the fix: FKs ON → CASCADE fires → orphan child rows
            // from the 2 deleted checkpoints are gone.
            let orphans: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM mux_pane_state ps
                     WHERE NOT EXISTS (
                         SELECT 1 FROM session_checkpoints c
                         WHERE c.id = ps.checkpoint_id
                     )",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                orphans, 0,
                "ft-rfpk6: mux_pane_state must not contain rows whose \
                 checkpoint_id no longer exists after cleanup"
            );
        });
    }

    #[test]
    fn save_checkpoint_sync_updates_session_row_to_match_checkpoint() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-atomic",
            1000,
            r#"{"version":"old"}"#,
            crate::VERSION,
        )
        .unwrap();

        let pane = PaneStateSnapshot::from_pane_info(&make_test_pane(7, 24, 80), 2000, false);
        let prepared = prepare_test_snapshot(r#"{"version":"new"}"#, &[pane]);
        let receipt = save_checkpoint_sync(
            db_path.as_str(),
            "sess-atomic",
            2000,
            "event",
            &prepared,
            None,
        )
        .unwrap();
        assert_eq!(receipt.session_id, "sess-atomic");

        let conn = Connection::open(db_path.as_str()).unwrap();
        let (last_checkpoint_at, topology_json): (Option<i64>, String) = conn
            .query_row(
                "SELECT last_checkpoint_at, topology_json
                 FROM mux_sessions
                 WHERE session_id = 'sess-atomic'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_checkpoint_at, Some(2000));
        assert_eq!(topology_json, r#"{"version":"new"}"#);

        let (checkpoint_at, pane_count): (i64, i64) = conn
            .query_row(
                "SELECT checkpoint_at, pane_count
                 FROM session_checkpoints
                 WHERE id = ?1",
                [receipt.checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(checkpoint_at, 2000);
        assert_eq!(pane_count, 1);

        let corrupt_pane_info = make_test_pane(8, 24, 80);
        let (corrupt_topology, _) =
            TopologySnapshot::from_panes(std::slice::from_ref(&corrupt_pane_info), 3000);
        let mut corrupt_pane = PaneStateSnapshot::from_pane_info(&corrupt_pane_info, 3000, false);
        corrupt_pane.scrollback_ref = Some(ScrollbackRef {
            output_segments_seq: -1,
            retained_segment_count: 1,
            last_capture_at: 3000,
        });
        let error = prepare_snapshot_persistence(&corrupt_topology, &[corrupt_pane], None)
            .expect_err("a negative scrollback sequence cannot be prepared");
        assert!(matches!(
            error,
            SnapshotPreparationError::NegativeScrollbackSequence
        ));

        let wide_topology = TopologySnapshot {
            schema_version: crate::session_topology::TOPOLOGY_SCHEMA_VERSION,
            captured_at: 3_000,
            workspace_id: None,
            windows: vec![crate::session_topology::WindowSnapshot {
                window_id: 1,
                title: None,
                position: None,
                size: None,
                tabs: vec![crate::session_topology::TabSnapshot {
                    tab_id: 1,
                    title: None,
                    pane_tree: crate::session_topology::PaneNode::VSplit {
                        children: (0..=crate::session_topology::MAX_SPLIT_FANOUT)
                            .map(|pane_id| {
                                (
                                    1.0,
                                    crate::session_topology::PaneNode::Leaf {
                                        pane_id: u64::try_from(pane_id)
                                            .expect("test pane id fits u64"),
                                        rows: 24,
                                        cols: 80,
                                        cwd: None,
                                        title: None,
                                        is_active: pane_id == 0,
                                    },
                                )
                            })
                            .collect(),
                    },
                    active_pane_id: Some(0),
                }],
                active_tab_index: None,
            }],
        };
        let error = prepare_snapshot_persistence(&wide_topology, &[], None)
            .expect_err("persistence preparation must enforce topology admission");
        assert!(matches!(
            error,
            SnapshotPreparationError::Topology(TopologySnapshotError::ResourceLimit {
                resource: "split_fanout",
                ..
            })
        ));

        let oversized_topology = TopologySnapshot {
            schema_version: crate::session_topology::TOPOLOGY_SCHEMA_VERSION,
            captured_at: 3_000,
            workspace_id: Some(
                "x".repeat(crate::session_topology::MAX_SNAPSHOT_BYTES.saturating_add(1)),
            ),
            windows: Vec::new(),
        };
        let error = prepare_snapshot_persistence(&oversized_topology, &[], None)
            .expect_err("persistence must not write topology JSON its reader rejects");
        assert!(matches!(
            error,
            SnapshotPreparationError::Topology(TopologySnapshotError::TooLarge {
                bytes,
                limit: crate::session_topology::MAX_SNAPSHOT_BYTES,
            }) if bytes > crate::session_topology::MAX_SNAPSHOT_BYTES
        ));

        let checkpoint_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            checkpoint_count, 1,
            "invalid snapshot preparation must not add a checkpoint"
        );
    }

    #[test]
    fn actual_persistence_projection_bounds_every_pane_text_column() {
        let mut raw_pane = make_test_pane(41, 24, 80);
        raw_pane.workspace = Some("w".repeat(100_000));
        raw_pane.title = Some("t".repeat(100_000));
        raw_pane.cwd = Some("c".repeat(100_000));
        raw_pane.extra.insert(
            FOREGROUND_PROCESS_NAME_FIELD.to_owned(),
            Value::String("p".repeat(100_000)),
        );
        raw_pane.extra.insert(
            "unrelated_backend_field".to_owned(),
            Value::String("u".repeat(100_000)),
        );

        let projection =
            project_snapshot_panes(&[raw_pane]).expect("one pane is inside topology admission");
        assert_eq!(projection.panes.len(), 1);
        assert!(projection.truncated_pane_ids.contains(&41));
        let projected_pane = &projection.panes[0];
        assert!(projected_pane.title.as_ref().unwrap().len() <= SNAPSHOT_TEXT_FIELD_INPUT_BYTES);
        assert!(
            projected_pane.cwd.is_none(),
            "an oversized cwd must be omitted rather than rewritten into a synthetic path"
        );
        assert!(projected_pane.workspace.is_none());
        assert!(!projected_pane.extra.contains_key("unrelated_backend_field"));
        assert!(
            projected_pane.extra.is_empty(),
            "an oversized process identity must be omitted rather than shortened"
        );
        assert!(
            projection.topology_workspace_id.is_none(),
            "an oversized workspace must be omitted instead of persisted under a synthetic prefix"
        );

        let (mut topology, _) = TopologySnapshot::from_panes(&projection.panes, 4_100);
        topology
            .workspace_id
            .clone_from(&projection.topology_workspace_id);
        let mut pane = PaneStateSnapshot::from_pane_info(projected_pane, 4_100, false);
        let hostile = "\u{1}".repeat(100_000);
        pane.terminal.title = hostile.clone();
        pane.cwd = Some(hostile.clone());
        pane.foreground_process = Some(crate::session_pane_state::ProcessInfo {
            name: hostile.clone(),
            pid: Some(99),
            argv: Some(vec![hostile.clone()]),
        });
        pane.env = Some(CapturedEnv {
            vars: HashMap::from([
                ("PATH".to_owned(), hostile.clone()),
                ("UNSAFE_PROGRAMMATIC_NAME".to_owned(), hostile.clone()),
            ]),
            redacted_count: 0,
        });
        pane.agent = Some(AgentMetadata {
            agent_type: hostile.clone(),
            session_id: Some(hostile.clone()),
            state: Some(hostile),
        });

        let prepared = prepare_snapshot_persistence_with_prebounded_panes(
            &topology,
            &[pane],
            None,
            &projection.truncated_pane_ids,
        )
        .expect("oversized optional observations should be bounded");
        assert_eq!(prepared.truncated_pane_count, 1);
        assert_eq!(prepared.panes.len(), 1);
        assert!(
            prepared.topology_json.len() <= crate::session_topology::MAX_SNAPSHOT_BYTES,
            "the topology must be built from the bounded projection"
        );
        let row_bytes = persisted_pane_text_bytes(&prepared.panes[0]).unwrap();
        assert!(row_bytes <= MAX_PERSISTED_PANE_TEXT_BYTES);
        assert_eq!(
            prepared.persisted_text_bytes,
            persisted_checkpoint_text_bytes(
                Some(&prepared.topology_json),
                prepared.metadata_json.as_deref(),
                &prepared.panes,
            )
            .unwrap()
        );
        assert!(prepared.persisted_text_bytes <= MAX_PERSISTED_CHECKPOINT_TEXT_BYTES);
        assert!(
            prepared.panes[0].command.is_none(),
            "oversized process identity must be omitted"
        );
        assert!(
            prepared.panes[0].cwd.is_none(),
            "oversized cwd must not become a different restorable path"
        );
        let persisted_env: CapturedEnv = serde_json::from_str(
            prepared.panes[0]
                .env_json
                .as_deref()
                .expect("bounded environment envelope remains useful"),
        )
        .expect("bounded environment JSON decodes");
        assert!(
            persisted_env.vars.is_empty(),
            "oversized and non-allow-listed environment values must be omitted"
        );
        assert!(
            prepared.panes[0].agent_metadata_json.is_none(),
            "oversized agent identity must be omitted as one semantic unit"
        );
    }

    #[test]
    fn persistence_projection_rejects_equal_cardinality_pane_identity_mismatch() {
        let topology_pane = make_test_pane(41, 24, 80);
        let (topology, _) = TopologySnapshot::from_panes(&[topology_pane], 4_150);
        let pane_state =
            PaneStateSnapshot::from_pane_info(&make_test_pane(42, 24, 80), 4_150, false);

        let preparation_error = prepare_snapshot_persistence(&topology, &[pane_state], None)
            .expect_err("different pane-id sets must fail before persistence");
        match &preparation_error {
            SnapshotPreparationError::PaneIdentitySetMismatch {
                topology_panes,
                pane_states,
            } => {
                assert_eq!(*topology_panes, 1);
                assert_eq!(*pane_states, 1);
            }
            other => panic!("unexpected preparation error: {other}"),
        }

        assert!(matches!(
            SnapshotError::from(preparation_error),
            SnapshotError::PaneIdentitySetMismatch {
                topology_panes: 1,
                pane_states: 1,
            }
        ));
    }

    #[test]
    fn pane_projection_rejects_over_limit_pane_count() {
        let panes = (0..=MAX_TOPOLOGY_PANES)
            .map(|pane_id| {
                make_test_pane(
                    u64::try_from(pane_id).expect("test pane id is representable"),
                    24,
                    80,
                )
            })
            .collect::<Vec<_>>();

        let error = match project_snapshot_panes(&panes) {
            Ok(_) => panic!("over-limit pane projection must fail"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            SnapshotError::Topology(TopologySnapshotError::ResourceLimit {
                resource: "panes",
                count,
                limit: MAX_TOPOLOGY_PANES,
            }) if count == MAX_TOPOLOGY_PANES + 1
        ));
    }

    #[test]
    fn pane_projection_does_not_alias_distinct_long_workspaces() {
        let shared_prefix = "w".repeat(SNAPSHOT_TEXT_FIELD_INPUT_BYTES + 1);
        let mut first = make_test_pane(51, 24, 80);
        first.workspace = Some(format!("{shared_prefix}-first"));
        let mut second = make_test_pane(52, 24, 80);
        second.workspace = Some(format!("{shared_prefix}-second"));
        let mut ordinary = make_test_pane(53, 24, 80);
        ordinary.workspace = Some("ordinary-workspace".to_string());

        let projection = project_snapshot_panes(&[first, second, ordinary])
            .expect("three panes are inside topology admission");
        assert!(
            projection.topology_workspace_id.is_none(),
            "workspace equality must be decided before bounding common prefixes"
        );
        assert_eq!(
            projection.truncated_pane_ids,
            HashSet::from([51, 52]),
            "global workspace omission must count only panes whose own identity was omitted"
        );
    }

    #[test]
    fn actual_persistence_projection_rejects_oversized_metadata() {
        let metadata = Value::String("m".repeat(MAX_CHECKPOINT_METADATA_BYTES));
        let error =
            prepare_snapshot_persistence(&TopologySnapshot::empty(4_200), &[], Some(&metadata))
                .expect_err("encoded metadata over the hard limit must fail closed");
        assert!(matches!(
            error,
            SnapshotPreparationError::MetadataResourceLimit {
                bytes,
                limit: MAX_CHECKPOINT_METADATA_BYTES,
            } if bytes > MAX_CHECKPOINT_METADATA_BYTES
        ));
    }

    #[test]
    fn actual_persistence_projection_accepts_exact_metadata_boundaries() {
        let exact_bytes =
            Value::String("m".repeat(MAX_CHECKPOINT_METADATA_BYTES.saturating_sub(2)));
        let exact_bytes_prepared =
            prepare_snapshot_persistence(&TopologySnapshot::empty(4_200), &[], Some(&exact_bytes))
                .expect("metadata at the exact encoded-byte limit must be admitted");
        assert_eq!(
            exact_bytes_prepared.metadata_json.as_deref().map(str::len),
            Some(MAX_CHECKPOINT_METADATA_BYTES)
        );

        let mut exact_depth = Value::Null;
        for _ in 1..SNAPSHOT_METADATA_MAX_DEPTH {
            exact_depth = Value::Array(vec![exact_depth]);
        }
        prepare_snapshot_persistence(&TopologySnapshot::empty(4_200), &[], Some(&exact_depth))
            .expect("metadata at the exact nesting-depth limit must be admitted");

        let exact_nodes = Value::Array(
            std::iter::repeat_n(Value::Null, SNAPSHOT_METADATA_MAX_NODES - 1).collect(),
        );
        prepare_snapshot_persistence(&TopologySnapshot::empty(4_200), &[], Some(&exact_nodes))
            .expect("metadata at the exact node-count limit must be admitted");
    }

    #[test]
    fn snapshot_capture_options_drop_deep_metadata_iteratively() {
        let mut metadata = Value::Null;
        for _ in 0..100_000 {
            metadata = Value::Array(vec![metadata]);
        }
        drop(SnapshotCaptureOptions {
            include_scrollback: false,
            metadata: Some(metadata),
        });
    }

    #[test]
    fn actual_persistence_projection_rejects_excessive_metadata_shape() {
        let mut deep_metadata = Value::Null;
        for _ in 0..SNAPSHOT_METADATA_MAX_DEPTH {
            deep_metadata = Value::Array(vec![deep_metadata]);
        }
        let depth_error = prepare_snapshot_persistence(
            &TopologySnapshot::empty(4_200),
            &[],
            Some(&deep_metadata),
        )
        .expect_err("metadata beyond the nesting limit must fail before recursive encoding");
        assert!(matches!(
            depth_error,
            SnapshotPreparationError::MetadataShapeResourceLimit {
                resource: SnapshotProjectionResource::MetadataDepth,
                observed,
                limit: SNAPSHOT_METADATA_MAX_DEPTH,
            } if observed == SNAPSHOT_METADATA_MAX_DEPTH + 1
        ));

        let wide_metadata =
            Value::Array(std::iter::repeat_n(Value::Null, SNAPSHOT_METADATA_MAX_NODES).collect());
        let node_error = prepare_snapshot_persistence(
            &TopologySnapshot::empty(4_200),
            &[],
            Some(&wide_metadata),
        )
        .expect_err("metadata beyond the node limit must fail before canonicalization");
        assert!(matches!(
            node_error,
            SnapshotPreparationError::MetadataShapeResourceLimit {
                resource: SnapshotProjectionResource::MetadataNodes,
                observed,
                limit: SNAPSHOT_METADATA_MAX_NODES,
            } if observed == SNAPSHOT_METADATA_MAX_NODES + 1
        ));
    }

    #[test]
    fn existing_session_checkpoint_resets_clean_flag_until_new_mark_receipt() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-reopened",
            1_000,
            r#"{"version":"initial"}"#,
            crate::VERSION,
        )
        .unwrap();
        let first_pane =
            PaneStateSnapshot::from_pane_info(&make_test_pane(1, 24, 80), 2_000, false);
        let first = prepare_test_snapshot(r#"{"version":"first"}"#, &[first_pane]);
        let first_receipt = save_checkpoint_sync(
            db_path.as_str(),
            "sess-reopened",
            2_000,
            "shutdown",
            &first,
            None,
        )
        .unwrap();
        mark_shutdown_sync(
            db_path.as_str(),
            "sess-reopened",
            first_receipt.checkpoint_id,
            2_000,
            &first_receipt.state_hash,
        )
        .unwrap();

        let second_pane =
            PaneStateSnapshot::from_pane_info(&make_test_pane(1, 30, 100), 3_000, false);
        let second = prepare_test_snapshot(r#"{"version":"second"}"#, &[second_pane]);
        save_checkpoint_sync(
            db_path.as_str(),
            "sess-reopened",
            3_000,
            "shutdown",
            &second,
            None,
        )
        .unwrap();

        let clean: i64 = Connection::open(db_path.as_str())
            .unwrap()
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-reopened'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            clean, 0,
            "a newer checkpoint must make the session unclean until its own mark settles"
        );
    }

    #[test]
    fn mark_shutdown_requires_exact_deterministic_latest_snapshot_identity() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-exact-close",
            1_000,
            r#"{"version":"initial"}"#,
            crate::VERSION,
        )
        .unwrap();
        let first = prepare_test_snapshot(
            r#"{"version":"first"}"#,
            &[PaneStateSnapshot::from_pane_info(
                &make_test_pane(1, 24, 80),
                2_000,
                false,
            )],
        );
        let first_receipt = save_checkpoint_sync(
            db_path.as_str(),
            "sess-exact-close",
            2_000,
            "event",
            &first,
            None,
        )
        .unwrap();
        let latest = prepare_test_snapshot(
            r#"{"version":"latest"}"#,
            &[PaneStateSnapshot::from_pane_info(
                &make_test_pane(1, 30, 100),
                2_000,
                false,
            )],
        );
        let latest_receipt = save_checkpoint_sync(
            db_path.as_str(),
            "sess-exact-close",
            2_000,
            "event",
            &latest,
            None,
        )
        .unwrap();

        assert!(
            mark_shutdown_sync(
                db_path.as_str(),
                "sess-exact-close",
                first_receipt.checkpoint_id,
                2_000,
                &first_receipt.state_hash,
            )
            .is_err(),
            "the lower ID in a timestamp tie is not the latest checkpoint"
        );
        assert!(
            mark_shutdown_sync(
                db_path.as_str(),
                "sess-exact-close",
                latest_receipt.checkpoint_id,
                2_000,
                &first_receipt.state_hash,
            )
            .is_err(),
            "a stale witness must not authorize a clean transition"
        );
        mark_shutdown_sync(
            db_path.as_str(),
            "sess-exact-close",
            latest_receipt.checkpoint_id,
            2_000,
            &latest_receipt.state_hash,
        )
        .unwrap();

        let receipt_id = insert_checkpoint_fixture(
            db_path.as_str(),
            "sess-exact-close",
            3_000,
            "restore_receipt",
            "restore",
            None,
            0,
        );
        set_session_checkpoint_summary(
            db_path.as_str(),
            "sess-exact-close",
            3_000,
            r#"{"version":"latest"}"#,
            None,
        );
        assert!(
            mark_shutdown_sync(
                db_path.as_str(),
                "sess-exact-close",
                receipt_id,
                3_000,
                "restore",
            )
            .is_err(),
            "a restore receipt cannot masquerade as a shutdown snapshot"
        );
    }

    #[test]
    fn mark_shutdown_requires_verified_witness_and_matching_session_summary() {
        let (_witness_tmp, witness_db) = setup_test_db();
        create_session_sync(
            witness_db.as_str(),
            "sess-corrupt-close",
            1_000,
            r#"{"version":"initial"}"#,
            crate::VERSION,
        )
        .unwrap();
        let prepared = prepare_test_snapshot(
            r#"{"version":"final"}"#,
            &[PaneStateSnapshot::from_pane_info(
                &make_test_pane(1, 24, 80),
                2_000,
                false,
            )],
        );
        let receipt = save_checkpoint_sync(
            witness_db.as_str(),
            "sess-corrupt-close",
            2_000,
            "shutdown",
            &prepared,
            None,
        )
        .unwrap();
        Connection::open(witness_db.as_str())
            .unwrap()
            .execute(
                "UPDATE mux_pane_state SET terminal_state_json = '{}'
                 WHERE checkpoint_id = ?1",
                [receipt.checkpoint_id],
            )
            .unwrap();
        assert!(
            mark_shutdown_sync(
                witness_db.as_str(),
                "sess-corrupt-close",
                receipt.checkpoint_id,
                2_000,
                &receipt.state_hash,
            )
            .is_err(),
            "matching identity columns cannot authorize a corrupt pane projection"
        );

        let (_summary_tmp, summary_db) = setup_test_db();
        create_session_sync(
            summary_db.as_str(),
            "sess-stale-summary",
            1_000,
            r#"{"version":"initial"}"#,
            crate::VERSION,
        )
        .unwrap();
        let prepared = prepare_test_snapshot(r#"{"version":"final"}"#, &[]);
        let receipt = save_checkpoint_sync(
            summary_db.as_str(),
            "sess-stale-summary",
            2_000,
            "shutdown",
            &prepared,
            None,
        )
        .unwrap();
        Connection::open(summary_db.as_str())
            .unwrap()
            .execute(
                "UPDATE mux_sessions
                 SET last_checkpoint_at = 1999, topology_json = '{}'
                 WHERE session_id = 'sess-stale-summary'",
                [],
            )
            .unwrap();
        assert!(
            mark_shutdown_sync(
                summary_db.as_str(),
                "sess-stale-summary",
                receipt.checkpoint_id,
                2_000,
                &receipt.state_hash,
            )
            .is_err(),
            "clean marking must compare-and-swap the exact session summary"
        );
        let clean: i64 = Connection::open(summary_db.as_str())
            .unwrap()
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions
                 WHERE session_id = 'sess-stale-summary'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(clean, 0);
    }

    #[test]
    fn causal_checkpoint_id_beats_rolled_back_wall_clock_for_clean_marking() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-clock-rollback",
            500,
            r#"{"version":"initial"}"#,
            crate::VERSION,
        )
        .unwrap();
        let first = prepare_test_snapshot(r#"{"version":"first"}"#, &[]);
        save_checkpoint_sync(
            db_path.as_str(),
            "sess-clock-rollback",
            3_000,
            "event",
            &first,
            None,
        )
        .unwrap();
        let causal_latest = prepare_test_snapshot(r#"{"version":"second"}"#, &[]);
        let latest_receipt = save_checkpoint_sync(
            db_path.as_str(),
            "sess-clock-rollback",
            1_000,
            "shutdown",
            &causal_latest,
            None,
        )
        .unwrap();

        mark_shutdown_sync(
            db_path.as_str(),
            "sess-clock-rollback",
            latest_receipt.checkpoint_id,
            1_000,
            &latest_receipt.state_hash,
        )
        .expect("the causally latest inserted checkpoint must remain close authority");
    }

    #[test]
    fn checkpoint_delete_missing_target_is_retry_safe_acknowledged_noop() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

            let deleted = engine
                .delete_checkpoint(SnapshotDeleteTarget::Id(9_999))
                .await
                .expect("a missing target is an acknowledged no-op");
            assert!(deleted.is_none());
            assert_eq!(checkpoint_count(db_path.as_str()), 0);
            assert!(
                !engine
                    .snapshot_authority
                    .reconciliation_required
                    .load(Ordering::Acquire),
                "a read-only miss must not latch indeterminate authority"
            );
        });
    }

    #[test]
    fn checkpoint_delete_cascades_and_reconciles_final_clean_snapshot() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            create_session_sync(
                db_path.as_str(),
                "sess-delete-final",
                1_000,
                r#"{"version":"final"}"#,
                crate::VERSION,
            )
            .unwrap();
            let checkpoint_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-delete-final",
                2_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000003",
                Some(r#"{"version":"final"}"#),
                64,
            );
            let conn = Connection::open(db_path.as_str()).unwrap();
            conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
            conn.execute(
                "INSERT INTO mux_pane_state
                 (checkpoint_id, pane_id, terminal_state_json)
                 VALUES (?1, 7, '{}')",
                [checkpoint_id],
            )
            .unwrap();
            drop(conn);
            set_session_checkpoint_summary(
                db_path.as_str(),
                "sess-delete-final",
                2_000,
                r#"{"version":"final"}"#,
                Some(checkpoint_id),
            );

            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let deleted = engine
                .delete_checkpoint(SnapshotDeleteTarget::Id(checkpoint_id))
                .await
                .unwrap()
                .expect("checkpoint should exist");
            assert_eq!(deleted.identity.checkpoint_id, checkpoint_id);
            assert_eq!(deleted.recorded_payload_bytes, 64);
            assert!(deleted.invalidated_clean_state);

            let conn = Connection::open(db_path.as_str()).unwrap();
            let pane_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
                .unwrap();
            let summary: (Option<i64>, i64, Option<i64>, String) = conn
                .query_row(
                    "SELECT last_checkpoint_at, shutdown_clean, clean_checkpoint_id, topology_json
                     FROM mux_sessions WHERE session_id = 'sess-delete-final'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(checkpoint_count(db_path.as_str()), 0);
            assert_eq!(pane_count, 0, "foreign-key pane rows must cascade");
            assert_eq!(summary.0, None);
            assert_eq!(summary.1, 0);
            assert_eq!(summary.2, None);
            assert_eq!(summary.3, r#"{"version":"final"}"#);
        });
    }

    #[test]
    fn checkpoint_delete_nonlatest_preserves_exact_clean_latest_summary() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            create_session_sync(
                db_path.as_str(),
                "sess-delete-old",
                500,
                r#"{"version":"initial"}"#,
                crate::VERSION,
            )
            .unwrap();
            let old_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-delete-old",
                1_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000004",
                Some(r#"{"version":"old"}"#),
                10,
            );
            let latest_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-delete-old",
                2_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000005",
                Some(r#"{"version":"latest"}"#),
                20,
            );
            set_session_checkpoint_summary(
                db_path.as_str(),
                "sess-delete-old",
                2_000,
                r#"{"version":"latest"}"#,
                Some(latest_id),
            );

            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let deleted = engine
                .delete_checkpoint(SnapshotDeleteTarget::Exact(SnapshotCheckpointIdentity {
                    checkpoint_id: old_id,
                    session_id: "sess-delete-old".to_string(),
                    checkpoint_at: 1_000,
                    checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
                    state_hash: "0000000000000004".to_string(),
                }))
                .await
                .unwrap()
                .expect("old checkpoint should match exact identity");
            assert!(!deleted.invalidated_clean_state);

            let summary: (Option<i64>, i64, Option<i64>, String) =
                Connection::open(db_path.as_str())
                    .unwrap()
                    .query_row(
                        "SELECT last_checkpoint_at, shutdown_clean, clean_checkpoint_id,
                                topology_json
                         FROM mux_sessions WHERE session_id = 'sess-delete-old'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .unwrap();
            assert_eq!(summary.0, Some(2_000));
            assert_eq!(summary.1, 1);
            assert_eq!(summary.2, Some(latest_id));
            assert_eq!(summary.3, r#"{"version":"latest"}"#);
        });
    }

    #[test]
    fn deleting_latest_snapshot_invalidates_newer_clean_restore_receipt() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            create_session_sync(
                db_path.as_str(),
                "sess-delete-snapshot",
                500,
                r#"{"version":"restored"}"#,
                crate::VERSION,
            )
            .unwrap();
            let snapshot_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-delete-snapshot",
                1_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000006",
                Some(r#"{"version":"restored"}"#),
                20,
            );
            let receipt_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-delete-snapshot",
                2_000,
                "restore_receipt",
                "restore",
                None,
                0,
            );
            set_session_checkpoint_summary(
                db_path.as_str(),
                "sess-delete-snapshot",
                2_000,
                r#"{"version":"restored"}"#,
                Some(receipt_id),
            );

            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let deleted = engine
                .delete_checkpoint(SnapshotDeleteTarget::Latest(
                    SnapshotCheckpointRoleScope::Snapshot,
                ))
                .await
                .unwrap()
                .expect("latest snapshot should exist");
            assert_eq!(deleted.identity.checkpoint_id, snapshot_id);
            assert!(deleted.invalidated_clean_state);

            let summary: (Option<i64>, i64, Option<i64>, String) =
                Connection::open(db_path.as_str())
                    .unwrap()
                    .query_row(
                        "SELECT last_checkpoint_at, shutdown_clean, clean_checkpoint_id,
                                topology_json
                         FROM mux_sessions WHERE session_id = 'sess-delete-snapshot'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .unwrap();
            assert_eq!(summary.0, Some(2_000));
            assert_eq!(summary.1, 0);
            assert_eq!(summary.2, None);
            assert_eq!(summary.3, r#"{"version":"restored"}"#);
        });
    }

    #[test]
    fn checkpoint_delete_exact_identity_defeats_explicit_primary_key_reuse() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            create_session_sync(
                db_path.as_str(),
                "sess-rowid-reuse",
                500,
                r#"{"version":"first"}"#,
                crate::VERSION,
            )
            .unwrap();
            let reused_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-rowid-reuse",
                1_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000007",
                Some(r#"{"version":"first"}"#),
                10,
            );
            let stale_identity = SnapshotCheckpointIdentity {
                checkpoint_id: reused_id,
                session_id: "sess-rowid-reuse".to_string(),
                checkpoint_at: 1_000,
                checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
                state_hash: "0000000000000007".to_string(),
            };
            let conn = Connection::open(db_path.as_str()).unwrap();
            conn.execute("DELETE FROM session_checkpoints WHERE id = ?1", [reused_id])
                .unwrap();
            let inserted = conn
                .execute(
                    "INSERT INTO session_checkpoints
                     (id, session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
                      total_bytes, metadata_json, checkpoint_role, topology_json)
                     VALUES (?1, ?2, ?3, 'event', ?4, 0, ?5, NULL, ?6, ?7)",
                    rusqlite::params![
                        reused_id,
                        "sess-rowid-reuse",
                        1_000,
                        "0000000000000008",
                        11,
                        CHECKPOINT_ROLE_SNAPSHOT,
                        r#"{"version":"replacement"}"#,
                    ],
                )
                .unwrap();
            assert_eq!(
                inserted, 1,
                "the fixture must explicitly reuse the deleted primary key"
            );
            drop(conn);

            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let deleted = engine
                .delete_checkpoint(SnapshotDeleteTarget::Exact(stale_identity))
                .await
                .unwrap();
            assert!(deleted.is_none(), "stale preview identity must be a no-op");
            let replacement_hash: String = Connection::open(db_path.as_str())
                .unwrap()
                .query_row(
                    "SELECT state_hash FROM session_checkpoints WHERE id = ?1",
                    [reused_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(replacement_hash, "0000000000000008");
            assert!(
                !engine
                    .snapshot_authority
                    .reconciliation_required
                    .load(Ordering::Acquire)
            );
        });
    }

    #[test]
    fn cleanup_counts_only_snapshots_and_preserves_independent_receipts() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            create_session_sync(
                db_path.as_str(),
                "sess-role-retention",
                500,
                r#"{"version":"latest"}"#,
                crate::VERSION,
            )
            .unwrap();
            let old_snapshot_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-role-retention",
                1_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000009",
                Some(r#"{"version":"old"}"#),
                10,
            );
            let latest_snapshot_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-role-retention",
                2_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "000000000000000a",
                Some(r#"{"version":"latest"}"#),
                20,
            );
            let receipt_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-role-retention",
                3_000,
                "restore_receipt",
                "restore",
                None,
                0,
            );
            set_session_checkpoint_summary(
                db_path.as_str(),
                "sess-role-retention",
                3_000,
                r#"{"version":"latest"}"#,
                Some(receipt_id),
            );

            let config = SnapshotConfig {
                retention_count: 1,
                retention_days: u64::MAX,
                ..SnapshotConfig::default()
            };
            let engine = SnapshotEngine::new(db_path.clone(), config);
            assert_eq!(engine.cleanup().await.unwrap(), 1);

            let conn = Connection::open(db_path.as_str()).unwrap();
            let remaining: Vec<(i64, String)> = conn
                .prepare(
                    "SELECT id, checkpoint_role FROM session_checkpoints
                     ORDER BY checkpoint_at, id",
                )
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                remaining,
                vec![
                    (latest_snapshot_id, CHECKPOINT_ROLE_SNAPSHOT.to_string()),
                    (receipt_id, "restore_receipt".to_string()),
                ]
            );
            assert!(!remaining.iter().any(|(id, _)| *id == old_snapshot_id));
            let clean: (i64, Option<i64>) = conn
                .query_row(
                    "SELECT shutdown_clean, clean_checkpoint_id
                     FROM mux_sessions WHERE session_id = 'sess-role-retention'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(clean, (1, Some(receipt_id)));
        });
    }

    #[test]
    fn unresolved_restore_source_is_protected_from_cleanup_and_explicit_delete() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            create_session_sync(
                db_path.as_str(),
                "sess-protected-source",
                500,
                r#"{"version":"source"}"#,
                crate::VERSION,
            )
            .unwrap();
            let source_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-protected-source",
                1_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000011",
                Some(r#"{"version":"source"}"#),
                0,
            );
            let unprotected_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-protected-source",
                2_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000012",
                Some(r#"{"version":"newer"}"#),
                0,
            );
            let intent_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-protected-source",
                3_000,
                "restore_intent",
                "restore",
                None,
                0,
            );
            let conn = Connection::open(db_path.as_str()).unwrap();
            conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
            conn.execute(
                "INSERT INTO restore_attempt_lifecycle (
                     intent_checkpoint_id, session_id, source_checkpoint_id,
                     outcome_checkpoint_id, status, created_at, resolved_at
                 ) VALUES (?1, ?2, ?3, NULL, 'intent', 3000, NULL)",
                rusqlite::params![intent_id, "sess-protected-source", source_id],
            )
            .unwrap();
            drop(conn);

            let config = SnapshotConfig {
                retention_count: 0,
                retention_days: u64::MAX,
                ..SnapshotConfig::default()
            };
            let engine = SnapshotEngine::new(db_path.clone(), config);
            assert_eq!(
                engine.cleanup().await.unwrap(),
                1,
                "cleanup may delete only the unprotected snapshot"
            );
            let conn = Connection::open(db_path.as_str()).unwrap();
            let remaining: Vec<i64> = conn
                .prepare("SELECT id FROM session_checkpoints ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(remaining, vec![source_id, intent_id]);
            assert!(!remaining.contains(&unprotected_id));
            drop(conn);

            let error = engine
                .delete_checkpoint(SnapshotDeleteTarget::Id(source_id))
                .await
                .expect_err("an unresolved restore source must not be explicitly deleted");
            assert!(matches!(
                error,
                SnapshotError::Database(ref message)
                    if message.contains("protected by an unresolved restore attempt")
            ));
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "retry-safe refusal must not mutate the protected chain"
            );
        });
    }

    #[test]
    fn save_checkpoint_total_bytes_matches_historical_json_contract_exactly() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-bytes",
            1_000,
            r#"{"version":"bytes"}"#,
            crate::VERSION,
        )
        .unwrap();

        let mut pane = PaneStateSnapshot::from_pane_info(&make_test_pane(9, 24, 80), 2_000, false);
        pane.terminal.title = "終端🙂".to_owned();
        pane.cwd = Some("/ignored/路径".to_owned());
        pane.foreground_process = Some(crate::session_pane_state::ProcessInfo {
            name: "ignored-command".to_owned(),
            pid: Some(42),
            argv: None,
        });
        pane.env = Some(crate::session_pane_state::CapturedEnv {
            vars: std::collections::HashMap::from([("LANG".to_owned(), "日本語.UTF-8".to_owned())]),
            redacted_count: 0,
        });
        pane.agent = Some(crate::session_pane_state::AgentMetadata {
            agent_type: "codex-δ".to_owned(),
            session_id: Some("会話🙂".to_owned()),
            state: Some("working".to_owned()),
        });

        let expected = serde_json::to_string(&pane.terminal).unwrap().len()
            + serde_json::to_string(pane.env.as_ref().unwrap())
                .unwrap()
                .len()
            + serde_json::to_string(pane.agent.as_ref().unwrap())
                .unwrap()
                .len();
        let prepared = prepare_test_snapshot(r#"{"version":"bytes-2"}"#, &[pane]);
        let receipt = save_checkpoint_sync(
            db_path.as_str(),
            "sess-bytes",
            2_000,
            "event",
            &prepared,
            None,
        )
        .unwrap();
        assert_eq!(receipt.total_bytes, expected);

        let persisted_bytes: i64 = Connection::open(db_path.as_str())
            .unwrap()
            .query_row(
                "SELECT total_bytes FROM session_checkpoints WHERE id = ?1",
                [receipt.checkpoint_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted_bytes, i64::try_from(expected).unwrap());
    }

    #[test]
    fn save_checkpoint_sync_rolls_back_if_session_update_is_ignored() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-rollback",
            1000,
            r#"{"version":"old"}"#,
            crate::VERSION,
        )
        .unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch(
            "DROP TRIGGER mux_sessions_retained_size_au;
             CREATE TRIGGER mux_sessions_retained_size_au
             BEFORE UPDATE OF last_checkpoint_at ON mux_sessions
             WHEN OLD.session_id = 'sess-rollback'
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();
        drop(conn);

        let pane = PaneStateSnapshot::from_pane_info(&make_test_pane(9, 24, 80), 2000, false);
        let prepared = prepare_test_snapshot(r#"{"version":"new"}"#, &[pane]);
        let error = save_checkpoint_sync(
            db_path.as_str(),
            "sess-rollback",
            2000,
            "event",
            &prepared,
            None,
        )
        .expect_err("an ignored authority-row update must abort the checkpoint transaction");
        assert!(matches!(error, rusqlite::Error::StatementChangedRows(0)));

        let conn = Connection::open(db_path.as_str()).unwrap();
        let checkpoint_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        let pane_state_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap();
        let (last_checkpoint_at, topology_json): (Option<i64>, String) = conn
            .query_row(
                "SELECT last_checkpoint_at, topology_json
                 FROM mux_sessions WHERE session_id = 'sess-rollback'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);
        assert_eq!(pane_state_count, 0);
        assert_eq!(last_checkpoint_at, None);
        assert_eq!(topology_json, r#"{"version":"old"}"#);
    }

    #[test]
    fn save_checkpoint_sync_rejects_and_rolls_back_missing_size_trigger_dml() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-missing-size-trigger",
            1_000,
            r#"{"version":"old"}"#,
            crate::VERSION,
        )
        .unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch("DROP TRIGGER session_checkpoints_retained_size_ai;")
            .unwrap();
        drop(conn);

        let pane = PaneStateSnapshot::from_pane_info(&make_test_pane(10, 24, 80), 2_000, false);
        let prepared = prepare_test_snapshot(r#"{"version":"new"}"#, &[pane]);
        let error = save_checkpoint_sync(
            db_path.as_str(),
            "sess-missing-size-trigger",
            2_000,
            "event",
            &prepared,
            None,
        )
        .expect_err("a missing canonical summary mutation must invalidate the checkpoint receipt");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));

        let conn = Connection::open(db_path.as_str()).unwrap();
        let checkpoint_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        let pane_state_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap();
        let (last_checkpoint_at, topology_json): (Option<i64>, String) = conn
            .query_row(
                "SELECT last_checkpoint_at, topology_json
                 FROM mux_sessions WHERE session_id = 'sess-missing-size-trigger'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);
        assert_eq!(pane_state_count, 0);
        assert_eq!(last_checkpoint_at, None);
        assert_eq!(topology_json, r#"{"version":"old"}"#);
    }

    #[test]
    fn save_checkpoint_sync_rejects_and_rolls_back_extra_trigger_dml() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-authority",
            1000,
            r#"{"version":"old"}"#,
            crate::VERSION,
        )
        .unwrap();
        create_session_sync(
            db_path.as_str(),
            "sess-unrelated",
            1000,
            r#"{"version":"unrelated"}"#,
            crate::VERSION,
        )
        .unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER mutate_unrelated_session_after_checkpoint
             AFTER INSERT ON session_checkpoints
             BEGIN
                 UPDATE mux_sessions
                 SET topology_json = '{\"triggered\":true}'
                 WHERE session_id = 'sess-unrelated';
             END;",
        )
        .unwrap();
        drop(conn);

        let pane = PaneStateSnapshot::from_pane_info(&make_test_pane(10, 24, 80), 2000, false);
        let prepared = prepare_test_snapshot(r#"{"version":"new"}"#, &[pane]);
        let error = save_checkpoint_sync(
            db_path.as_str(),
            "sess-authority",
            2000,
            "event",
            &prepared,
            None,
        )
        .expect_err("unexpected trigger DML must invalidate the checkpoint receipt");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));

        let conn = Connection::open(db_path.as_str()).unwrap();
        let checkpoint_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        let unrelated_topology: String = conn
            .query_row(
                "SELECT topology_json FROM mux_sessions WHERE session_id = 'sess-unrelated'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);
        assert_eq!(unrelated_topology, r#"{"version":"unrelated"}"#);
    }

    #[test]
    fn cleanup_rejects_triggers_on_foreign_key_cascade_targets_before_delete() {
        let (_tmp, db_path) = setup_test_db();
        create_session_sync(
            db_path.as_str(),
            "sess-cleanup-trigger",
            1_000,
            r#"{"version":"old"}"#,
            crate::VERSION,
        )
        .unwrap();

        for checkpoint_at in [2_000_u64, 3_000_u64] {
            let pane = PaneStateSnapshot::from_pane_info(
                &make_test_pane(checkpoint_at, 24, 80),
                checkpoint_at,
                false,
            );
            let prepared = prepare_test_snapshot(r#"{"version":"new"}"#, &[pane]);
            save_checkpoint_sync(
                db_path.as_str(),
                "sess-cleanup-trigger",
                checkpoint_at,
                "event",
                &prepared,
                None,
            )
            .unwrap();
        }

        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER observe_cascaded_pane_delete
             AFTER DELETE ON MuX_PaNe_StAtE
             BEGIN
                 SELECT 1;
             END;",
        )
        .unwrap();
        drop(conn);

        let error = cleanup_sync(db_path.as_str(), 0, 365)
            .expect_err("a trigger on an FK cascade target must block cleanup before deletion");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));

        let conn = Connection::open(db_path.as_str()).unwrap();
        let checkpoint_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        let pane_state_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(checkpoint_count, 2);
        assert_eq!(pane_state_count, 2);
    }

    #[test]
    fn cleanup_retains_latest_checkpoints_per_session() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                retention_count: 1,
                retention_days: 365,
                ..SnapshotConfig::default()
            };
            let engine_a = SnapshotEngine::new(db_path.clone(), config.clone());
            let engine_b = SnapshotEngine::new(db_path.clone(), config.clone());
            let cleanup_engine = SnapshotEngine::new(db_path.clone(), config);

            let first_a = engine_a
                .capture(&[make_test_pane(1, 24, 80)], SnapshotTrigger::Manual)
                .await
                .unwrap();
            let first_b = engine_b
                .capture(&[make_test_pane(2, 24, 80)], SnapshotTrigger::Manual)
                .await
                .unwrap();
            let latest_a = engine_a
                .capture(&[make_test_pane(1, 30, 100)], SnapshotTrigger::Manual)
                .await
                .unwrap();
            let latest_b = engine_b
                .capture(&[make_test_pane(2, 30, 100)], SnapshotTrigger::Manual)
                .await
                .unwrap();

            let deleted = cleanup_engine.cleanup().await.unwrap();
            assert_eq!(deleted, 2, "should delete one older checkpoint per session");

            let conn = Connection::open(db_path.as_str()).unwrap();
            let remaining: Vec<(String, i64)> = conn
                .prepare(
                    "SELECT session_id, id
                     FROM session_checkpoints
                     ORDER BY session_id, id",
                )
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();

            assert_eq!(
                remaining.len(),
                2,
                "one checkpoint should remain per session"
            );
            assert_eq!(
                remaining,
                vec![
                    (first_a.session_id.clone(), latest_a.checkpoint_id),
                    (first_b.session_id.clone(), latest_b.checkpoint_id),
                ]
            );
            assert!(
                !remaining.iter().any(|(_, id)| *id == first_a.checkpoint_id),
                "older checkpoint from session A should be pruned"
            );
            assert!(
                !remaining.iter().any(|(_, id)| *id == first_b.checkpoint_id),
                "older checkpoint from session B should be pruned"
            );
        });
    }

    #[test]
    fn close_after_checkpoint_sets_flag() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let r = engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .unwrap();
            engine.close_after_checkpoint(&r).await.unwrap();

            let conn = Connection::open(db_path.as_str()).unwrap();
            let clean: i64 = conn
                .query_row(
                    "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                    [&r.session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(clean, 1);
            assert_eq!(
                engine.capture_lifecycle.load(Ordering::Acquire),
                CAPTURE_LIFECYCLE_SHUTDOWN_COMPLETE
            );

            let checkpoints_before_retry: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let retry_error = engine
                .shutdown_checkpoint(&[make_test_pane(1, 40, 120)], Duration::from_secs(5))
                .await
                .expect_err("a completed shutdown lifecycle cannot capture again");
            assert!(matches!(retry_error, SnapshotError::ShuttingDown));
            let ordinary_capture_error = engine
                .capture(&[make_test_pane(1, 50, 140)], SnapshotTrigger::Manual)
                .await
                .expect_err("ordinary capture remains fenced after clean shutdown");
            assert!(matches!(
                ordinary_capture_error,
                SnapshotError::ShuttingDown
            ));
            let checkpoints_after_retry: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(checkpoints_after_retry, checkpoints_before_retry);
        });
    }

    /// The Cx-first receipt-bound close must set
    /// the shutdown_clean flag identically to the legacy path
    /// when given a fresh, uncancelled cx.
    #[test]
    fn close_after_checkpoint_with_cx_sets_flag() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let r = engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .unwrap();

            let cx = crate::cx::for_testing();
            engine
                .close_after_checkpoint_with_cx(&cx, &r)
                .await
                .unwrap();

            let conn = Connection::open(db_path.as_str()).unwrap();
            let clean: i64 = conn
                .query_row(
                    "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                    [&r.session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(clean, 1);
        });
    }

    /// ft-xbnl0.2.3 Cx-first tick 112: `run_periodic_with_cx`
    /// must exit promptly when the caller's cx is cancelled
    /// mid-flight — not just at the entry checkpoint. This
    /// exercises the deep-threading upgrade from tick 106 (which
    /// only gated entry). With `config.interval_seconds = 30`,
    /// the periodic scheduler would normally block for 30s
    /// between iterations; a mid-flight cx-cancel must cut the
    /// shutdown-watcher `shutdown.changed(&cx)` poll so the task
    /// exits in <500ms.
    #[test]
    fn run_periodic_with_cx_mid_flight_cancel_exits_quickly() {
        // Thread-isolated to prevent TLS interference from 25K+ parallel tests.
        run_async_test_isolated(|| async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                interval_seconds: 30,
                ..SnapshotConfig::default()
            };
            let engine = Arc::new(SnapshotEngine::new(db_path, config));
            let (_shutdown_tx, shutdown_rx) = watch::channel(false);

            let cx = crate::cx::for_testing();

            // Move cx + engine into the task so the task is the sole owner.
            let task_cx = cx.clone();
            let task_engine = Arc::clone(&engine);
            let handle = crate::runtime_async::task::spawn(async move {
                task_engine
                    .run_periodic_with_cx(&task_cx, shutdown_rx, || async {
                        Ok(vec![make_test_pane(1, 24, 80)])
                    })
                    .await
            });

            // Let the scheduler complete its startup capture and settle
            // into the shutdown-watcher poll.
            crate::runtime_async::sleep(Duration::from_millis(100)).await;

            // Cancel the caller's cx mid-flight; the scheduler should
            // notice at its next checkpoint / shutdown.changed(&cx) poll.
            let started = Instant::now();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("mid-flight cancel snapshot scheduler"),
            );

            let task_result = crate::runtime_async::timeout(Duration::from_secs(5), handle)
                .await
                .expect("cancelled scheduler must exit before timeout")
                .expect("snapshot scheduler task must not panic");
            let elapsed = started.elapsed();

            assert!(
                matches!(task_result, Err(SnapshotError::Cancelled)),
                "mid-flight cancellation must remain a typed scheduler result"
            );

            assert!(
                elapsed < Duration::from_secs(5),
                "mid-flight cx-cancel should cut the scheduler within 5s \
                 (cx threading upgrade should avoid the 30s interval wait), took {elapsed:?}"
            );
        });
    }

    /// The Cx-first receipt-bound close must
    /// return `SnapshotError::Cancelled` when given a
    /// pre-cancelled cx, without touching the DB.
    #[test]
    fn close_after_checkpoint_with_precancelled_cx_returns_cancelled() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let receipt = engine
                .capture(&[make_test_pane(1, 24, 80)], SnapshotTrigger::Startup)
                .await
                .expect("capture close receipt");

            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel close_after_checkpoint test"),
            );

            let err = match engine.close_after_checkpoint_with_cx(&cx, &receipt).await {
                Err(e) => e,
                Ok(()) => panic!("receipt close should fail on cancelled cx"),
            };
            assert!(matches!(err, SnapshotError::Cancelled));
        });
    }

    #[test]
    fn snapshot_trigger_db_str() {
        assert_eq!(SnapshotTrigger::Periodic.as_db_str(), "periodic");
        assert_eq!(SnapshotTrigger::Manual.as_db_str(), "event");
        assert_eq!(SnapshotTrigger::Shutdown.as_db_str(), "shutdown");
        assert_eq!(SnapshotTrigger::Startup.as_db_str(), "startup");
        assert_eq!(SnapshotTrigger::Event.as_db_str(), "event");
    }

    #[test]
    fn state_hash_deterministic() {
        let panes = vec![make_test_pane(1, 24, 80)];
        let h1 = compute_state_hash(&panes);
        let h2 = compute_state_hash(&panes);
        assert_eq!(h1, h2);
    }

    #[test]
    fn state_hash_changes_on_different_input() {
        let panes1 = vec![make_test_pane(1, 24, 80)];
        let panes2 = vec![make_test_pane(1, 30, 120)];
        let h1 = compute_state_hash(&panes1);
        let h2 = compute_state_hash(&panes2);
        assert_ne!(h1, h2);
    }

    #[test]
    fn persistence_dedup_tracks_exact_durable_state_but_not_capture_clock() {
        use crate::session_pane_state::{AgentMetadata, ProcessInfo, ScrollbackRef};

        let mut pane = make_test_pane(1, 24, 80);
        pane.workspace = Some("workspace-a".to_string());
        let (topology_at_100, _) = TopologySnapshot::from_panes(&[pane.clone()], 100);
        let state_at_100 = PaneStateSnapshot::from_pane_info(&pane, 100, false);
        let (topology_at_200, _) = TopologySnapshot::from_panes(&[pane.clone()], 200);
        let state_at_200 = PaneStateSnapshot::from_pane_info(&pane, 200, false);
        let baseline = prepare_snapshot_persistence(&topology_at_100, &[state_at_100], None)
            .expect("baseline projection")
            .dedup_hash;
        let clock_only = prepare_snapshot_persistence(
            &topology_at_200,
            std::slice::from_ref(&state_at_200),
            None,
        )
        .expect("clock-shifted projection")
        .dedup_hash;
        assert_eq!(
            baseline, clock_only,
            "capture timestamps alone must not defeat periodic dedup"
        );

        let agent_changed = state_at_200.clone().with_agent(AgentMetadata {
            agent_type: "codex".to_string(),
            session_id: Some("session-a".to_string()),
            state: Some("working".to_string()),
        });
        assert_ne!(
            baseline,
            prepare_snapshot_persistence(&topology_at_200, &[agent_changed], None)
                .expect("agent-enriched projection")
                .dedup_hash,
            "agent metadata changes must produce a checkpoint even when PaneInfo is unchanged"
        );

        let scrollback_changed = state_at_200.clone().with_scrollback(ScrollbackRef {
            output_segments_seq: 42,
            retained_segment_count: 500,
            last_capture_at: 150,
        });
        assert_ne!(
            baseline,
            prepare_snapshot_persistence(
                &topology_at_200,
                std::slice::from_ref(&scrollback_changed),
                None,
            )
            .expect("scrollback-enriched projection")
            .dedup_hash,
            "scrollback authority changes must participate in dedup"
        );
        let retained_count_only = state_at_200.clone().with_scrollback(ScrollbackRef {
            retained_segment_count: 999_999,
            ..scrollback_changed.scrollback_ref.unwrap()
        });
        let same_scrollback_columns = state_at_200.clone().with_scrollback(ScrollbackRef {
            output_segments_seq: 42,
            retained_segment_count: 1,
            last_capture_at: 150,
        });
        assert_eq!(
            prepare_snapshot_persistence(&topology_at_200, &[retained_count_only], None)
                .unwrap()
                .dedup_hash,
            prepare_snapshot_persistence(&topology_at_200, &[same_scrollback_columns], None)
                .unwrap()
                .dedup_hash,
            "non-persisted retained-segment counts must not create checkpoints"
        );

        let process_baseline = state_at_200.clone().with_process(ProcessInfo {
            name: "shell".to_string(),
            pid: Some(1),
            argv: Some(vec!["shell".to_string(), "--first".to_string()]),
        });
        let mut process_ephemera_changed = process_baseline.clone();
        process_ephemera_changed.foreground_process = Some(ProcessInfo {
            name: "shell".to_string(),
            pid: Some(999),
            argv: Some(vec!["shell".to_string(), "--second".to_string()]),
        });
        process_ephemera_changed.shell = Some("/bin/zsh".to_string());
        assert_eq!(
            prepare_snapshot_persistence(&topology_at_200, &[process_baseline], None)
                .unwrap()
                .dedup_hash,
            prepare_snapshot_persistence(&topology_at_200, &[process_ephemera_changed], None,)
                .unwrap()
                .dedup_hash,
            "PID, argv, and shell are not persisted and must not affect dedup"
        );

        pane.workspace = Some("workspace-b".to_string());
        let (workspace_changed, _) = TopologySnapshot::from_panes(&[pane], 200);
        assert_ne!(
            baseline,
            prepare_snapshot_persistence(
                &workspace_changed,
                std::slice::from_ref(&state_at_200),
                None,
            )
            .expect("workspace-changed projection")
            .dedup_hash,
            "persisted topology fields must participate"
        );
    }

    #[test]
    fn persistence_state_hash_canonicalizes_environment_map_order() {
        use crate::session_pane_state::CapturedEnv;

        let pane = make_test_pane(1, 24, 80);
        let (topology, _) = TopologySnapshot::from_panes(std::slice::from_ref(&pane), 100);
        let mut first = PaneStateSnapshot::from_pane_info(&pane, 100, false);
        let mut second = first.clone();
        let mut first_vars = HashMap::new();
        first_vars.insert("LANG".to_string(), "en_US.UTF-8".to_string());
        first_vars.insert("TERM".to_string(), "xterm-256color".to_string());
        let mut second_vars = HashMap::new();
        second_vars.insert("TERM".to_string(), "xterm-256color".to_string());
        second_vars.insert("LANG".to_string(), "en_US.UTF-8".to_string());
        first.env = Some(CapturedEnv {
            vars: first_vars,
            redacted_count: 0,
        });
        second.env = Some(CapturedEnv {
            vars: second_vars,
            redacted_count: 0,
        });

        assert_eq!(
            prepare_snapshot_persistence(&topology, &[first], None)
                .expect("first env projection")
                .dedup_hash,
            prepare_snapshot_persistence(&topology, &[second], None)
                .expect("second env projection")
                .dedup_hash,
            "map insertion order must not create redundant checkpoints"
        );
    }

    #[test]
    fn generate_session_id_format() {
        let id = generate_session_id();
        assert!(id.starts_with("sess-"));
        assert!(id.len() > 20);
    }

    #[test]
    fn host_identity_is_optional_trimmed_and_utf8_bounded() {
        assert_eq!(bounded_host_id(None), None);
        assert_eq!(bounded_host_id(Some(" \t\n ".to_string())), None);
        assert_eq!(
            bounded_host_id(Some("  trj.example  ".to_string())).as_deref(),
            Some("trj.example")
        );

        let bounded = bounded_host_id(Some("M🦀".repeat(SNAPSHOT_HOST_ID_INPUT_BYTES)))
            .expect("non-empty host identity should remain present");
        assert!(bounded.len() <= SNAPSHOT_HOST_ID_INPUT_BYTES);
        assert!(
            bounded.ends_with('…'),
            "oversized host identity must carry explicit truncation evidence"
        );
    }

    // =========================================================================
    // Intelligent scheduling tests
    // =========================================================================

    fn intelligent_config(threshold: f64) -> SnapshotConfig {
        SnapshotConfig {
            scheduling: crate::config::SnapshotSchedulingConfig {
                mode: crate::config::SnapshotSchedulingMode::Intelligent,
                snapshot_threshold: threshold,
                work_completed_value: 2.0,
                state_transition_value: 1.0,
                idle_window_value: 3.0,
                memory_pressure_value: 4.0,
                hazard_trigger_value: 10.0,
                periodic_fallback_minutes: 60,
            },
            ..SnapshotConfig::default()
        }
    }

    #[test]
    fn trigger_value_mapping() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, intelligent_config(5.0));

        assert!((engine.trigger_value(SnapshotTrigger::WorkCompleted) - 2.0).abs() < f64::EPSILON);
        assert!(
            (engine.trigger_value(SnapshotTrigger::StateTransition) - 1.0).abs() < f64::EPSILON
        );
        assert!((engine.trigger_value(SnapshotTrigger::IdleWindow) - 3.0).abs() < f64::EPSILON);
        assert!((engine.trigger_value(SnapshotTrigger::MemoryPressure) - 4.0).abs() < f64::EPSILON);
        assert!(
            (engine.trigger_value(SnapshotTrigger::HazardThreshold) - 10.0).abs() < f64::EPSILON
        );
        assert!((engine.trigger_value(SnapshotTrigger::Event) - 2.0).abs() < f64::EPSILON);
        assert!((engine.trigger_value(SnapshotTrigger::Periodic)).abs() < f64::EPSILON);
        assert!((engine.trigger_value(SnapshotTrigger::PeriodicFallback)).abs() < f64::EPSILON);
        assert!((engine.trigger_value(SnapshotTrigger::Manual)).abs() < f64::EPSILON);
        assert!((engine.trigger_value(SnapshotTrigger::Shutdown)).abs() < f64::EPSILON);
        assert!((engine.trigger_value(SnapshotTrigger::Startup)).abs() < f64::EPSILON);
    }

    #[test]
    fn immediate_trigger_classification() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, intelligent_config(5.0));

        assert!(engine.is_immediate_trigger(SnapshotTrigger::HazardThreshold));
        assert!(engine.is_immediate_trigger(SnapshotTrigger::MemoryPressure));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::WorkCompleted));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::StateTransition));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::IdleWindow));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::Periodic));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::Manual));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::Shutdown));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::Startup));
        assert!(!engine.is_immediate_trigger(SnapshotTrigger::Event));
    }

    #[test]
    fn deferred_trigger_priority_preserves_due_nonperiodic_and_immediate_work() {
        assert!(!should_upgrade_pending_scheduler_trigger(
            SnapshotTrigger::PeriodicFallback,
            SnapshotTrigger::WorkCompleted,
            false,
        ));
        assert!(should_upgrade_pending_scheduler_trigger(
            SnapshotTrigger::PeriodicFallback,
            SnapshotTrigger::WorkCompleted,
            true,
        ));
        assert!(should_upgrade_pending_scheduler_trigger(
            SnapshotTrigger::WorkCompleted,
            SnapshotTrigger::HazardThreshold,
            true,
        ));
        assert!(!should_upgrade_pending_scheduler_trigger(
            SnapshotTrigger::HazardThreshold,
            SnapshotTrigger::WorkCompleted,
            true,
        ));
        assert_eq!(
            intelligent_scheduler_poll_wait(
                Duration::ZERO,
                Some(SCHEDULER_URGENT_CAPTURE_RETRY_DELAY),
            ),
            SCHEDULER_URGENT_CAPTURE_RETRY_DELAY,
            "an expired fallback must not hot-loop ahead of a pending capture retry"
        );
    }

    #[test]
    fn emit_trigger_sends_to_channel() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path, intelligent_config(5.0));

            assert!(engine.emit_trigger(SnapshotTrigger::WorkCompleted));
            assert!(engine.emit_trigger(SnapshotTrigger::StateTransition));

            let mut rx = engine
                .trigger_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .unwrap();
            assert_eq!(recv_trigger(&mut rx).await, SnapshotTrigger::WorkCompleted);
            assert_eq!(
                recv_trigger(&mut rx).await,
                SnapshotTrigger::StateTransition
            );
        });
    }

    fn checkpoint_count(db_path: &str) -> i64 {
        let conn = Connection::open(db_path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    /// Wait until the scheduler has written `expected` checkpoints (ft-83kc7).
    ///
    /// The scheduler tests used to sleep a fixed 100 ms and assert immediately.
    /// That is the sleep-and-hope pattern this project rejects by design
    /// ("Event-Driven, Not Time-Based"), and it was only ever passing by
    /// accident: while the intelligent scheduler could not observe shutdown,
    /// each of these tests wedged at its final `handle.await` and stopped
    /// competing for CPU. With the hang fixed they all run concurrently, doing
    /// real SQLite work on a shared worker, and 100 ms stopped being enough —
    /// four of them failed their *startup* assertion.
    ///
    /// Polling for the condition removes the guess. It fails with the same
    /// message shape as the assertion it replaces, so a genuine "capture did not
    /// happen" bug still reports clearly rather than timing out silently.
    async fn await_checkpoint_count(db_path: &str, expected: i64, label: &str) {
        const WAIT_BUDGET: Duration = Duration::from_secs(10);
        const POLL_STEP: Duration = Duration::from_millis(20);

        let deadline = Instant::now() + WAIT_BUDGET;
        let mut observed = checkpoint_count(db_path);
        while observed < expected && Instant::now() < deadline {
            sleep(POLL_STEP).await;
            observed = checkpoint_count(db_path);
        }

        assert_eq!(
            observed, expected,
            "{label}: expected {expected} checkpoint(s) within {WAIT_BUDGET:?}, saw {observed}"
        );
    }

    fn counting_pane_provider() -> impl Fn() -> TestPaneProviderFuture + Send + Sync + 'static {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok(vec![make_test_pane(n as u64, 24 + n, 80)]) })
        }
    }

    #[test]
    fn intelligent_accumulates_below_threshold() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            // ft-83kc7: wait for the startup capture rather than guessing at it.
            await_checkpoint_count(db_path.as_str(), 1, "startup capture").await;

            // Sum = 4.0 < threshold(5.0)
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted); // +2.0
            sleep(Duration::from_millis(50)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::StateTransition); // +1.0
            sleep(Duration::from_millis(50)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::StateTransition); // +1.0 = 4.0
            sleep(Duration::from_millis(100)).await;

            let after_below = checkpoint_count(db_path.as_str());
            assert_eq!(after_below, 1, "below threshold: no new capture");

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_captures_at_threshold() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            // 3 x WorkCompleted = 6.0 >= 5.0
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(30)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(30)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(200)).await;

            let count = checkpoint_count(db_path.as_str());
            assert_eq!(count, 2, "startup + threshold capture");

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_immediate_bypasses_threshold() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(100.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            let _ = engine.emit_trigger(SnapshotTrigger::HazardThreshold);
            sleep(Duration::from_millis(200)).await;

            let count = checkpoint_count(db_path.as_str());
            assert_eq!(count, 2, "startup + immediate HazardThreshold");

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_memory_pressure_immediate() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(100.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            let _ = engine.emit_trigger(SnapshotTrigger::MemoryPressure);
            sleep(Duration::from_millis(200)).await;

            let count = checkpoint_count(db_path.as_str());
            assert_eq!(count, 2, "startup + immediate MemoryPressure");

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_value_resets_after_capture() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            // First batch: 3 x 2.0 = 6.0 >= 5.0 → capture + reset
            for _ in 0..3 {
                let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
                sleep(Duration::from_millis(30)).await;
            }
            sleep(Duration::from_millis(150)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "startup + first threshold"
            );

            // Second batch: 2 x 2.0 = 4.0 < 5.0 (reset happened)
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(30)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(150)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "still 2: 4.0 < 5.0 after reset"
            );

            // Third trigger crosses again: 4.0 + 2.0 = 6.0
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(200)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                3,
                "startup + 2 threshold captures"
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_shutdown_stops_loop() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;
            shutdown_tx.send(true).unwrap();

            let result = timeout(Duration::from_secs(5), handle).await;
            assert!(result.is_ok(), "run_periodic exits on shutdown");
        });
    }

    #[test]
    fn intelligent_zero_threshold_captures_every_trigger() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(0.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            // Wait for startup capture + scheduler loop to settle (250ms poll step)
            sleep(Duration::from_millis(350)).await;

            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(350)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::StateTransition);
            sleep(Duration::from_millis(350)).await;

            let count = checkpoint_count(db_path.as_str());
            assert_eq!(count, 3, "startup + 2 captures (zero threshold)");

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn periodic_mode_ignores_triggers() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                interval_seconds: 3600,
                scheduling: crate::config::SnapshotSchedulingConfig {
                    mode: crate::config::SnapshotSchedulingMode::Periodic,
                    ..Default::default()
                },
                ..SnapshotConfig::default()
            };
            let engine = Arc::new(SnapshotEngine::new(db_path.clone(), config));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            let _ = engine.emit_trigger(SnapshotTrigger::HazardThreshold);
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(200)).await;

            let count = checkpoint_count(db_path.as_str());
            assert_eq!(count, 1, "periodic mode: only startup");

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn emit_trigger_returns_false_when_full() {
        let (_tmp, db_path) = setup_test_db();
        let (trigger_tx, _trigger_rx) = mpsc::channel::<SnapshotTrigger>(2);
        let engine = SnapshotEngine {
            db_path,
            config: intelligent_config(5.0),
            session_id: RwLock::new(None),
            owner_identity: None,
            last_dedup_hash: RwLock::new(None),
            capture_lifecycle: AtomicU8::new(CAPTURE_LIFECYCLE_OPEN_IDLE),
            scheduler_in_progress: AtomicBool::new(false),
            session_cleanup_in_progress: AtomicBool::new(false),
            snapshot_authority: Arc::new(SnapshotAuthorityState::new(None)),
            trigger_tx,
            trigger_rx: StdMutex::new(None),
            telemetry: SnapshotEngineTelemetry::new(),
            auxiliary_projection_read_attempts: AtomicU64::new(0),
        };

        assert!(engine.emit_trigger(SnapshotTrigger::WorkCompleted));
        assert!(engine.emit_trigger(SnapshotTrigger::WorkCompleted));
        assert!(
            !engine.emit_trigger(SnapshotTrigger::WorkCompleted),
            "channel full: returns false"
        );
    }

    // =========================================================================
    // Additional intelligent scheduling tests (wa-w9rd)
    // =========================================================================

    #[test]
    fn intelligent_mixed_accumulate_then_immediate_resets() {
        run_async_test(async {
            // Accumulate below threshold, then an immediate trigger should
            // capture AND reset the accumulator, so subsequent triggers
            // need to re-accumulate from zero.
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            // ft-83kc7: wait for the startup capture rather than guessing at it.
            await_checkpoint_count(db_path.as_str(), 1, "startup capture").await;

            // Accumulate 2.0 < 5.0 threshold
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted); // +2.0
            sleep(Duration::from_millis(50)).await;

            // HazardThreshold is immediate — captures + resets accumulator
            let _ = engine.emit_trigger(SnapshotTrigger::HazardThreshold);
            sleep(Duration::from_millis(200)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "startup + immediate (hazard)"
            );

            // After reset: 2 x WorkCompleted = 4.0 < 5.0 — no capture
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted); // +2.0
            sleep(Duration::from_millis(30)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted); // +2.0 = 4.0
            sleep(Duration::from_millis(200)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "4.0 < 5.0 after reset — still 2"
            );

            // One more pushes over: 4.0 + 2.0 = 6.0 >= 5.0
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(200)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                3,
                "startup + hazard + threshold"
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_non_accumulating_triggers_dont_capture() {
        run_async_test(async {
            // Manual, Startup, Periodic, PeriodicFallback, Shutdown all have
            // trigger_value = 0.0 and are not immediate. Sending them through
            // the channel should not cause any captures (beyond startup).
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            // ft-83kc7: wait for the startup capture rather than guessing at it.
            await_checkpoint_count(db_path.as_str(), 1, "startup only").await;

            // Send non-accumulating triggers
            let _ = engine.emit_trigger(SnapshotTrigger::Manual);
            sleep(Duration::from_millis(30)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::Periodic);
            sleep(Duration::from_millis(30)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::PeriodicFallback);
            sleep(Duration::from_millis(30)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::Startup);
            sleep(Duration::from_millis(200)).await;

            assert_eq!(
                checkpoint_count(db_path.as_str()),
                1,
                "no captures from zero-value triggers"
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    /// ft-83kc7: the intelligent scheduler must exit when shutdown is signalled.
    ///
    /// It did not. The shutdown check was a `Duration::ZERO` timeout wrapped
    /// around `shutdown.changed()`, which returns `Elapsed` without polling the
    /// inner future once the ambient clock has passed the (immediately-expired)
    /// deadline — so the signal was never observed and the loop ran forever.
    /// Every test that spawns this scheduler and awaits its handle hung, which
    /// is what made an unfiltered `--lib` run impossible once DB-backed tests
    /// started working at all.
    ///
    /// The assertion is wrapped in a timeout so a regression FAILS this test
    /// instead of wedging the whole suite the way the original defect did.
    #[test]
    fn ft_83kc7_intelligent_scheduler_exits_on_shutdown() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            // Let the scheduler reach its loop (startup capture + first
            // session-cleanup pass) before signalling.
            sleep(Duration::from_millis(150)).await;
            shutdown_tx.send(true).expect("shutdown send");

            // The loop re-checks the flag once per trigger wait-step (250 ms),
            // so a few seconds is generous while still bounding a regression.
            timeout(Duration::from_secs(10), handle)
                .await
                .expect("intelligent scheduler must observe shutdown and exit")
                .expect("scheduler task must not panic");
        });
    }

    #[test]
    fn intelligent_exact_threshold_boundary() {
        run_async_test(async {
            // threshold = 5.0, send WorkCompleted(2.0) + IdleWindow(3.0) = exactly 5.0
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            // ft-83kc7: wait for the startup capture rather than guessing at it.
            await_checkpoint_count(db_path.as_str(), 1, "startup").await;

            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted); // +2.0
            sleep(Duration::from_millis(50)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::IdleWindow); // +3.0 = 5.0
            sleep(Duration::from_millis(200)).await;

            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "exactly at threshold captures"
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_idle_window_accumulation() {
        run_async_test(async {
            // IdleWindow has value 3.0; two IdleWindows = 6.0 >= threshold(5.0)
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            let _ = engine.emit_trigger(SnapshotTrigger::IdleWindow); // +3.0
            sleep(Duration::from_millis(50)).await;
            assert_eq!(checkpoint_count(db_path.as_str()), 1, "3.0 < 5.0");

            let _ = engine.emit_trigger(SnapshotTrigger::IdleWindow); // +3.0 = 6.0
            sleep(Duration::from_millis(200)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "6.0 >= 5.0 with IdleWindow"
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_multiple_immediate_triggers() {
        run_async_test(async {
            // Two consecutive immediate triggers should each cause a capture.
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(100.0), // high threshold so only immediates capture
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            // ft-83kc7: wait for the startup capture rather than guessing at it.
            await_checkpoint_count(db_path.as_str(), 1, "startup").await;

            let _ = engine.emit_trigger(SnapshotTrigger::HazardThreshold);
            sleep(Duration::from_millis(100)).await;
            let _ = engine.emit_trigger(SnapshotTrigger::MemoryPressure);
            sleep(Duration::from_millis(200)).await;

            assert_eq!(
                checkpoint_count(db_path.as_str()),
                3,
                "startup + hazard + memory_pressure"
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_scheduler_rejects_concurrency_and_restores_receiver_for_restart() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            // First call takes the receiver
            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });
            await_checkpoint_count(db_path.as_str(), 1, "first scheduler startup").await;

            let (_shutdown_tx2, shutdown_rx2) = watch::channel(false);
            let concurrent = engine
                .run_periodic(shutdown_rx2, counting_pane_provider())
                .await;
            assert!(matches!(
                concurrent,
                Err(SnapshotError::SchedulerInProgress)
            ));

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();

            let (restart_shutdown_tx, restart_shutdown_rx) = watch::channel(false);
            let restarted_engine = Arc::clone(&engine);
            let restarted = crate::runtime_async::task::spawn(async move {
                restarted_engine
                    .run_periodic(restart_shutdown_rx, counting_pane_provider())
                    .await
                    .expect("restarted snapshot scheduler");
            });
            await_checkpoint_count(db_path.as_str(), 2, "restarted scheduler startup").await;
            assert!(engine.emit_trigger(SnapshotTrigger::HazardThreshold));
            await_checkpoint_count(db_path.as_str(), 3, "restarted scheduler trigger").await;
            restart_shutdown_tx.send(true).unwrap();
            restarted.await.unwrap();
        });
    }

    #[test]
    fn intelligent_custom_config_values() {
        run_async_test(async {
            // Test with non-default config: high work_completed_value so one trigger
            // crosses the threshold.
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                scheduling: crate::config::SnapshotSchedulingConfig {
                    mode: crate::config::SnapshotSchedulingMode::Intelligent,
                    snapshot_threshold: 5.0,
                    work_completed_value: 6.0, // one trigger crosses threshold
                    state_transition_value: 0.5,
                    idle_window_value: 0.5,
                    memory_pressure_value: 4.0,
                    hazard_trigger_value: 10.0,
                    periodic_fallback_minutes: 60,
                },
                ..SnapshotConfig::default()
            };
            let engine = Arc::new(SnapshotEngine::new(db_path.clone(), config));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            // One WorkCompleted = 6.0 >= 5.0
            let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            sleep(Duration::from_millis(200)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "single trigger crosses custom threshold"
            );

            // StateTransition = 0.5 < 5.0 — no additional capture
            let _ = engine.emit_trigger(SnapshotTrigger::StateTransition);
            sleep(Duration::from_millis(200)).await;
            assert_eq!(
                checkpoint_count(db_path.as_str()),
                2,
                "0.5 < 5.0 no capture"
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn intelligent_rapid_burst_all_processed() {
        run_async_test(async {
            // Send a burst of triggers rapidly — all should be processed.
            let (_tmp, db_path) = setup_test_db();
            let engine = Arc::new(SnapshotEngine::new(
                db_path.clone(),
                intelligent_config(5.0),
            ));
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let e2 = engine.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                e2.run_periodic(shutdown_rx, counting_pane_provider())
                    .await
                    .expect("snapshot scheduler");
            });

            sleep(Duration::from_millis(100)).await;

            // Fire 5 WorkCompleted triggers (5 x 2.0 = 10.0) as fast as possible
            for _ in 0..5 {
                let _ = engine.emit_trigger(SnapshotTrigger::WorkCompleted);
            }
            sleep(Duration::from_millis(300)).await;

            // Should have captured at least twice (startup + when >= 5.0)
            let count = checkpoint_count(db_path.as_str());
            assert!(
                count >= 2,
                "burst should produce at least 2 captures, got {}",
                count
            );

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    fn snapshot_trigger_serde_roundtrip() {
        let triggers = vec![
            SnapshotTrigger::Periodic,
            SnapshotTrigger::PeriodicFallback,
            SnapshotTrigger::Manual,
            SnapshotTrigger::Shutdown,
            SnapshotTrigger::Startup,
            SnapshotTrigger::Event,
            SnapshotTrigger::WorkCompleted,
            SnapshotTrigger::HazardThreshold,
            SnapshotTrigger::StateTransition,
            SnapshotTrigger::IdleWindow,
            SnapshotTrigger::MemoryPressure,
        ];
        for trigger in triggers {
            let json = serde_json::to_string(&trigger).unwrap();
            let back: SnapshotTrigger = serde_json::from_str(&json).unwrap();
            assert_eq!(trigger, back);
        }
    }

    #[test]
    fn snapshot_trigger_db_str_all_variants() {
        // Verify all 11 trigger variants have valid db strings.
        let mapping = vec![
            (SnapshotTrigger::Periodic, "periodic"),
            (SnapshotTrigger::PeriodicFallback, "periodic"),
            (SnapshotTrigger::Manual, "event"),
            (SnapshotTrigger::Shutdown, "shutdown"),
            (SnapshotTrigger::Startup, "startup"),
            (SnapshotTrigger::Event, "event"),
            (SnapshotTrigger::WorkCompleted, "event"),
            (SnapshotTrigger::HazardThreshold, "event"),
            (SnapshotTrigger::StateTransition, "event"),
            (SnapshotTrigger::IdleWindow, "event"),
            (SnapshotTrigger::MemoryPressure, "event"),
        ];
        for (trigger, expected) in mapping {
            assert_eq!(
                trigger.as_db_str(),
                expected,
                "db_str mismatch for {:?}",
                trigger
            );
        }
    }

    #[test]
    fn trigger_value_non_accumulating_all_zero() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, intelligent_config(5.0));

        let zero_triggers = vec![
            SnapshotTrigger::Periodic,
            SnapshotTrigger::PeriodicFallback,
            SnapshotTrigger::Manual,
            SnapshotTrigger::Shutdown,
            SnapshotTrigger::Startup,
        ];
        for trigger in zero_triggers {
            assert!(
                engine.trigger_value(trigger).abs() < f64::EPSILON,
                "{:?} should have zero value",
                trigger
            );
        }
    }

    #[test]
    fn trigger_value_accumulating_all_positive() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path, intelligent_config(5.0));

        let positive_triggers = vec![
            SnapshotTrigger::WorkCompleted,
            SnapshotTrigger::StateTransition,
            SnapshotTrigger::IdleWindow,
            SnapshotTrigger::MemoryPressure,
            SnapshotTrigger::HazardThreshold,
            SnapshotTrigger::Event,
        ];
        for trigger in positive_triggers {
            assert!(
                engine.trigger_value(trigger) > 0.0,
                "{:?} should have positive value",
                trigger
            );
        }
    }

    // =========================================================================
    // Batch 13 — PearlSpring wa-1u90p.7.1 utility function & edge-case tests
    // =========================================================================

    #[test]
    fn agent_type_from_db_known_types() {
        assert!(matches!(agent_type_from_db("codex"), AgentType::Codex));
        assert!(matches!(
            agent_type_from_db("claude_code"),
            AgentType::ClaudeCode
        ));
        assert!(matches!(agent_type_from_db("gemini"), AgentType::Gemini));
        assert!(matches!(agent_type_from_db("wezterm"), AgentType::Wezterm));
    }

    #[test]
    fn agent_type_from_db_unknown_fallback() {
        assert!(matches!(agent_type_from_db(""), AgentType::Unknown));
        assert!(matches!(
            agent_type_from_db("something_else"),
            AgentType::Unknown
        ));
        assert!(matches!(agent_type_from_db("CODEX"), AgentType::Unknown)); // case-sensitive
    }

    #[test]
    fn is_missing_events_table_detects_error() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::Unknown,
                extended_code: 1,
            },
            Some("no such table: events".to_string()),
        );
        assert!(is_missing_events_table(&err));
    }

    #[test]
    fn is_missing_events_table_other_error() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::Unknown,
                extended_code: 1,
            },
            Some("syntax error".to_string()),
        );
        assert!(!is_missing_events_table(&err));
    }

    #[test]
    fn compute_state_hash_empty_panes() {
        let h = compute_state_hash(&[]);
        assert!(h.starts_with(crate::checkpoint_witness::SNAPSHOT_DEDUP_PREFIX));
        assert_eq!(h.len(), 70); // `snpd2:` plus 64 lowercase SHA-256 hex chars
    }

    #[test]
    fn compute_state_hash_order_independent_for_ids() {
        // Persistence rows are a pane-ID-keyed set, so input enumeration order
        // cannot create a new witness.
        let p1 = make_test_pane(1, 24, 80);
        let p2 = make_test_pane(2, 30, 120);

        let h1 = compute_state_hash(&[p1.clone(), p2.clone()]);
        let h2 = compute_state_hash(&[p2, p1]);
        assert_eq!(h1, h2, "pane enumeration order must not affect the hash");
    }

    #[test]
    fn compute_state_hash_differs_for_different_pane_count() {
        let p1 = make_test_pane(1, 24, 80);
        let p2 = make_test_pane(2, 30, 120);

        let h1 = compute_state_hash(std::slice::from_ref(&p1));
        let h2 = compute_state_hash(&[p1, p2]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn epoch_ms_returns_reasonable_value() {
        let ms = epoch_ms();
        // Should be after 2020-01-01 (1577836800000) and before 2100-01-01
        assert!(ms > 1_577_836_800_000);
        assert!(ms < 4_102_444_800_000);
    }

    #[test]
    fn generate_session_id_unique() {
        let id1 = generate_session_id();
        let id2 = generate_session_id();
        assert_ne!(id1, id2, "session IDs should be unique");
        assert!(id1.starts_with("sess-"));
        assert!(id2.starts_with("sess-"));
    }

    #[test]
    fn snapshot_error_display_messages() {
        assert_eq!(
            SnapshotError::InProgress.to_string(),
            "snapshot already in progress"
        );
        assert_eq!(
            SnapshotError::SchedulerInProgress.to_string(),
            "snapshot scheduler already running for this engine"
        );
        assert_eq!(
            SnapshotError::TriggerReceiverUnavailable.to_string(),
            "snapshot intelligent scheduler trigger receiver is unavailable"
        );
        assert_eq!(SnapshotError::NoPanes.to_string(), "no panes found");
        assert_eq!(
            SnapshotError::NoChanges.to_string(),
            "no changes since last snapshot"
        );
        assert_eq!(
            SnapshotError::PaneList("CONTENT_FREE_PANE_SOURCE".into()).to_string(),
            "pane listing failed"
        );
        assert_eq!(
            SnapshotError::Database("CONTENT_FREE_DATABASE_SOURCE".into()).to_string(),
            "snapshot database operation failed"
        );
        assert_eq!(
            SnapshotError::Serialization("CONTENT_FREE_SERIALIZATION_SOURCE".into()).to_string(),
            "snapshot serialization failed"
        );
        assert_eq!(
            SnapshotError::IndeterminateAuthorityMutation {
                operation: SnapshotAuthorityOperation::CheckpointCommit,
            }
            .to_string(),
            concat!(
                "snapshot authority outcome is indeterminate after checkpoint_commit handoff; ",
                "reconcile durable state before retrying"
            )
        );
        assert_eq!(
            SnapshotError::AuthorityReconciliationRequired {
                operation: SnapshotAuthorityOperation::ShutdownMark,
                first_indeterminate_operation: Some(SnapshotAuthorityOperation::CheckpointCommit,),
            }
            .to_string(),
            concat!(
                "snapshot authority reconciliation is required before shutdown_mark; first ",
                "indeterminate operation: Some(CheckpointCommit); durable mutation suppressed"
            )
        );
        assert_eq!(
            SnapshotError::AuthorityMutationInProgress {
                operation: SnapshotAuthorityOperation::SessionRetentionCleanup,
            }
            .to_string(),
            "snapshot authority mutation already in progress during session_retention_cleanup"
        );
    }

    #[test]
    fn empty_shutdown_creates_and_closes_a_terminal_session() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

            let result = engine
                .shutdown_checkpoint(&[], Duration::from_secs(5))
                .await
                .expect("empty terminal observation must be persisted");
            assert_eq!(result.pane_count, 0);
        });
    }

    #[test]
    fn shutdown_checkpoint_captures_and_marks() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            // First capture to establish session
            engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .unwrap();

            // Shutdown always persists a final receipt, even when the ordinary
            // periodic state projection would otherwise deduplicate.
            let panes2 = vec![make_test_pane(1, 30, 100)];
            let snap = engine
                .shutdown_checkpoint(&panes2, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(snap.trigger, SnapshotTrigger::Shutdown);

            // Verify shutdown flag is set
            let conn = Connection::open(db_path.as_str()).unwrap();
            let clean: i64 = conn
                .query_row(
                    "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                    [&snap.session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(clean, 1);
        });
    }

    /// ft-xbnl0.2.x Cx-first: `shutdown_checkpoint_with_cx` must short-circuit
    /// when the caller's Cx is already cancelled on entry. Neither mutation is
    /// attempted and the result is `SnapshotError::Cancelled`; marking a
    /// session clean without an authoritative final checkpoint would suppress
    /// legitimate recovery after output raced the abandoned shutdown.
    #[test]
    fn shutdown_checkpoint_with_cx_pre_cancelled_leaves_session_unclean_without_capture() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            // Establish a session first so there's something to mark shutdown on.
            engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .expect("startup capture");
            let captures_before = engine.telemetry().snapshot().captures_attempted;

            // Pre-cancel the Cx. Use a different pane state to make it explicit
            // that cancellation, not state equivalence, suppresses the write.
            let panes2 = vec![make_test_pane(1, 30, 100)];
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("ft-xbnl0.2.x pre-cancel shutdown"),
            );

            let wall_start = std::time::Instant::now();
            let error = engine
                .shutdown_checkpoint_with_cx(&cx, &panes2, Duration::from_secs(5))
                .await
                .expect_err("pre-cancelled shutdown must report cancellation");

            // Must not have triggered a capture.
            assert!(
                matches!(error, SnapshotError::Cancelled),
                "pre-cancelled shutdown must preserve typed cancellation"
            );
            assert_eq!(
                engine.telemetry().snapshot().captures_attempted,
                captures_before,
                "pre-cancelled shutdown must not record a capture attempt"
            );
            assert!(
                wall_start.elapsed() < Duration::from_secs(1),
                "pre-cancelled shutdown must not consume the timeout budget; took {:?}",
                wall_start.elapsed()
            );

            // Without a final checkpoint receipt, the session must remain
            // unclean so restart recovery is never suppressed on a guess.
            let conn = Connection::open(db_path.as_str()).unwrap();
            let clean: i64 = conn
                .query_row(
                    "SELECT shutdown_clean FROM mux_sessions ORDER BY rowid DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                clean, 0,
                "pre-cancelled shutdown must leave the session recoverably unclean"
            );
        });
    }

    /// ft-xbnl0.2.x Cx-first: happy path — `shutdown_checkpoint_with_cx`
    /// with a fresh Cx behaves identically to `shutdown_checkpoint`, i.e.
    /// captures a Shutdown-triggered snapshot and marks the session
    /// clean. Pins the contract that the Cx entry point doesn't regress
    /// the existing operational behavior.
    #[test]
    fn shutdown_checkpoint_with_cx_happy_path_captures_and_marks() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .expect("startup capture");

            // Fresh, non-cancelled Cx.
            let cx = crate::cx::for_request();
            let panes2 = vec![make_test_pane(1, 30, 100)];
            let snap = engine
                .shutdown_checkpoint_with_cx(&cx, &panes2, Duration::from_secs(5))
                .await
                .expect("shutdown capture");
            assert_eq!(snap.trigger, SnapshotTrigger::Shutdown);

            let conn = Connection::open(db_path.as_str()).unwrap();
            let clean: i64 = conn
                .query_row(
                    "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                    [&snap.session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(clean, 1);
        });
    }

    /// ft-xbnl0.2.x Cx-first: `cleanup_with_cx` with a pre-cancelled Cx
    /// must NOT delete any checkpoints and must return `SnapshotError::Cancelled`. The
    /// `cleanup_runs` counter MUST still increment so observability
    /// stays honest about how many cleanup *attempts* the engine saw.
    #[test]
    fn cleanup_with_cx_pre_cancelled_skips_db_work() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                retention_count: 2,
                retention_days: 365,
                ..SnapshotConfig::default()
            };
            let engine = SnapshotEngine::new(db_path.clone(), config);

            // Plant 4 snapshots so a non-cancelled cleanup would remove 2.
            for i in 0..4u64 {
                let panes = vec![make_test_pane(i, 24 + i as u32, 80)];
                engine
                    .capture(&panes, SnapshotTrigger::Manual)
                    .await
                    .expect("capture");
            }

            let runs_before = engine.telemetry().snapshot().cleanup_runs;
            let removed_before = engine.telemetry().snapshot().cleanup_removed;

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("ft-xbnl0.2.x snapshot cleanup precancel"),
            );

            let error = engine
                .cleanup_with_cx(&cx)
                .await
                .expect_err("pre-cancelled cleanup must report cancellation");
            assert!(matches!(error, SnapshotError::Cancelled));

            // 4 checkpoints must still be in the DB.
            let conn = Connection::open(db_path.as_str()).unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                count, 4,
                "pre-cancelled cleanup must not delete any checkpoints"
            );

            // Observability parity: runs incremented, removed NOT changed.
            assert_eq!(
                engine.telemetry().snapshot().cleanup_runs,
                runs_before + 1,
                "cleanup_runs must still increment on pre-cancelled attempts \
                 so operators can see how many cleanups the engine tried"
            );
            assert_eq!(
                engine.telemetry().snapshot().cleanup_removed,
                removed_before,
                "cleanup_removed must NOT change when the DB work was skipped"
            );
        });
    }

    /// ft-xbnl0.2.x Cx-first: happy path — `cleanup_with_cx` with a
    /// fresh Cx behaves identically to `cleanup`: removes the retention
    /// overflow and increments both counters. Pins no-regression on the
    /// success path.
    #[test]
    fn cleanup_with_cx_happy_path_removes_overflow() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                retention_count: 2,
                retention_days: 365,
                ..SnapshotConfig::default()
            };
            let engine = SnapshotEngine::new(db_path.clone(), config);

            for i in 0..4u64 {
                let panes = vec![make_test_pane(i, 24 + i as u32, 80)];
                engine
                    .capture(&panes, SnapshotTrigger::Manual)
                    .await
                    .expect("capture");
            }

            let cx = crate::cx::for_request();
            let deleted = engine
                .cleanup_with_cx(&cx)
                .await
                .expect("happy-path cleanup");
            assert_eq!(deleted, 2, "should remove 2 overflow checkpoints");

            let conn = Connection::open(db_path.as_str()).unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 2, "retention_count=2 should leave 2 checkpoints");

            assert!(
                engine.telemetry().snapshot().cleanup_removed >= 2,
                "cleanup_removed must reflect the actual deletion"
            );
        });
    }

    /// ft-xbnl0.2.3 Cx-first: capture_with_cx with a pre-cancelled Cx
    /// must surface `SnapshotError::Cancelled` without entering the
    /// in-progress guard or touching panes/storage. The
    /// `captures_attempted` counter still increments (parity with the
    /// legacy observability surface).
    #[test]
    fn capture_with_cx_pre_cancelled_returns_cancelled() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let attempts_before = engine.telemetry().snapshot().captures_attempted;
            let successes_before = engine.telemetry().snapshot().captures_succeeded;

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("ft-xbnl0.2.3 capture_with_cx precancel"),
            );

            let result = engine
                .capture_with_cx(&cx, &panes, SnapshotTrigger::Manual)
                .await;

            assert!(
                matches!(result, Err(SnapshotError::Cancelled)),
                "pre-cancelled capture_with_cx must surface Cancelled, got: {:?}",
                result
            );

            // Observability parity
            assert_eq!(
                engine.telemetry().snapshot().captures_attempted,
                attempts_before + 1,
                "captures_attempted must still increment on pre-cancelled attempts"
            );
            assert_eq!(
                engine.telemetry().snapshot().captures_succeeded,
                successes_before,
                "captures_succeeded must NOT change when capture was cancelled"
            );

            // The in-progress guard must NOT be stuck — a subsequent
            // uncancelled capture() must still succeed.
            engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .expect("subsequent capture after pre-cancel must succeed");
        });
    }

    /// ft-xbnl0.2.3 Cx-first: capture_with_cx happy path with a live Cx
    /// produces the same snapshot shape as legacy capture(). No
    /// regression on the success path.
    #[test]
    fn capture_with_cx_happy_path_matches_legacy_capture() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let cx = crate::cx::for_request();
            let snap = engine
                .capture_with_cx(&cx, &panes, SnapshotTrigger::Manual)
                .await
                .expect("capture_with_cx happy path");

            assert_eq!(snap.trigger, SnapshotTrigger::Manual);
            assert_eq!(snap.pane_count, 1);
            assert!(
                snap.total_bytes > 0,
                "captured snapshot must record nonzero bytes"
            );

            let tel = engine.telemetry().snapshot();
            assert!(
                tel.captures_succeeded >= 1,
                "captures_succeeded must increment on happy path"
            );
            assert!(
                tel.panes_captured >= 1,
                "panes_captured must increment on happy path"
            );
        });
    }

    #[test]
    fn dedup_skips_periodic_fallback_too() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            // First capture sets the hash
            engine
                .capture(&panes, SnapshotTrigger::PeriodicFallback)
                .await
                .unwrap();

            // PeriodicFallback should also be deduped
            let r2 = engine
                .capture(&panes, SnapshotTrigger::PeriodicFallback)
                .await;
            assert!(matches!(r2, Err(SnapshotError::NoChanges)));
        });
    }

    #[test]
    fn dedup_does_not_skip_event_triggers() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .unwrap();

            // Event, Shutdown, WorkCompleted etc. should NOT be deduped
            let r2 = engine.capture(&panes, SnapshotTrigger::Event).await;
            assert!(r2.is_ok());
        });
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_trigger_serde_json_is_snake_case() {
        // Verify #[serde(rename_all = "snake_case")] produces expected strings
        assert_eq!(
            serde_json::to_string(&SnapshotTrigger::Periodic).unwrap(),
            "\"periodic\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotTrigger::PeriodicFallback).unwrap(),
            "\"periodic_fallback\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotTrigger::WorkCompleted).unwrap(),
            "\"work_completed\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotTrigger::HazardThreshold).unwrap(),
            "\"hazard_threshold\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotTrigger::StateTransition).unwrap(),
            "\"state_transition\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotTrigger::IdleWindow).unwrap(),
            "\"idle_window\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotTrigger::MemoryPressure).unwrap(),
            "\"memory_pressure\""
        );
    }

    #[test]
    fn snapshot_trigger_copy_semantics() {
        let a = SnapshotTrigger::Manual;
        let b = a; // Copy
        let c = a; // Still valid after copy
        assert_eq!(b, c);
        assert_eq!(a, SnapshotTrigger::Manual);
    }

    #[test]
    fn snapshot_trigger_debug_not_empty() {
        let dbg = format!("{:?}", SnapshotTrigger::WorkCompleted);
        assert_ne!(dbg, "");
        assert!(dbg.contains("WorkCompleted"));
    }

    #[test]
    fn snapshot_error_debug_format() {
        let err = SnapshotError::InProgress;
        let dbg = format!("{:?}", err);
        assert!(
            dbg.contains("InProgress"),
            "Debug should contain variant name"
        );

        let db_err = SnapshotError::Database("CONTENT_FREE_DATABASE_SOURCE".into());
        let dbg2 = format!("{:?}", db_err);
        assert!(dbg2.contains("Database"));
        assert!(!dbg2.contains("CONTENT_FREE_DATABASE_SOURCE"));

        let dedup = LastDedupCheckpoint {
            dedup_hash: "snpd2:CONTENT_FREE_DEDUP_HASH".to_string(),
            identity: SnapshotCheckpointIdentity {
                checkpoint_id: 42,
                session_id: "sess-CONTENT_FREE_SESSION".to_string(),
                checkpoint_at: 84,
                checkpoint_role: "CONTENT_FREE_ROLE".to_string(),
                state_hash: "snp2:CONTENT_FREE_STATE_HASH".to_string(),
            },
        };
        let dedup_debug = format!("{dedup:?}");
        assert!(dedup_debug.contains("LastDedupCheckpoint"));
        assert!(dedup_debug.contains("checkpoint_id: 42"));
        for canary in [
            "CONTENT_FREE_DEDUP_HASH",
            "CONTENT_FREE_SESSION",
            "CONTENT_FREE_ROLE",
            "CONTENT_FREE_STATE_HASH",
        ] {
            assert!(!dedup_debug.contains(canary));
        }

        let prepared = PreparedSnapshotPersistence {
            topology_json: "CONTENT_FREE_TOPOLOGY".to_string(),
            metadata_json: Some("CONTENT_FREE_METADATA".to_string()),
            panes: Vec::new(),
            pane_count: 3,
            pane_count_sql: 3,
            total_bytes: 5,
            total_bytes_sql: 5,
            persisted_text_bytes: 7,
            truncated_pane_count: 1,
            dedup_hash: "snpd2:CONTENT_FREE_PREPARED_HASH".to_string(),
        };
        let new_session = NewSessionMetadata {
            ft_version: "CONTENT_FREE_VERSION".to_string(),
            host_id: Some("CONTENT_FREE_HOST".to_string()),
        };
        let receipt = CheckpointCommitReceipt {
            session_id: "sess-CONTENT_FREE_RECEIPT_SESSION".to_string(),
            checkpoint_id: 42,
            state_hash: "snp2:CONTENT_FREE_RECEIPT_HASH".to_string(),
            total_bytes: 5,
            persisted_text_bytes: 7,
            truncated_pane_count: 1,
        };
        let private_debug = format!("{prepared:?} {new_session:?} {receipt:?}");
        assert!(private_debug.contains("PreparedSnapshotPersistence"));
        assert!(private_debug.contains("has_metadata: true"));
        assert!(private_debug.contains("CheckpointCommitReceipt"));
        for canary in [
            "CONTENT_FREE_TOPOLOGY",
            "CONTENT_FREE_METADATA",
            "CONTENT_FREE_PREPARED_HASH",
            "CONTENT_FREE_VERSION",
            "CONTENT_FREE_HOST",
            "CONTENT_FREE_RECEIPT_SESSION",
            "CONTENT_FREE_RECEIPT_HASH",
        ] {
            assert!(!private_debug.contains(canary));
        }
    }

    #[test]
    fn snapshot_result_fields_after_capture_are_consistent() {
        // SnapshotResult is Clone — verify clone preserves all fields
        let result = SnapshotResult {
            session_id: "sess-test-001".to_string(),
            checkpoint_id: 42,
            checkpoint_at: 1_234,
            state_hash: "snp2:test".to_string(),
            pane_count: 3,
            total_bytes: 1024,
            persisted_text_bytes: 2048,
            truncated_pane_count: 1,
            trigger: SnapshotTrigger::Manual,
        };
        let cloned = result.clone();
        assert_eq!(cloned.session_id, "sess-test-001");
        assert_eq!(cloned.checkpoint_id, 42);
        assert_eq!(cloned.checkpoint_at, 1_234);
        assert_eq!(cloned.state_hash, "snp2:test");
        assert_eq!(cloned.pane_count, 3);
        assert_eq!(cloned.total_bytes, 1024);
        assert_eq!(cloned.persisted_text_bytes, 2048);
        assert_eq!(cloned.truncated_pane_count, 1);
        assert_eq!(cloned.trigger, SnapshotTrigger::Manual);
    }

    #[test]
    fn state_detection_max_age_is_five_minutes() {
        assert_eq!(STATE_DETECTION_MAX_AGE, Duration::from_secs(300));
    }

    #[test]
    fn compute_state_hash_differs_on_cwd_change() {
        let mut p1 = make_test_pane(1, 24, 80);
        p1.cwd = Some("file:///home/user/project-a".to_string());
        let mut p2 = make_test_pane(1, 24, 80);
        p2.cwd = Some("file:///home/user/project-b".to_string());

        let h1 = compute_state_hash(&[p1]);
        let h2 = compute_state_hash(&[p2]);
        assert_ne!(h1, h2, "different cwd should produce different hash");
    }

    #[test]
    fn compute_state_hash_differs_on_title_change() {
        let mut p1 = make_test_pane(1, 24, 80);
        p1.title = Some("vim".to_string());
        let mut p2 = make_test_pane(1, 24, 80);
        p2.title = Some("bash".to_string());

        let h1 = compute_state_hash(&[p1]);
        let h2 = compute_state_hash(&[p2]);
        assert_ne!(h1, h2, "different title should produce different hash");
    }

    #[test]
    fn compute_state_hash_differs_on_cursor_position() {
        let mut p1 = make_test_pane(1, 24, 80);
        p1.cursor_x = Some(0);
        p1.cursor_y = Some(0);
        let mut p2 = make_test_pane(1, 24, 80);
        p2.cursor_x = Some(40);
        p2.cursor_y = Some(12);

        let h1 = compute_state_hash(&[p1]);
        let h2 = compute_state_hash(&[p2]);
        assert_ne!(
            h1, h2,
            "different cursor position should produce different hash"
        );
    }

    #[test]
    fn compute_state_hash_ignores_unpersisted_is_zoomed() {
        let mut p1 = make_test_pane(1, 24, 80);
        p1.is_zoomed = false;
        let mut p2 = make_test_pane(1, 24, 80);
        p2.is_zoomed = true;

        let h1 = compute_state_hash(&[p1]);
        let h2 = compute_state_hash(&[p2]);
        assert_eq!(
            h1, h2,
            "zoom state is not represented in the durable topology or pane rows"
        );
    }

    #[test]
    fn compute_state_hash_differs_on_tab_id() {
        let mut p1 = make_test_pane(1, 24, 80);
        p1.tab_id = 0;
        let mut p2 = make_test_pane(1, 24, 80);
        p2.tab_id = 5;

        let h1 = compute_state_hash(&[p1]);
        let h2 = compute_state_hash(&[p2]);
        assert_ne!(h1, h2, "different tab_id should produce different hash");
    }

    #[test]
    fn compute_state_hash_differs_on_window_id() {
        let mut p1 = make_test_pane(1, 24, 80);
        p1.window_id = 0;
        let mut p2 = make_test_pane(1, 24, 80);
        p2.window_id = 99;

        let h1 = compute_state_hash(&[p1]);
        let h2 = compute_state_hash(&[p2]);
        assert_ne!(h1, h2, "different window_id should produce different hash");
    }

    #[test]
    fn compute_state_hash_many_panes_deterministic() {
        let panes: Vec<PaneInfo> = (0..50).map(|i| make_test_pane(i, 24, 80)).collect();
        let h1 = compute_state_hash(&panes);
        let h2 = compute_state_hash(&panes);
        assert_eq!(h1, h2, "hash of 50 panes should be deterministic");
        assert_eq!(h1.len(), 70, "hash should use the snpd2 SHA-256 shape");
    }

    #[test]
    fn generate_session_id_has_correct_structure() {
        let id = generate_session_id();
        let parts: Vec<&str> = id.splitn(3, '-').collect();
        assert_eq!(parts.len(), 3, "session ID has 3 dash-separated parts");
        assert_eq!(parts[0], "sess");
        assert_eq!(
            parts[1].len(),
            13,
            "timestamp hex should be 13 chars (zero-padded)"
        );
        assert_eq!(
            parts[2].len(),
            16,
            "random hex should be 16 chars (zero-padded)"
        );
        // All hex chars
        assert!(
            parts[1].chars().all(|c| c.is_ascii_hexdigit()),
            "timestamp part should be hex"
        );
        assert!(
            parts[2].chars().all(|c| c.is_ascii_hexdigit()),
            "random part should be hex"
        );
    }

    #[test]
    fn capture_with_minimal_pane_info() {
        run_async_test(async {
            // Pane with all optional fields set to None
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let pane = PaneInfo {
                pane_id: 1,
                tab_id: 0,
                window_id: 0,
                domain_id: None,
                domain_name: None,
                workspace: None,
                size: None,
                rows: None,
                cols: None,
                title: None,
                cwd: None,
                tty_name: None,
                cursor_x: None,
                cursor_y: None,
                cursor_visibility: None,
                left_col: None,
                top_row: None,
                is_active: false,
                is_zoomed: false,
                extra: std::collections::HashMap::new(),
            };

            let result = engine.capture(&[pane], SnapshotTrigger::Manual).await;
            assert!(result.is_ok(), "capture with minimal pane should succeed");
            let snap = result.unwrap();
            assert_eq!(snap.pane_count, 1);
        });
    }

    #[test]
    fn capture_stores_correct_checkpoint_type_in_db() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let r = engine
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .unwrap();

            let conn = Connection::open(db_path.as_str()).unwrap();
            let ctype: String = conn
                .query_row(
                    "SELECT checkpoint_type FROM session_checkpoints WHERE id = ?1",
                    [r.checkpoint_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(ctype, "startup");
        });
    }

    #[test]
    fn capture_stores_event_type_for_manual() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let r = engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .unwrap();

            let conn = Connection::open(db_path.as_str()).unwrap();
            let ctype: String = conn
                .query_row(
                    "SELECT checkpoint_type FROM session_checkpoints WHERE id = ?1",
                    [r.checkpoint_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(ctype, "event");
        });
    }

    #[test]
    fn multiple_checkpoints_have_increasing_ids() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

            let r1 = engine
                .capture(&[make_test_pane(1, 24, 80)], SnapshotTrigger::Manual)
                .await
                .unwrap();
            let r2 = engine
                .capture(&[make_test_pane(2, 30, 120)], SnapshotTrigger::Manual)
                .await
                .unwrap();
            let r3 = engine
                .capture(&[make_test_pane(3, 40, 160)], SnapshotTrigger::Manual)
                .await
                .unwrap();

            assert!(
                r1.checkpoint_id < r2.checkpoint_id,
                "checkpoint IDs should be monotonically increasing"
            );
            assert!(
                r2.checkpoint_id < r3.checkpoint_id,
                "checkpoint IDs should be monotonically increasing"
            );
        });
    }

    #[test]
    fn capture_total_bytes_is_nonzero() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            let r = engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .unwrap();
            assert!(
                r.total_bytes > 0,
                "total_bytes should be positive for a valid capture"
            );
        });
    }

    #[test]
    fn capture_reports_exact_truncation_and_telemetry_deltas() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());
            let before = engine.telemetry().snapshot();
            let mut pane = make_test_pane(1, 24, 80);
            pane.title = Some("t".repeat(SNAPSHOT_TEXT_FIELD_INPUT_BYTES + 1));

            let result = engine
                .capture(&[pane], SnapshotTrigger::Manual)
                .await
                .expect("oversized-but-admissible title must be bounded and persisted");
            let after = engine.telemetry().snapshot();

            assert_eq!(result.pane_count, 1);
            assert_eq!(result.truncated_pane_count, 1);
            assert!(result.persisted_text_bytes > 0);
            assert_eq!(after.captures_attempted - before.captures_attempted, 1);
            assert_eq!(after.captures_succeeded - before.captures_succeeded, 1);
            assert_eq!(after.panes_captured - before.panes_captured, 1);
            assert_eq!(
                after.pane_states_truncated - before.pane_states_truncated,
                u64::try_from(result.truncated_pane_count).unwrap()
            );
            assert_eq!(
                after.persisted_text_bytes - before.persisted_text_bytes,
                u64::try_from(result.persisted_text_bytes).unwrap()
            );
            assert_eq!(after.capture_errors, before.capture_errors);
        });
    }

    #[test]
    fn shutdown_checkpoint_persists_final_receipt_when_state_is_unchanged() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            // First capture to set hash
            engine
                .capture(&panes, SnapshotTrigger::Periodic)
                .await
                .unwrap();

            // Shutdown bypasses periodic dedup so callers always receive a
            // durable final-checkpoint receipt before the clean mark.
            let result = engine
                .shutdown_checkpoint(&panes, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(result.trigger, SnapshotTrigger::Shutdown);
        });
    }

    #[test]
    fn shutdown_checkpoint_with_empty_panes() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

            // First capture to establish session
            engine
                .capture(&[make_test_pane(1, 24, 80)], SnapshotTrigger::Startup)
                .await
                .unwrap();

            // A terminal empty observation is still authority-bearing: it
            // closes a previously non-empty session without pretending stale
            // panes survived shutdown.
            let result = engine
                .shutdown_checkpoint(&[], Duration::from_secs(5))
                .await
                .expect("empty terminal checkpoint must persist");
            assert_eq!(result.pane_count, 0);
        });
    }

    #[test]
    fn dedup_does_not_skip_shutdown_trigger() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            engine
                .capture(&panes, SnapshotTrigger::Periodic)
                .await
                .unwrap();

            // The reservation-bound shutdown path should NOT be deduped even
            // with identical data; direct ordinary admission is forbidden.
            let r2 = engine
                .shutdown_checkpoint(&panes, Duration::from_secs(5))
                .await;
            assert!(r2.is_ok(), "reserved shutdown bypasses dedup");
        });
    }

    #[test]
    fn dedup_does_not_skip_work_completed_trigger() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let panes = vec![make_test_pane(1, 24, 80)];

            engine
                .capture(&panes, SnapshotTrigger::Periodic)
                .await
                .unwrap();

            // WorkCompleted should NOT be deduped
            let r2 = engine.capture(&panes, SnapshotTrigger::WorkCompleted).await;
            assert!(r2.is_ok(), "WorkCompleted bypasses dedup");
        });
    }

    #[test]
    fn open_conn_sets_wal_mode() {
        let (_tmp, db_path) = setup_test_db();
        let conn = open_conn(db_path.as_str()).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "connection should be in WAL mode");
    }

    #[test]
    fn cleanup_with_zero_retention_count_deletes_all() {
        run_async_test(async {
            let (_tmp, db_path) = setup_test_db();
            let config = SnapshotConfig {
                retention_count: 0,
                retention_days: 365,
                ..SnapshotConfig::default()
            };
            let engine = SnapshotEngine::new(db_path.clone(), config);

            // Create 3 checkpoints
            for i in 0..3u64 {
                engine
                    .capture(
                        &[make_test_pane(i, 24 + i as u32, 80)],
                        SnapshotTrigger::Manual,
                    )
                    .await
                    .unwrap();
            }

            let deleted = engine.cleanup().await.unwrap();
            assert_eq!(
                deleted, 3,
                "retention_count=0 should delete all checkpoints"
            );

            let count = checkpoint_count(db_path.as_str());
            assert_eq!(count, 0);
        });
    }

    #[test]
    fn snapshot_config_default_has_sane_values() {
        let config = SnapshotConfig::default();
        assert!(
            config.interval_seconds >= 30,
            "interval should be at least 30s"
        );
        assert!(
            config.retention_count > 0,
            "retention count should be positive"
        );
        assert!(
            config.retention_days > 0,
            "retention days should be positive"
        );
    }

    // ── Telemetry counter tests ──────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let engine =
            SnapshotEngine::new(Arc::new(":memory:".to_string()), SnapshotConfig::default());
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.captures_attempted, 0);
        assert_eq!(snap.captures_succeeded, 0);
        assert_eq!(snap.dedup_skips, 0);
        assert_eq!(snap.capture_errors, 0);
        assert_eq!(snap.cleanup_runs, 0);
        assert_eq!(snap.cleanup_removed, 0);
        assert_eq!(snap.triggers_emitted, 0);
        assert_eq!(snap.triggers_accepted, 0);
        assert_eq!(snap.panes_captured, 0);
        assert_eq!(snap.bytes_persisted, 0);
        assert_eq!(snap.persisted_text_bytes, 0);
        assert_eq!(snap.pane_states_truncated, 0);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let telem = SnapshotEngineTelemetry::new();
        saturating_telemetry_add(&telem.captures_attempted, 5);
        saturating_telemetry_add(&telem.captures_succeeded, 3);
        saturating_telemetry_add(&telem.dedup_skips, 2);

        let snap = telem.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: SnapshotEngineTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.captures_attempted, 5);
        assert_eq!(parsed.captures_succeeded, 3);
        assert_eq!(parsed.dedup_skips, 2);
    }

    #[test]
    fn telemetry_add_saturates_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);
        saturating_telemetry_add(&counter, 10);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        saturating_telemetry_add(&counter, 1);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
}
