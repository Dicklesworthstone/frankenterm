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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_correlator::AgentCorrelator;
use crate::checkpoint_witness::{
    CHECKPOINT_ROLE_SNAPSHOT, CheckpointWitnessError, PersistedPaneState,
    canonical_json_string, checkpoint_witness, snapshot_dedup_witness,
};
use crate::config::{SnapshotConfig, SnapshotSchedulingMode};
use crate::outcome::CancelKind;
use crate::patterns::{AgentType, Detection, Severity};
use crate::runtime_async::{LockAcquireError, Mutex, RwLock, mpsc, watch};
use crate::session_pane_state::{PaneStateSnapshot, ScrollbackRef};
use crate::session_topology::TopologySnapshot;
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
        match counter.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
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
    /// Number of cleanup() calls.
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
    /// Number of cleanup() calls.
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

/// Result of a successful snapshot capture.
#[derive(Debug, Clone)]
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
    /// What triggered this snapshot.
    pub trigger: SnapshotTrigger,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCheckpointIdentity {
    pub checkpoint_id: i64,
    pub session_id: String,
    pub checkpoint_at: u64,
    pub checkpoint_role: String,
    pub state_hash: String,
}

/// In-memory periodic-dedup hint bound to the exact durable row that made the
/// hint safe. The database identity is revalidated before every skip because
/// another engine or process may prune that row without access to this cache.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastDedupCheckpoint {
    dedup_hash: String,
    identity: SnapshotCheckpointIdentity,
}

/// Transaction-local checkpoint deletion selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotDeleteTarget {
    /// Delete the row currently carrying this numeric ID.
    Id(i64),
    /// Delete only if every immutable identity field still matches.
    Exact(SnapshotCheckpointIdentity),
    /// Resolve and delete the deterministic latest row atomically.
    Latest(SnapshotCheckpointRoleScope),
}

/// Receipt for one authority-serialized checkpoint deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Per-capture options for call sites that need snapshot behavior beyond the
/// engine's normal periodic/event defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotCaptureOptions {
    /// Link the checkpoint to already-captured scrollback segments.
    pub include_scrollback: bool,
    /// Optional operator/event metadata persisted atomically with the
    /// checkpoint and covered by its v2 witness.
    pub metadata: Option<Value>,
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
}

impl SnapshotAuthorityOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CheckpointCommit => "checkpoint_commit",
            Self::CheckpointCleanup => "checkpoint_cleanup",
            Self::SessionRetentionCleanup => "session_retention_cleanup",
            Self::ShutdownMark => "shutdown_mark",
            Self::CheckpointDelete => "checkpoint_delete",
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::CheckpointCommit => 1,
            Self::CheckpointCleanup => 2,
            Self::SessionRetentionCleanup => 3,
            Self::ShutdownMark => 4,
            Self::CheckpointDelete => 5,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::CheckpointCommit),
            2 => Some(Self::CheckpointCleanup),
            3 => Some(Self::SessionRetentionCleanup),
            4 => Some(Self::ShutdownMark),
            5 => Some(Self::CheckpointDelete),
            _ => None,
        }
    }
}

impl std::fmt::Display for SnapshotAuthorityOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Error returned when a snapshot-engine operation cannot complete safely.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot already in progress")]
    InProgress,
    #[error("snapshot capture admission is closed for shutdown")]
    ShuttingDown,
    #[error("snapshot scheduler already running for this engine")]
    SchedulerInProgress,
    #[error("no panes found")]
    NoPanes,
    #[error("no changes since last snapshot")]
    NoChanges,
    #[error("pane listing failed: {0}")]
    PaneList(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("serialization error: {0}")]
    Serialization(String),
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
    #[error("clean-shutdown mark failed after final checkpoint settlement: {source}")]
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

impl SnapshotError {
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
                | Self::Serialization(_)
                | Self::BlockingRuntimeFailure
                | Self::LockTimedOut { .. }
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

fn classify_shutdown_timeout(
    cx: &crate::cx::Cx,
    timeout: Duration,
) -> SnapshotError {
    match cx.root_cancel_cause().map(|reason| reason.kind) {
        Some(CancelKind::Deadline | CancelKind::Timeout) => SnapshotError::DeadlineExceeded,
        Some(CancelKind::PollQuota) => SnapshotError::PollQuotaExhausted,
        Some(CancelKind::CostBudget) => SnapshotError::CostBudgetExhausted,
        Some(_) => SnapshotError::Cancelled,
        None if cx.is_cancel_requested() => SnapshotError::ContextFailure,
        None => SnapshotError::ShutdownTimedOut {
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        },
    }
}

// =============================================================================
// SnapshotEngine
// =============================================================================

/// Retry-safe automatic session-cleanup failures and admission contention are
/// retried soon enough to recover without waiting for the normal hours-long
/// cadence, but not on every 250 ms intelligent-scheduler poll.
const SESSION_CLEANUP_RETRY_DELAY: Duration = Duration::from_secs(30);
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
    /// The attempted work settled without an indeterminate durable effect, but
    /// a transient database, serialization, pane-list, blocking-runtime, or
    /// lock-timeout failure prevented a checkpoint receipt.
    RetrySafeFailure,
}

impl SchedulerCaptureOutcome {
    const fn settled(self) -> bool {
        matches!(self, Self::Captured | Self::Unchanged)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SchedulerCaptureRetryState {
    consecutive_retry_safe_failures: u32,
}

impl SchedulerCaptureRetryState {
    fn record_settled(&mut self) {
        self.consecutive_retry_safe_failures = 0;
    }

    fn retry_deadline(
        &mut self,
        now: Instant,
        trigger: SnapshotTrigger,
        reason: SchedulerCaptureDeferredReason,
    ) -> Instant {
        if reason == SchedulerCaptureDeferredReason::RetrySafeFailure {
            self.consecutive_retry_safe_failures =
                self.consecutive_retry_safe_failures.saturating_add(1);
        }
        scheduler_capture_retry_deadline(
            now,
            trigger,
            reason,
            self.consecutive_retry_safe_failures,
        )
    }
}

fn scheduler_capture_retry_delay(
    trigger: SnapshotTrigger,
    reason: SchedulerCaptureDeferredReason,
    consecutive_retry_safe_failures: u32,
) -> Duration {
    match reason {
        SchedulerCaptureDeferredReason::RetrySafeFailure => {
            let exponent = consecutive_retry_safe_failures.saturating_sub(1).min(5);
            let delay = SCHEDULER_RETRY_SAFE_CAPTURE_MIN_DELAY
                .saturating_mul(1_u32 << exponent);
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
    consecutive_retry_safe_failures: u32,
) -> Instant {
    // Both fixed delays are tiny relative to every representable production
    // `Instant`. Falling back to `now` at the numeric ceiling preserves
    // liveness instead of silently dropping a deferred trigger.
    now.checked_add(scheduler_capture_retry_delay(
        trigger,
        reason,
        consecutive_retry_safe_failures,
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
/// `last_authoritative_success` advances only after a typed successful cleanup
/// receipt. `retry_deferred_at` rate-limits admission contention and failures
/// that are explicitly safe to retry. Indeterminate outcomes are governed by
/// the engine-owned sticky reconciliation latch instead of this schedule.
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
                CAPTURE_LIFECYCLE_SHUTDOWN_RESERVED_OWNED
                | CAPTURE_LIFECYCLE_SHUTDOWN_ACTIVE => CAPTURE_LIFECYCLE_SHUTDOWN_RETRYABLE,
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
        SnapshotAuthorityOperation::from_code(
            self.first_latched_operation.load(Ordering::Acquire),
        )
    }
}

enum SnapshotAuthorityRegistryEntry {
    Live(Weak<SnapshotAuthorityState>),
    Latched(Arc<SnapshotAuthorityState>),
}

fn snapshot_authority_registry(
) -> &'static StdMutex<HashMap<String, SnapshotAuthorityRegistryEntry>> {
    static REGISTRY: OnceLock<
        StdMutex<HashMap<String, SnapshotAuthorityRegistryEntry>>,
    > = OnceLock::new();
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
) {
    let identities = snapshot_authority_file_identities(db_path);
    if identities.is_empty() {
        return;
    }

    let mut conflicts: Vec<Arc<SnapshotAuthorityState>> = Vec::new();
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

        if conflicts.is_empty() {
            let mut registered = state
                .registry_identities
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            return;
        }
    }

    // A split was already live before refresh could publish the inode. Do not
    // guess which state owned earlier work: latch every participant after
    // releasing registry locks so all future mutations fail closed.
    tracing::error!(
        conflict_count = conflicts.len(),
        "detected split snapshot authority for one SQLite filesystem object"
    );
    state.latch_reconciliation(SnapshotAuthorityOperation::CheckpointCommit);
    for conflict in conflicts {
        conflict.latch_reconciliation(SnapshotAuthorityOperation::CheckpointCommit);
    }
}

fn freeze_filesystem_path_from_base(path: &Path, base: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    std::fs::canonicalize(&absolute).or_else(|_| {
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
    if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
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
        Some(vfs) => {
            (cfg!(unix) && vfs == b"unix") || (cfg!(windows) && vfs == b"win32")
        }
    }
}

fn sqlite_uri_raw_file_path(raw_path: &str) -> Option<&str> {
    let Some(authority_and_path) = raw_path.strip_prefix("//") else {
        return Some(raw_path);
    };
    let (authority, path) = authority_and_path
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
        .and_then(|uri| uri.split_once('#').map_or(uri, |(before, _)| before).split_once('?'))
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
        if mode.as_deref().is_some_and(|value| {
            !matches!(value, b"ro" | b"rw" | b"rwc" | b"memory")
        }) || cache
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
        let memory = uri_path == b":memory:"
            || mode.as_deref().is_some_and(|value| value == b"memory");
        if vfs.as_deref() == Some(b"memdb") {
            // Bundled SQLite's memdb VFS chooses its backing-store identity
            // solely from the decoded filename: names longer than one byte
            // beginning with `/` or `\` share one MemStore regardless of
            // mode/cache. Core `cache=shared` also shares a BtShared for the
            // same filename/VFS before a second xOpen. Both mechanisms resolve
            // to one authority key; every other memdb spelling is private.
            let shared = cache.as_deref() == Some(b"shared")
                || (uri_path.len() > 1
                    && matches!(uri_path.first(), Some(b'/') | Some(b'\\')));
            return shared.then(|| {
                format!(
                    "sqlite-memdb-vfs:{}",
                    sqlite_uri_identity_bytes(uri_path)
                )
            });
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
    let mut entries = snapshot_authority_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    entries.retain(|_, entry| match entry {
        SnapshotAuthorityRegistryEntry::Live(state) => state.strong_count() > 0,
        SnapshotAuthorityRegistryEntry::Latched(_) => true,
    });
    let state = identities
        .iter()
        .find_map(|identity| match entries.get(identity) {
            Some(SnapshotAuthorityRegistryEntry::Live(state)) => state.upgrade(),
            Some(SnapshotAuthorityRegistryEntry::Latched(state)) => Some(Arc::clone(state)),
            None => None,
        })
        .unwrap_or_else(|| {
            Arc::new(SnapshotAuthorityState::new_with_registry_identities(
                identities.clone(),
                Some(db_path.to_owned()),
            ))
        });

    {
        let mut registered = state
            .registry_identities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for identity in &identities {
            if !registered.contains(identity) {
                registered.push(identity.clone());
            }
        }
    }
    let latched = state.reconciliation_is_required();
    for identity in identities {
        let entry = if latched {
            SnapshotAuthorityRegistryEntry::Latched(Arc::clone(&state))
        } else {
            SnapshotAuthorityRegistryEntry::Live(Arc::downgrade(&state))
        };
        entries.insert(identity, entry);
    }
    state
}

/// Result transported across the blocking boundary. `Suppressed` means the
/// async owner disappeared (or the executor failed) before the closure began,
/// and the closure therefore proved that it performed no durable work.
enum AuthorityBlockingOutcome<T, E> {
    Suppressed,
    Executed(std::result::Result<T, E>),
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
trait SnapshotAuthorityWorkFailure: std::fmt::Display {
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
            if matches!(state, AUTHORITY_HANDOFF_STARTED | AUTHORITY_HANDOFF_COMPLETED) {
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
    /// Runtime-owned receiver, taken by `run_periodic`.
    trigger_rx: Mutex<Option<mpsc::Receiver<SnapshotTrigger>>>,
    /// Operational telemetry counters.
    telemetry: SnapshotEngineTelemetry,
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
            last_dedup_hash: RwLock::new(None),
            capture_lifecycle: AtomicU8::new(CAPTURE_LIFECYCLE_OPEN_IDLE),
            scheduler_in_progress: AtomicBool::new(false),
            session_cleanup_in_progress: AtomicBool::new(false),
            snapshot_authority,
            trigger_tx,
            trigger_rx: Mutex::new(Some(trigger_rx)),
            telemetry: SnapshotEngineTelemetry::new(),
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
                                crate::runtime_async::sleep_with_cx(
                                    cx,
                                    Duration::from_millis(1),
                                )
                                .await
                                .map_err(|_| SnapshotError::Cancelled)?;
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
                                crate::runtime_async::sleep_with_cx(
                                    cx,
                                    Duration::from_millis(1),
                                )
                                .await
                                .map_err(|_| SnapshotError::Cancelled)?;
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

    async fn spawn_blocking_db<T, E, F>(work: F) -> std::result::Result<T, SnapshotError>
    where
        T: Send + 'static,
        E: std::fmt::Display + Send + 'static,
        F: FnOnce() -> std::result::Result<T, E> + Send + 'static,
    {
        crate::runtime_async::spawn_blocking(work)
            .await
            .map_err(|e| SnapshotError::Database(format!("task join: {e}")))?
            .map_err(|e| SnapshotError::Database(e.to_string()))
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
        if cx.checkpoint().is_err() {
            return Err(SnapshotError::Cancelled);
        }

        let handoff_state = attempt.handoff_state();
        let authority_lifetime = Arc::clone(&attempt.authority);
        let outcome = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            // Keep the database-keyed authority alive until the queued closure
            // has either suppressed itself or reached terminal return. A new
            // engine must not prune the Weak registry entry while old work can
            // still commit.
            let _authority_lifetime = authority_lifetime;
            run_authority_work_if_started(&handoff_state, work)
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
                        error = %error,
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

    async fn spawn_blocking_db_best_effort<T, E, F>(work: F) -> T
    where
        T: Default + Send + 'static,
        E: std::fmt::Display + Send + 'static,
        F: FnOnce() -> std::result::Result<T, E> + Send + 'static,
    {
        match Self::spawn_blocking_db(work).await {
            Ok(val) => val,
            Err(e) => {
                tracing::warn!(error = %e, "best-effort DB operation failed, returning default");
                T::default()
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

        if cx.checkpoint().is_err() {
            saturating_telemetry_add(&self.telemetry.capture_errors, 1);
            return Err(SnapshotError::Cancelled);
        }
        if self.authority_reconciliation_is_required() {
            saturating_telemetry_add(&self.telemetry.capture_errors, 1);
            return Err(self.authority_reconciliation_error(
                SnapshotAuthorityOperation::CheckpointCommit,
            ));
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

        let now_ms = epoch_ms();

        // 2. Load auxiliary persisted observations without blocking an async
        // worker on SQLite.
        let pane_ids: Vec<u64> = panes.iter().map(|p| p.pane_id).collect();
        let detection_pane_ids = pane_ids.clone();
        let db_path_for_detections = Arc::clone(&self.db_path);
        let detection_max_age_ms =
            u64::try_from(STATE_DETECTION_MAX_AGE.as_millis()).unwrap_or(u64::MAX);
        let cutoff_ms: i64 = i64::try_from(now_ms.saturating_sub(detection_max_age_ms))
            .unwrap_or(i64::MAX);

        let detections_by_pane = Self::spawn_blocking_db_best_effort(move || {
            load_latest_detections_by_pane_sync(
                db_path_for_detections.as_str(),
                &detection_pane_ids,
                cutoff_ms,
            )
        })
        .await;

        let scrollback_refs = if options.include_scrollback {
            let db_path_for_scrollback = Arc::clone(&self.db_path);
            let scrollback_pane_ids = pane_ids.clone();
            Self::spawn_blocking_db(move || {
                load_latest_scrollback_refs_sync(
                    db_path_for_scrollback.as_str(),
                    &scrollback_pane_ids,
                )
            })
            .await?
        } else {
            std::collections::HashMap::new()
        };

        // 3. Topology construction, agent correlation, pane projection,
        // canonical JSON serialization, and hashing are all CPU/allocation
        // heavy for large mux domains. Move the whole pure phase off the async
        // worker. Cloning the borrowed pane slice is the only caller-thread
        // cost retained by this compatibility API; the expensive work happens
        // in the bounded blocking pool and has no durable side effects.
        let owned_panes = panes.to_vec();
        let metadata = options.metadata;
        let prepared = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let (topology, _report) = TopologySnapshot::from_panes(&owned_panes, now_ms);
            let mut correlator = AgentCorrelator::new();
            for (pane_id, detections) in detections_by_pane {
                correlator.ingest_detections(pane_id, &detections);
            }
            for pane in &owned_panes {
                correlator.update_from_pane_info(pane);
            }
            let pane_states: Vec<PaneStateSnapshot> = owned_panes
                .iter()
                .map(|pane| {
                    let mut snapshot =
                        PaneStateSnapshot::from_pane_info(pane, now_ms, false);
                    if let Some(scrollback_ref) = scrollback_refs.get(&pane.pane_id) {
                        snapshot = snapshot.with_scrollback(scrollback_ref.clone());
                    }
                    if let Some(agent) = correlator.get_metadata(pane.pane_id) {
                        snapshot = snapshot.with_agent(agent);
                    }
                    snapshot
                })
                .collect();
            prepare_snapshot_persistence(&topology, &pane_states, metadata.as_ref())
        })
        .await
        .map_err(classify_snapshot_pure_blocking_failure)?
        .map_err(|error| SnapshotError::Serialization(error.to_string()))?;
        let dedup_hash = prepared.dedup_hash.clone();

        // 4. Skip if periodic-like and unchanged — Cx-bound read
        if matches!(
            trigger,
            SnapshotTrigger::Periodic | SnapshotTrigger::PeriodicFallback
        ) {
            let cached_checkpoint = {
                let last = self
                    .last_dedup_hash
                    .read_with_cx(cx)
                    .await
                    .map_err(snapshot_lock_error)?;
                last.as_ref()
                    .filter(|cached| cached.dedup_hash == dedup_hash)
                    .cloned()
            };
            if self.authority_reconciliation_is_required() {
                return Err(self.authority_reconciliation_error(
                    SnapshotAuthorityOperation::CheckpointCommit,
                ));
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
        let session_id = session_id_guard
            .clone()
            .unwrap_or_else(generate_session_id);
        let new_session = creates_session.then(|| NewSessionMetadata {
            ft_version: crate::VERSION.to_string(),
            host_id: current_host_id(),
        });

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
        let authority_for_identity_refresh = Arc::clone(&self.snapshot_authority);

        let result = self
            .spawn_blocking_authority_with_cx(
                cx,
                SnapshotAuthorityOperation::CheckpointCommit,
                move || {
                    let receipt = save_checkpoint_authoritatively_sync(
                        db_path.as_str(),
                        &session_id,
                        now_ms,
                        &checkpoint_type,
                        &prepared,
                        new_session.as_ref(),
                    )?;
                    // Keep mutation admission held while publishing an inode
                    // discovered by the first commit, closing the construction
                    // race with a later hard-link-spelled engine.
                    refresh_snapshot_authority_file_identities(
                        db_path.as_str(),
                        &authority_for_identity_refresh,
                    );
                    Ok(receipt)
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
        capture_guard.complete_without_error();

        Ok(SnapshotResult {
            session_id: result.session_id,
            checkpoint_id: result.checkpoint_id,
            checkpoint_at: now_ms,
            state_hash: result.state_hash,
            pane_count,
            total_bytes: result.total_bytes,
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

        Ok(removed)
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
        Fut:
            std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
    {
        match pane_provider().await {
            Ok(panes) => match self.capture(&panes, trigger).await {
                Ok(result) => {
                    tracing::info!(
                        trigger = ?trigger,
                        pane_count = result.pane_count,
                        total_bytes = result.total_bytes,
                        checkpoint_id = result.checkpoint_id,
                        "snapshot captured"
                    );
                    if let Err(e) = self.cleanup().await {
                        tracing::warn!(error = %e, "snapshot retention cleanup failed");
                    }
                    true
                }
                Err(SnapshotError::NoChanges) => {
                    tracing::debug!(trigger = ?trigger, "snapshot skipped: no changes");
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
                    tracing::warn!(trigger = ?trigger, error = %e, "snapshot capture failed");
                    false
                }
            },
            Err(error) => {
                tracing::warn!(trigger = ?trigger, error = %error, "snapshot pane listing failed");
                false
            }
        }
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`capture_from_provider`].
    ///
    /// Routes both the `capture` and post-capture `cleanup` writes
    /// through their cx-first siblings so the scheduler's main work
    /// loop honours caller cancellation across every write seam.
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
        Fut:
            std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
    {
        if self.authority_reconciliation_is_required() {
            return Err(self.authority_reconciliation_error(
                SnapshotAuthorityOperation::CheckpointCommit,
            ));
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
            Ok(panes) => match self.capture_with_cx(cx, &panes, trigger).await {
                Ok(result) => {
                    tracing::info!(
                        trigger = ?trigger,
                        pane_count = result.pane_count,
                        total_bytes = result.total_bytes,
                        checkpoint_id = result.checkpoint_id,
                        "snapshot captured (cx path)"
                    );
                    if let Err(error) = self.cleanup_with_cx(cx).await {
                        if error.requires_reconciliation() {
                            tracing::warn!(
                                error = %error,
                                "snapshot retention cleanup requires durable-state reconciliation"
                            );
                            return Err(error);
                        }
                        tracing::warn!(
                            error = %error,
                            "snapshot retention cleanup failed (cx path)"
                        );
                    }
                    Ok(SchedulerCaptureOutcome::Captured)
                }
                Err(SnapshotError::NoChanges) => {
                    tracing::debug!(trigger = ?trigger, "snapshot skipped: no changes");
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
                Err(error) if error.is_retry_safe_scheduler_failure() => {
                    tracing::warn!(
                        trigger = ?trigger,
                        error = %error,
                        "snapshot capture failed retry-safely; scheduler will retain demand with bounded backoff"
                    );
                    Ok(SchedulerCaptureOutcome::Deferred(
                        SchedulerCaptureDeferredReason::RetrySafeFailure,
                    ))
                }
                Err(e) => {
                    tracing::warn!(trigger = ?trigger, error = %e, "snapshot capture failed");
                    Err(e)
                }
            },
            Err(error) if error.is_retry_safe_scheduler_failure() => {
                tracing::warn!(
                    trigger = ?trigger,
                    error = %error,
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
        Fut:
            std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
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
        Fut:
            std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
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
        Fut:
            std::future::Future<Output = std::result::Result<Vec<PaneInfo>, SnapshotError>> + Send,
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
                    let next_wait = periodic_scheduler_wait_duration(
                        &session_cleanup_schedule,
                        self.config.session_retention.cleanup_interval_hours,
                        self.authority_reconciliation_is_required(),
                        last_snapshot_completion,
                        snapshot_retry_not_before,
                        interval,
                        now,
                    );

                    if !next_wait.is_zero() {
                        // ft-xbnl0.2.3 tick 296: cx-first timeout wrapping
                        // shutdown.changed(cx) — both the outer interval-timeout
                        // AND the inner shutdown wait now honor cx-cancel.
                        let shutdown_fut = shutdown.changed(cx);
                        let shutdown_wait = crate::runtime_async::timeout_with_cx(
                            cx,
                            next_wait,
                            shutdown_fut,
                        )
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

                    self.maybe_run_session_cleanup(cx, &mut session_cleanup_schedule)
                        .await;
                    cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
                    if *shutdown.borrow() {
                        tracing::info!("snapshot engine shutting down after cleanup");
                        break;
                    }

                    let now = Instant::now();
                    let snapshot_is_due = last_snapshot_completion.is_none_or(|last| {
                        now.saturating_duration_since(last) >= interval
                    }) && snapshot_retry_not_before.is_none_or(|retry_at| now >= retry_at);
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

                let mut trigger_rx = {
                    let mut guard = self.trigger_rx.lock().await;
                    match guard.take() {
                        Some(rx) => rx,
                        None => {
                            tracing::warn!(
                                "snapshot intelligent scheduler: receiver already taken"
                            );
                            return Ok(());
                        }
                    }
                };

                // Retention has its own startup contract. Attempt it before
                // pane capture so a transient capture/provider failure cannot
                // suppress cleanup for the entire scheduler invocation.
                self.maybe_run_session_cleanup(cx, &mut session_cleanup_schedule)
                    .await;
                cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
                if *shutdown.borrow() {
                    tracing::info!("snapshot engine shutting down after startup cleanup");
                    return Ok(());
                }

                let startup_outcome = self
                    .capture_from_provider_with_cx(
                        cx,
                        &pane_provider,
                        SnapshotTrigger::Startup,
                    )
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

                    self.maybe_run_session_cleanup(cx, &mut session_cleanup_schedule)
                        .await;
                    cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
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
                                if self.is_immediate_trigger(trigger)
                                    || snapshot_threshold <= 0.0
                                {
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
                        let recv_fut = trigger_rx.recv(cx);
                        let recv_result =
                            crate::runtime_async::timeout_with_cx(cx, wait_step, recv_fut)
                                .await;

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
                                    capture_retry_at = Some(
                                        capture_retry_at.map_or(upgraded_retry, |current| {
                                            current.max(upgraded_retry)
                                        }),
                                    );
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
                                        capture_retry_at = Some(
                                            snapshot_retry_state.retry_deadline(
                                                Instant::now(),
                                                trigger,
                                                reason,
                                            ),
                                        );
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
                                    capture_retry_at = Some(
                                        snapshot_retry_state.retry_deadline(
                                            Instant::now(),
                                            SnapshotTrigger::PeriodicFallback,
                                            reason,
                                        ),
                                    );
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
    /// retried after [`SESSION_CLEANUP_RETRY_DELAY`], while the first successful
    /// receipt ends automatic cleanup for this scheduler invocation. A positive
    /// value reruns every N hours after authoritative success. The DB connection
    /// is opened fresh inside the cleanup engine (a blocking SQLite pipeline run
    /// on the blocking pool). If its authoritative completion is lost, the
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
                    crate::session_retention::cleanup_sessions_from_path(
                        db_path.as_str(),
                        &config,
                    )
                },
            )
            .await
        {
            Ok(result) => {
                schedule.record_authoritative_success(Instant::now());
                let total = result.total_sessions_deleted();
                if total > 0 || result.orphaned_checkpoints > 0 || result.orphaned_pane_states > 0 {
                    tracing::info!(
                        sessions_deleted = total,
                        orphaned_checkpoints = result.orphaned_checkpoints,
                        orphaned_pane_states = result.orphaned_pane_states,
                        explicit_vacuum_attempted = false,
                        expected_default_free_space_policy =
                            "auto_vacuum_none_freelist_reuse",
                        interval_hours,
                        "Session retention cleanup completed"
                    );
                } else {
                    tracing::debug!(
                        interval_hours,
                        "Session retention cleanup: nothing to remove"
                    );
                }
            }
            Err(error) => {
                let reconciliation_latched = self.authority_reconciliation_is_required();
                if error.requires_reconciliation() || reconciliation_latched {
                    tracing::warn!(
                        error = %error,
                        reconciliation_latched,
                        automatic_retry_suppressed = true,
                        "Session retention cleanup outcome is indeterminate; reconcile durable state before restarting cleanup"
                    );
                } else {
                    schedule.defer_retry(Instant::now());
                    tracing::warn!(
                        error = %error,
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
            return Err(self.authority_reconciliation_error(
                SnapshotAuthorityOperation::CheckpointCommit,
            ));
        }
        if cx.is_cancel_requested() {
            tracing::debug!(
                "shutdown_checkpoint_with_cx: Cx pre-cancelled; skipping checkpoint and clean mark"
            );
            return Err(SnapshotError::Cancelled);
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
                    SnapshotCaptureOptions::default(),
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
                    Err(self.authority_reconciliation_error(
                        SnapshotAuthorityOperation::ShutdownMark,
                    ))
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
                    error = %source,
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
        cx.checkpoint().map_err(|_| SnapshotError::Cancelled)?;
        if self.authority_reconciliation_is_required() {
            return Err(self.authority_reconciliation_error(
                SnapshotAuthorityOperation::ShutdownMark,
            ));
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
            return Err(self.authority_reconciliation_error(
                SnapshotAuthorityOperation::ShutdownMark,
            ));
        }
        let id = session_id.ok_or(SnapshotError::ContextFailure)?;
        if id != checkpoint.session_id {
            return Err(SnapshotError::ContextFailure);
        }
        let db_path = Arc::clone(&self.db_path);
        let checkpoint_id = checkpoint.checkpoint_id;
        let checkpoint_at = checkpoint.checkpoint_at;
        let state_hash = checkpoint.state_hash.clone();
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
                )
            },
        )
        .await?;

        reservation.complete()
    }
}

/// Load the most recent detections per pane from storage.
///
/// This is best-effort: if the `events` table does not exist (e.g., tests using a
/// minimal schema), it returns an empty map.
fn load_latest_detections_by_pane_sync(
    db_path: &str,
    pane_ids: &[u64],
    cutoff_ms: i64,
) -> std::result::Result<std::collections::HashMap<u64, Vec<Detection>>, rusqlite::Error> {
    use std::collections::HashMap;

    if pane_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let conn = open_conn(db_path)?;

    let placeholders = std::iter::repeat_n("?", pane_ids.len())
        .collect::<Vec<_>>()
        .join(",");

    let sql = format!(
        "WITH ranked AS (
            SELECT pane_id,
                   rule_id,
                   agent_type,
                   event_type,
                   severity,
                   confidence,
                   extracted,
                   matched_text,
                   ROW_NUMBER() OVER (
                       PARTITION BY pane_id
                       ORDER BY detected_at DESC, id DESC
                   ) AS rn
            FROM events
            WHERE pane_id IN ({placeholders})
              AND detected_at >= ?
              AND agent_type NOT IN ('unknown', 'wezterm')
        )
        SELECT pane_id, rule_id, agent_type, event_type, severity, confidence, extracted, matched_text
        FROM ranked
        WHERE rn = 1"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(err) if is_missing_events_table(&err) => return Ok(HashMap::new()),
        Err(err) => return Err(err),
    };

    let mut params = Vec::with_capacity(pane_ids.len().saturating_add(1));
    for &pane_id in pane_ids {
        params.push(u64_to_sqlite_integer(pane_id)?);
    }
    params.push(cutoff_ms);

    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
    let mut out: HashMap<u64, Vec<Detection>> = HashMap::new();

    while let Some(row) = rows.next()? {
        let pane_id: i64 = row.get(0)?;
        let rule_id: String = row.get(1)?;
        let agent_type: String = row.get(2)?;
        let event_type: String = row.get(3)?;
        let severity: String = row.get(4)?;
        let confidence: f64 = row.get(5)?;
        let extracted: Option<String> = row.get(6)?;
        let matched_text: Option<String> = row.get(7)?;

        let detection = Detection {
            rule_id,
            agent_type: agent_type_from_db(&agent_type),
            event_type,
            severity: severity_from_db(&severity),
            confidence,
            extracted: extracted
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null),
            matched_text: matched_text.unwrap_or_default(),
            span: (0, 0),
        };

        let pane_id = sqlite_integer_to_u64(0, pane_id)?;
        out.insert(pane_id, vec![detection]);
    }

    Ok(out)
}

fn load_latest_scrollback_refs_sync(
    db_path: &str,
    pane_ids: &[u64],
) -> std::result::Result<std::collections::HashMap<u64, ScrollbackRef>, String> {
    use std::collections::HashMap;

    if pane_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let conn = open_conn(db_path).map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT MIN(seq), MAX(seq), COUNT(*), MIN(captured_at), MAX(captured_at)
             FROM output_segments
             WHERE pane_id = ?1",
        )
        .map_err(|error| error.to_string())?;

    let mut refs = HashMap::new();
    for &pane_id in pane_ids {
        let pane_id_i64 = i64::try_from(pane_id)
            .map_err(|_| format!("pane_id {pane_id} exceeds sqlite integer range"))?;
        let (min_seq, max_seq, segment_count, min_capture_at, last_capture_at): (
            Option<i64>,
            Option<i64>,
            i64,
            Option<i64>,
            Option<i64>,
        ) = stmt
            .query_row([pane_id_i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|error| error.to_string())?;

        let Some(min_seq) = min_seq else {
            continue;
        };
        let Some(output_segments_seq) = max_seq else {
            continue;
        };
        let Some(min_capture_at) = min_capture_at else {
            continue;
        };
        let Some(last_capture_at) = last_capture_at else {
            continue;
        };
        if min_seq < 0
            || output_segments_seq < 0
            || segment_count <= 0
            || min_capture_at < 0
            || last_capture_at < 0
        {
            return Err(format!(
                "invalid scrollback segment metadata for pane_id={pane_id}: \
                 min_seq={min_seq}, max_seq={output_segments_seq}, count={segment_count}, \
                 min_capture_at={min_capture_at}, last_capture_at={last_capture_at}"
            ));
        }

        refs.insert(
            pane_id,
            ScrollbackRef {
                output_segments_seq,
                total_lines_captured: u64::try_from(segment_count)
                    .map_err(|error| error.to_string())?,
                last_capture_at: u64::try_from(last_capture_at)
                    .map_err(|error| error.to_string())?,
            },
        );
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

fn severity_from_db(severity: &str) -> Severity {
    match severity {
        "warning" => Severity::Warning,
        "critical" => Severity::Critical,
        _ => Severity::Info,
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
            return Some(SESSION_CLEANUP_RETRY_DELAY - elapsed);
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

#[derive(Debug, thiserror::Error)]
enum SnapshotPreparationError {
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
}

#[derive(Debug, Clone)]
struct PreparedSnapshotPersistence {
    topology_json: String,
    metadata_json: Option<String>,
    panes: Vec<PersistedPaneState>,
    pane_count: usize,
    pane_count_sql: i64,
    total_bytes: usize,
    total_bytes_sql: i64,
    dedup_hash: String,
}

/// Build once from exactly the values the SQLite transaction will insert.
/// Non-persisted process PID/argv, shell, and scrollback line counts are
/// deliberately absent so they cannot manufacture redundant checkpoints.
fn prepare_snapshot_persistence(
    topology: &TopologySnapshot,
    pane_states: &[PaneStateSnapshot],
    metadata: Option<&Value>,
) -> std::result::Result<PreparedSnapshotPersistence, SnapshotPreparationError> {
    let topology_json = canonical_json_string(topology)?;
    let metadata_json = metadata.map(canonical_json_string).transpose()?;
    let mut panes = Vec::with_capacity(pane_states.len());
    let mut total_bytes = 0_usize;

    for pane in pane_states {
        let terminal_state_json = canonical_json_string(&pane.terminal)?;
        let env_json = pane.env.as_ref().map(canonical_json_string).transpose()?;
        let agent_metadata_json = pane
            .agent
            .as_ref()
            .map(canonical_json_string)
            .transpose()?;
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

        let pane_bytes = terminal_state_json
            .len()
            .checked_add(env_json.as_ref().map_or(0, String::len))
            .and_then(|bytes| {
                bytes.checked_add(agent_metadata_json.as_ref().map_or(0, String::len))
            })
            .ok_or(SnapshotPreparationError::ByteCountOverflow)?;
        total_bytes = total_bytes
            .checked_add(pane_bytes)
            .ok_or(SnapshotPreparationError::ByteCountOverflow)?;

        panes.push(PersistedPaneState {
            pane_id: i64::try_from(pane.pane_id)
                .map_err(|_| SnapshotPreparationError::PaneIdRange(pane.pane_id))?,
            cwd: pane.cwd.clone(),
            command: pane
                .foreground_process
                .as_ref()
                .map(|process| process.name.clone()),
            env_json,
            terminal_state_json,
            agent_metadata_json,
            scrollback_checkpoint_seq,
            last_output_at,
        });
    }

    panes.sort_unstable_by_key(|pane| pane.pane_id);
    let pane_count = panes.len();
    let pane_count_sql =
        i64::try_from(pane_count).map_err(|_| SnapshotPreparationError::PaneCountOverflow)?;
    let total_bytes_sql =
        i64::try_from(total_bytes).map_err(|_| SnapshotPreparationError::ByteCountOverflow)?;
    let dedup_hash = snapshot_dedup_witness(&topology_json, &panes)?;

    Ok(PreparedSnapshotPersistence {
        topology_json,
        metadata_json,
        panes,
        pane_count,
        pane_count_sql,
        total_bytes,
        total_bytes_sql,
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
        rollback: rusqlite::Error,
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
                rollback,
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
    F: FnOnce(
        &rusqlite::Transaction<'_>,
    ) -> std::result::Result<Option<T>, rusqlite::Error>,
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
                rollback,
            }),
        },
    }
}

fn u64_to_sqlite_integer(value: u64) -> std::result::Result<i64, rusqlite::Error> {
    i64::try_from(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn usize_to_sqlite_integer(value: usize) -> std::result::Result<i64, rusqlite::Error> {
    i64::try_from(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn sqlite_integer_to_u64(
    column: usize,
    value: i64,
) -> std::result::Result<u64, rusqlite::Error> {
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
fn open_snapshot_query_conn(
    db_path: &str,
) -> std::result::Result<Connection, rusqlite::Error> {
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
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

fn exact_snapshot_checkpoint_exists_sync(
    db_path: &str,
    identity: &SnapshotCheckpointIdentity,
) -> std::result::Result<bool, rusqlite::Error> {
    let checkpoint_at = u64_to_sqlite_integer(identity.checkpoint_at)?;
    let conn = open_snapshot_query_conn(db_path)?;
    conn.query_row(
        "SELECT EXISTS (
             SELECT 1
             FROM session_checkpoints
             WHERE id = ?1
               AND session_id = ?2
               AND checkpoint_at = ?3
               AND checkpoint_role = 'snapshot'
               AND state_hash = ?4
         )",
        rusqlite::params![
            identity.checkpoint_id,
            identity.session_id.as_str(),
            checkpoint_at,
            identity.state_hash.as_str(),
        ],
        |row| row.get(0),
    )
}

fn current_host_id() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_default()
}

/// Creation-only fields inserted alongside a first checkpoint. Keeping this
/// separate from the checkpoint arguments makes it impossible for an existing
/// session capture to accidentally rewrite creation authority.
#[derive(Debug)]
struct NewSessionMetadata {
    ft_version: String,
    host_id: String,
}

/// Authoritative result published only after the SQLite transaction commits.
#[derive(Debug)]
struct CheckpointCommitReceipt {
    session_id: String,
    checkpoint_id: i64,
    state_hash: String,
    total_bytes: usize,
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
    crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(
        &tx,
    )?;
    let inserted = tx.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, host_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session_id,
            now_ms,
            topology_json,
            ft_version,
            host_id
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
) -> std::result::Result<(), SnapshotAuthorityDbError> {
    let checkpoint_at = u64_to_sqlite_integer(checkpoint_at)?;
    let conn = open_conn(db_path).map_err(SnapshotAuthorityDbError::retry_safe)?;
    run_snapshot_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let updated = tx.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 clean_checkpoint_id = ?3
             WHERE session_id = ?1
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
                   ORDER BY latest.checkpoint_at DESC, latest.id DESC
                   LIMIT 1
               )",
            rusqlite::params![session_id, checkpoint_at, checkpoint_id, state_hash],
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
) -> std::result::Result<CheckpointCommitReceipt, SnapshotAuthorityDbError> {
    let now_ms = u64_to_sqlite_integer(now_ms)?;
    let conn = open_conn(db_path)?;

    run_snapshot_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let total_changes_before: i64 =
            tx.query_row("SELECT total_changes()", [], |row| row.get(0))?;

        if let Some(metadata) = new_session {
            let inserted_session = tx.execute(
                "INSERT INTO mux_sessions
                 (session_id, created_at, last_checkpoint_at, topology_json, ft_version, host_id)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    session_id,
                    now_ms,
                    prepared.topology_json.as_str(),
                    metadata.ft_version.as_str(),
                    metadata.host_id.as_str(),
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
                     clean_checkpoint_id = NULL
                 WHERE session_id = ?3",
                rusqlite::params![now_ms, prepared.topology_json.as_str(), session_id],
            )?;
            require_exactly_one_changed_row(updated_session)?;
        }

        // Direct execute counts deliberately exclude trigger side effects. The
        // connection-wide DML delta therefore proves that the transaction changed
        // exactly the checkpoint, its pane rows, and the session authority row.
        // Avoid re-reading every just-written JSON payload here: that doubled
        // large-session I/O and allocation while the SQLite writer lock was held.
        let total_changes_after: i64 =
            tx.query_row("SELECT total_changes()", [], |row| row.get(0))?;
        let expected_changes = prepared
            .pane_count_sql
            .checked_add(3)
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
                 ORDER BY checkpoint.checkpoint_at DESC, checkpoint.id DESC
                 LIMIT 1
             ),
             topology_json = COALESCE(
                 (
                     SELECT checkpoint.topology_json
                     FROM session_checkpoints AS checkpoint
                     WHERE checkpoint.session_id = session.session_id
                       AND checkpoint.checkpoint_role = 'snapshot'
                       AND checkpoint.topology_json IS NOT NULL
                     ORDER BY checkpoint.checkpoint_at DESC, checkpoint.id DESC
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
                            ORDER BY latest.checkpoint_at DESC, latest.id DESC
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
                            ORDER BY latest.checkpoint_at DESC, latest.id DESC
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

type CheckpointDeleteRow = (
    i64,
    String,
    i64,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
);

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
        let projection = "SELECT checkpoint.id,
                                 checkpoint.session_id,
                                 checkpoint.checkpoint_at,
                                 checkpoint.checkpoint_role,
                                 checkpoint.state_hash,
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
                                         ORDER BY latest.checkpoint_at DESC, latest.id DESC
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
                                         ORDER BY latest_snapshot.checkpoint_at DESC,
                                                  latest_snapshot.id DESC
                                         LIMIT 1
                                     ),
                                     0
                                 ),
                                 COALESCE(session.shutdown_clean = 1, 0)
                          FROM session_checkpoints AS checkpoint
                          JOIN mux_sessions AS session
                            ON session.session_id = checkpoint.session_id";
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
                         ORDER BY checkpoint.checkpoint_at DESC, checkpoint.id DESC
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

        let mut affected_stmt = tx.prepare(
            "WITH ranked AS (
                 SELECT session_id,
                        checkpoint_at,
                        ROW_NUMBER() OVER (
                            PARTITION BY session_id
                            ORDER BY checkpoint_at DESC, id DESC
                        ) AS checkpoint_rank
                 FROM session_checkpoints
                 WHERE checkpoint_role = 'snapshot'
             )
             SELECT session_id,
                    MAX(
                        CASE
                            WHEN checkpoint_rank = 1
                             AND (checkpoint_at < ?1 OR checkpoint_rank > ?2)
                            THEN 1
                            ELSE 0
                        END
                    ) AS deletes_latest_snapshot
             FROM ranked
             WHERE checkpoint_at < ?1 OR checkpoint_rank > ?2
             GROUP BY session_id",
        )?;
        let affected_sessions = affected_stmt
            .query_map(rusqlite::params![cutoff_ms, retention_count], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(affected_stmt);

        // Delete checkpoints older than retention_days
        let deleted_by_age: usize = tx.execute(
            "DELETE FROM session_checkpoints
             WHERE checkpoint_role = 'snapshot' AND checkpoint_at < ?1",
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
                                ORDER BY checkpoint_at DESC, id DESC
                            ) AS checkpoint_rank
                     FROM session_checkpoints
                     WHERE checkpoint_role = 'snapshot'
                 )
                 WHERE checkpoint_rank > ?1
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
    fn scrollback_loader_rejects_negative_db_metadata_and_oversized_pane_ids() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap();
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE output_segments (
                 pane_id INTEGER NOT NULL,
                 seq INTEGER NOT NULL,
                 captured_at INTEGER NOT NULL
             );
             INSERT INTO output_segments (pane_id, seq, captured_at)
             VALUES (7, 0, -1), (7, 1, 2);",
        )
        .unwrap();
        drop(conn);

        let negative_timestamp = load_latest_scrollback_refs_sync(db_path, &[7])
            .expect_err("a masked negative captured_at must not wrap to u64");
        assert!(negative_timestamp.contains("min_capture_at=-1"));

        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE output_segments SET captured_at = 1 WHERE pane_id = 7;",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE output_segments SET seq = -1 WHERE pane_id = 7 AND seq = 0",
            [],
        )
        .unwrap();
        drop(conn);
        let negative_sequence = load_latest_scrollback_refs_sync(db_path, &[7])
            .expect_err("a masked negative scrollback sequence must fail closed");
        assert!(negative_sequence.contains("min_seq=-1"));

        let oversized_pane_id = load_latest_scrollback_refs_sync(db_path, &[u64::MAX])
            .expect_err("an unrepresentable pane id must not wrap to a SQLite integer");
        assert!(oversized_pane_id.contains("exceeds sqlite integer range"));
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
        assert!(matches!(
            error,
            rusqlite::Error::StatementChangedRows(0)
        ));
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
        assert!(matches!(
            error,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));

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

        let error = mark_shutdown_sync(
            db_path.as_str(),
            "sess-trigger-shutdown",
            1,
            1,
            "missing",
        )
            .expect_err("an unaudited session trigger must fail before shutdown marking");
        assert!(matches!(
            error,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));

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
            (
                LockAcquireError::Poisoned,
                SnapshotError::LockPoisoned,
            ),
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
            (
                SnapshotAuthorityOperation::ShutdownMark,
                4,
                "shutdown_mark",
            ),
            (
                SnapshotAuthorityOperation::CheckpointDelete,
                5,
                "checkpoint_delete",
            ),
        ];

        for (operation, code, label) in cases {
            assert_eq!(operation.code(), code);
            assert_eq!(operation.as_str(), label);
            assert_eq!(operation.to_string(), label);
            assert_eq!(SnapshotAuthorityOperation::from_code(code), Some(operation));
        }
        assert_eq!(SnapshotAuthorityOperation::from_code(0), None);
        assert_eq!(SnapshotAuthorityOperation::from_code(6), None);
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
                crate::runtime_async::SpawnBlockingWithCxError::CancelledBeforeSpawn {
                    kind: None,
                },
                SnapshotError::Cancelled,
            ),
            (
                crate::runtime_async::SpawnBlockingWithCxError::CancelledMidFlight {
                    kind: None,
                },
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
                first_indeterminate_operation: Some(
                    SnapshotAuthorityOperation::CheckpointCommit
                ),
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
        let outcome = run_authority_work_if_started(&handoff_state, || {
            Ok::<(), SnapshotAuthorityDbError>(())
        });
        assert!(matches!(outcome, AuthorityBlockingOutcome::Executed(Ok(()))));
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
                    rollback: rusqlite::Error::ExecuteReturnedResults,
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
                    engine.try_begin_snapshot_authority(
                        SnapshotAuthorityOperation::ShutdownMark
                    ),
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

        let memory_a = SnapshotEngine::new(
            Arc::new(":memory:".to_owned()),
            SnapshotConfig::default(),
        );
        let memory_b = SnapshotEngine::new(
            Arc::new(":memory:".to_owned()),
            SnapshotConfig::default(),
        );
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
            Arc::new(
                "file:authority-a?mode=memory&cache=shared&vfs=unix-dotfile".to_owned(),
            ),
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
            Arc::new(
                "file:/authority-memdb?mode=memory&cache=private&vfs=memdb".to_owned(),
            ),
            SnapshotConfig::default(),
        );
        let memdb_mode_memory_shared = SnapshotEngine::new(
            Arc::new(
                "file:/authority-memdb?mode=memory&cache=shared&vfs=memdb".to_owned(),
            ),
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
            Arc::new(
                "file:authority-shared-memdb?mode=memory&vfs=memdb&cache=shared".to_owned(),
            ),
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
        let hard_link = path
            .parent()
            .expect("database parent")
            .join("snapshot-authority-hard-link.db");
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
                first_indeterminate_operation: Some(
                    SnapshotAuthorityOperation::CheckpointCommit
                ),
                ..
            })
        ));
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
        assert_eq!(sqlite_windows_uri_drive_path("/C:/data/frankenterm.db"), "C:/data/frankenterm.db");
        assert_eq!(sqlite_windows_uri_drive_path("/z:/data/frankenterm.db"), "z:/data/frankenterm.db");
        assert_eq!(sqlite_windows_uri_drive_path("/D:"), "D:");
        assert_eq!(sqlite_windows_uri_drive_path("/server/share.db"), "/server/share.db");
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
            snapshot_authority_file_identity(
                "file:authority-a?mode=memory&ca%63he=sh%61red"
            ),
            snapshot_authority_file_identity(
                "file:authority-a?mode=memory&cache=shared"
            ),
            "percent-decoded query names and values identify the same shared memory DB"
        );
        assert_ne!(
            snapshot_authority_file_identity(
                "file:authority-a?MODE=memory&CACHE=shared"
            ),
            snapshot_authority_file_identity(
                "file:authority-a?mode=memory&cache=shared"
            ),
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
            snapshot_authority_file_identity(&format!(
                "file:{}",
                percent_path.replace('%', "%25")
            )),
            "literal invalid percent escapes and encoded percent bytes alias like SQLite"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!(
                "file:{}%00ignored",
                db_path.as_str()
            )),
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
            snapshot_authority_file_identity(&format!(
                "file://local%68ost{}",
                db_path.as_str()
            )),
            None,
            "SQLite rejects an encoded spelling of the literal localhost authority"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!("file:{}?vfs=", db_path.as_str())),
            None,
            "bundled SQLite rejects an explicitly empty VFS name"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!(
                "file:{}?mode=bogus",
                db_path.as_str()
            )),
            None,
            "invalid access modes are rejected before SQLite opens a VFS"
        );
        assert_eq!(
            snapshot_authority_file_identity(&format!(
                "file:{}?cache=bogus",
                db_path.as_str()
            )),
            None,
            "invalid cache modes are rejected before SQLite opens a VFS"
        );

        #[cfg(unix)]
        {
            let invalid_ff = snapshot_authority_file_identity(&format!(
                "file:{}/authority%FF.db",
                parent.to_string_lossy()
            ));
            let invalid_ff_alias = snapshot_authority_file_identity(&format!(
                "file:{}/authority%ff.db",
                parent.to_string_lossy()
            ));
            let invalid_fe = snapshot_authority_file_identity(&format!(
                "file:{}/authority%FE.db",
                parent.to_string_lossy()
            ));
            assert_eq!(
                invalid_ff, invalid_ff_alias,
                "hex-digit case cannot split authority for the same non-UTF-8 Unix filename"
            );
            assert_ne!(
                invalid_ff, invalid_fe,
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
            replacement.try_begin_snapshot_authority(
                SnapshotAuthorityOperation::CheckpointCommit
            ),
            Err(SnapshotError::AuthorityReconciliationRequired {
                first_indeterminate_operation: Some(
                    SnapshotAuthorityOperation::CheckpointCleanup
                ),
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
            reservation.complete().expect("terminal lifecycle completion");
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
                .capture_from_provider_with_cx(
                    &cx,
                    &provider,
                    SnapshotTrigger::Periodic,
                )
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
    fn periodic_scheduler_survives_retry_safe_startup_database_failure() {
        run_async_test_isolated(|| async {
            let invalid_db_directory = tempfile::tempdir().expect("temporary directory");
            let db_path = Arc::new(
                invalid_db_directory
                    .path()
                    .to_string_lossy()
                    .into_owned(),
            );
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
            while engine.telemetry().snapshot().captures_attempted == 0
                && Instant::now() < deadline
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
        let before_retry = base
            .checked_add(SESSION_CLEANUP_RETRY_DELAY - Duration::from_nanos(1))
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
            SnapshotError::Serialization("temporary projection failure".to_string())
                .is_retry_safe_scheduler_failure()
        );
        assert!(SnapshotError::BlockingRuntimeFailure.is_retry_safe_scheduler_failure());
        assert!(
            SnapshotError::LockTimedOut { deadline_nanos: 1 }
                .is_retry_safe_scheduler_failure()
        );
        assert!(!SnapshotError::IndeterminateAuthorityMutation {
            operation: SnapshotAuthorityOperation::CheckpointCommit,
        }
        .is_retry_safe_scheduler_failure());
        assert!(!SnapshotError::Cancelled.is_retry_safe_scheduler_failure());
        assert!(!SnapshotError::ContextFailure.is_retry_safe_scheduler_failure());
    }

    #[test]
    fn session_cleanup_reconciliation_latch_is_monotonic() {
        use crate::session_retention::{
            SessionCleanupError, SessionCleanupIndeterminatePhase,
        };

        let engine = SnapshotEngine::new(
            Arc::new(":memory:".to_owned()),
            SnapshotConfig::default(),
        );
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
            use crate::session_retention::{
                SessionCleanupError, SessionCleanupIndeterminatePhase,
            };

            let engine = SnapshotEngine::new(
                Arc::new(":memory:".to_owned()),
                SnapshotConfig::default(),
            );
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

            engine
                .maybe_run_session_cleanup(&cx, &mut schedule)
                .await;

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

            engine
                .maybe_run_session_cleanup(&cx, &mut schedule)
                .await;

            let first_success = schedule
                .last_authoritative_success
                .expect("successful cleanup must advance normal cadence");
            assert!(schedule.retry_deferred_at.is_none());
            assert!(!session_cleanup_due(&schedule, 0, Instant::now()));

            engine
                .maybe_run_session_cleanup(&cx, &mut schedule)
                .await;
            assert_eq!(
                schedule.last_authoritative_success,
                Some(first_success),
                "interval=0 must not run again after authoritative startup success"
            );
        });
    }

    #[test]
    fn session_cleanup_admission_is_exclusive_and_drop_only_releases_scheduler_flag() {
        let engine = SnapshotEngine::new(
            Arc::new(":memory:".to_owned()),
            SnapshotConfig::default(),
        );

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
                .run_periodic_with_cx(
                    &crate::cx::for_testing(),
                    second_shutdown_rx,
                    || async { Ok(vec![make_test_pane(2, 24, 80)]) },
                )
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
                .run_periodic_with_cx(
                    &crate::cx::for_testing(),
                    restart_shutdown_rx,
                    || async { panic!("initially stopped restart must not call provider") },
                )
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
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS mux_sessions (
                session_id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                last_checkpoint_at INTEGER,
                shutdown_clean INTEGER NOT NULL DEFAULT 0,
                topology_json TEXT NOT NULL,
                window_metadata_json TEXT,
                ft_version TEXT NOT NULL,
                host_id TEXT,
                clean_checkpoint_id INTEGER
                    REFERENCES session_checkpoints(id) ON DELETE SET NULL
            );
            CREATE TABLE IF NOT EXISTS session_checkpoints (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                checkpoint_at INTEGER NOT NULL,
                checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
                state_hash TEXT NOT NULL,
                pane_count INTEGER NOT NULL,
                total_bytes INTEGER NOT NULL,
                metadata_json TEXT,
                checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
                    CHECK(checkpoint_role IN ('snapshot','restore_receipt')),
                topology_json TEXT
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
            CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_latest
                ON session_checkpoints(
                    session_id,
                    checkpoint_role,
                    checkpoint_at DESC,
                    id DESC
                );
            PRAGMA foreign_keys = ON;
            ",
        )
        .unwrap();

        (tmp, db_path)
    }

    fn prepare_test_snapshot(
        topology_json: &str,
        panes: &[PaneStateSnapshot],
    ) -> PreparedSnapshotPersistence {
        let mut prepared =
            prepare_snapshot_persistence(&TopologySnapshot::empty(0), panes, None).unwrap();
        let topology: Value = serde_json::from_str(topology_json).unwrap();
        prepared.topology_json = canonical_json_string(&topology).unwrap();
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
                "CREATE TRIGGER abort_first_checkpoint
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

        let mut corrupt_pane =
            PaneStateSnapshot::from_pane_info(&make_test_pane(8, 24, 80), 3000, false);
        corrupt_pane.scrollback_ref = Some(ScrollbackRef {
            output_segments_seq: -1,
            total_lines_captured: 1,
            last_capture_at: 3000,
        });
        let error = prepare_snapshot_persistence(
            &TopologySnapshot::empty(3000),
            &[corrupt_pane],
            None,
        )
        .expect_err("a negative scrollback sequence cannot be prepared");
        assert!(matches!(
            error,
            SnapshotPreparationError::NegativeScrollbackSequence
        ));

        let checkpoint_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            checkpoint_count, 1,
            "invalid metadata must not add a checkpoint"
        );
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
        let first_id = insert_checkpoint_fixture(
            db_path.as_str(),
            "sess-exact-close",
            2_000,
            CHECKPOINT_ROLE_SNAPSHOT,
            "0000000000000001",
            Some(r#"{"version":"first"}"#),
            10,
        );
        let latest_id = insert_checkpoint_fixture(
            db_path.as_str(),
            "sess-exact-close",
            2_000,
            CHECKPOINT_ROLE_SNAPSHOT,
            "0000000000000002",
            Some(r#"{"version":"latest"}"#),
            20,
        );
        set_session_checkpoint_summary(
            db_path.as_str(),
            "sess-exact-close",
            2_000,
            r#"{"version":"latest"}"#,
            None,
        );

        assert!(
            mark_shutdown_sync(
                db_path.as_str(),
                "sess-exact-close",
                first_id,
                2_000,
                "0000000000000001",
            )
            .is_err(),
            "the lower ID in a timestamp tie is not the latest checkpoint"
        );
        assert!(
            mark_shutdown_sync(
                db_path.as_str(),
                "sess-exact-close",
                latest_id,
                2_000,
                "0000000000000001",
            )
            .is_err(),
            "a stale witness must not authorize a clean transition"
        );
        mark_shutdown_sync(
            db_path.as_str(),
            "sess-exact-close",
            latest_id,
            2_000,
            "0000000000000002",
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
                .delete_checkpoint(SnapshotDeleteTarget::Exact(
                    SnapshotCheckpointIdentity {
                        checkpoint_id: old_id,
                        session_id: "sess-delete-old".to_string(),
                        checkpoint_at: 1_000,
                        checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
                        state_hash: "0000000000000004".to_string(),
                    },
                ))
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
    fn checkpoint_delete_exact_identity_defeats_rowid_reuse() {
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
            conn.execute(
                "DELETE FROM session_checkpoints WHERE id = ?1",
                [reused_id],
            )
            .unwrap();
            drop(conn);
            let replacement_id = insert_checkpoint_fixture(
                db_path.as_str(),
                "sess-rowid-reuse",
                1_000,
                CHECKPOINT_ROLE_SNAPSHOT,
                "0000000000000008",
                Some(r#"{"version":"replacement"}"#),
                11,
            );
            assert_eq!(replacement_id, reused_id, "SQLite should reuse the max ROWID");

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
                    [replacement_id],
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

        let mut pane =
            PaneStateSnapshot::from_pane_info(&make_test_pane(9, 24, 80), 2_000, false);
        pane.terminal.title = "終端🙂".to_owned();
        pane.cwd = Some("/ignored/路径".to_owned());
        pane.foreground_process = Some(crate::session_pane_state::ProcessInfo {
            name: "ignored-command".to_owned(),
            pid: Some(42),
            argv: None,
        });
        pane.env = Some(crate::session_pane_state::CapturedEnv {
            vars: std::collections::HashMap::from([(
                "LANG".to_owned(),
                "日本語.UTF-8".to_owned(),
            )]),
            redacted_count: 0,
        });
        pane.agent = Some(crate::session_pane_state::AgentMetadata {
            agent_type: "codex-δ".to_owned(),
            session_id: Some("会話🙂".to_owned()),
            state: Some("working".to_owned()),
        });

        let expected = serde_json::to_string(&pane.terminal).unwrap().len()
            + serde_json::to_string(pane.env.as_ref().unwrap()).unwrap().len()
            + serde_json::to_string(pane.agent.as_ref().unwrap()).unwrap().len();
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
            "CREATE TRIGGER ignore_snapshot_session_update
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
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| {
                row.get(0)
            })
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
        assert!(matches!(
            error,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));

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
        assert!(matches!(
            error,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));

        let conn = Connection::open(db_path.as_str()).unwrap();
        let checkpoint_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        let pane_state_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| {
                row.get(0)
            })
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
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| row.get(0))
                .unwrap();
            let retry_error = engine
                .shutdown_checkpoint(
                    &[make_test_pane(1, 40, 120)],
                    Duration::from_secs(5),
                )
                .await
                .expect_err("a completed shutdown lifecycle cannot capture again");
            assert!(matches!(retry_error, SnapshotError::ShuttingDown));
            let ordinary_capture_error = engine
                .capture(&[make_test_pane(1, 50, 140)], SnapshotTrigger::Manual)
                .await
                .expect_err("ordinary capture remains fenced after clean shutdown");
            assert!(matches!(ordinary_capture_error, SnapshotError::ShuttingDown));
            let checkpoints_after_retry: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| row.get(0))
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
                .capture(
                    &[make_test_pane(1, 24, 80)],
                    SnapshotTrigger::Startup,
                )
                .await
                .expect("capture close receipt");

            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel close_after_checkpoint test"),
            );

            let err = match engine
                .close_after_checkpoint_with_cx(&cx, &receipt)
                .await
            {
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
            total_lines_captured: 500,
            last_capture_at: 150,
        });
        assert_ne!(
            baseline,
            prepare_snapshot_persistence(&topology_at_200, &[scrollback_changed.clone()], None)
                .expect("scrollback-enriched projection")
                .dedup_hash,
            "scrollback authority changes must participate in dedup"
        );
        let total_lines_only = state_at_200.clone().with_scrollback(ScrollbackRef {
            total_lines_captured: 999_999,
            ..scrollback_changed.scrollback_ref.unwrap()
        });
        let same_scrollback_columns = state_at_200.clone().with_scrollback(ScrollbackRef {
            output_segments_seq: 42,
            total_lines_captured: 1,
            last_capture_at: 150,
        });
        assert_eq!(
            prepare_snapshot_persistence(&topology_at_200, &[total_lines_only], None)
                .unwrap()
                .dedup_hash,
            prepare_snapshot_persistence(&topology_at_200, &[same_scrollback_columns], None)
                .unwrap()
                .dedup_hash,
            "non-persisted scrollback line counts must not create checkpoints"
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
            prepare_snapshot_persistence(
                &topology_at_200,
                &[process_ephemera_changed],
                None,
            )
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
        let (topology, _) = TopologySnapshot::from_panes(&[pane.clone()], 100);
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

            let mut rx = engine.trigger_rx.lock().await.take().unwrap();
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

    fn counting_pane_provider()
    -> impl Fn()
        -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = std::result::Result<Vec<PaneInfo>, SnapshotError>,
                    > + Send,
            >,
        >
    + Send
    + Sync
    + 'static {
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
            last_dedup_hash: RwLock::new(None),
            capture_lifecycle: AtomicU8::new(CAPTURE_LIFECYCLE_OPEN_IDLE),
            scheduler_in_progress: AtomicBool::new(false),
            session_cleanup_in_progress: AtomicBool::new(false),
            snapshot_authority: Arc::new(SnapshotAuthorityState::new(None)),
            trigger_tx,
            trigger_rx: Mutex::new(None),
            telemetry: SnapshotEngineTelemetry::new(),
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
    fn intelligent_receiver_already_taken_returns_immediately() {
        run_async_test(async {
            // If run_periodic is called twice, the second call should return
            // immediately because the receiver was already taken.
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
            sleep(Duration::from_millis(100)).await;

            // Second call should return immediately (receiver already taken)
            let (_shutdown_tx2, shutdown_rx2) = watch::channel(false);
            let e3 = engine.clone();
            let result = timeout(Duration::from_secs(2), async move {
                e3.run_periodic(shutdown_rx2, counting_pane_provider())
                    .await
                    .expect("second snapshot scheduler call");
            })
            .await;
            assert!(result.is_ok(), "second run_periodic returns immediately");

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap();
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
    fn severity_from_db_known() {
        assert!(matches!(severity_from_db("warning"), Severity::Warning));
        assert!(matches!(severity_from_db("critical"), Severity::Critical));
        assert!(matches!(severity_from_db("info"), Severity::Info));
    }

    #[test]
    fn severity_from_db_unknown_defaults_to_info() {
        assert!(matches!(severity_from_db(""), Severity::Info));
        assert!(matches!(severity_from_db("debug"), Severity::Info));
        assert!(matches!(severity_from_db("WARNING"), Severity::Info)); // case-sensitive
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
        assert_eq!(SnapshotError::NoPanes.to_string(), "no panes found");
        assert_eq!(
            SnapshotError::NoChanges.to_string(),
            "no changes since last snapshot"
        );
        assert!(
            SnapshotError::PaneList("timeout".into())
                .to_string()
                .contains("timeout")
        );
        assert!(
            SnapshotError::Database("disk full".into())
                .to_string()
                .contains("disk full")
        );
        assert!(
            SnapshotError::Serialization("bad json".into())
                .to_string()
                .contains("bad json")
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
                first_indeterminate_operation: Some(
                    SnapshotAuthorityOperation::CheckpointCommit,
                ),
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
        assert!(!dbg.is_empty());
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

        let db_err = SnapshotError::Database("connection refused".into());
        let dbg2 = format!("{:?}", db_err);
        assert!(
            dbg2.contains("connection refused"),
            "Debug should contain inner message"
        );
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
            trigger: SnapshotTrigger::Manual,
        };
        let cloned = result.clone();
        assert_eq!(cloned.session_id, "sess-test-001");
        assert_eq!(cloned.checkpoint_id, 42);
        assert_eq!(cloned.checkpoint_at, 1_234);
        assert_eq!(cloned.state_hash, "snp2:test");
        assert_eq!(cloned.pane_count, 3);
        assert_eq!(cloned.total_bytes, 1024);
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
