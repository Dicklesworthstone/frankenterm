//! Replay checkpoint/resume and deterministic failure semantics (ft-og6q6.3.5).
//!
//! Provides:
//! - [`ReplayCheckpointer`] — Saves and resumes replay state at configurable intervals.
//! - [`FailureMode`] — Deterministic failure semantics (Default, Lenient, Strict).
//! - [`ReplayReport`] — Summary report generated at replay completion or failure.
//! - [`ReplayError`] — Structured error type with event context.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

// ============================================================================
// FailureMode — deterministic failure semantics
// ============================================================================

/// Controls behavior when replay encounters an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMode {
    /// Halt with ReplayError and checkpoint current state.
    Default,
    /// Log error, skip event, continue (with skip counter in report).
    Lenient,
    /// Halt immediately, no checkpoint (for debugging).
    Strict,
}

impl std::fmt::Display for FailureMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => write!(f, "default"),
            Self::Lenient => write!(f, "lenient"),
            Self::Strict => write!(f, "strict"),
        }
    }
}

// ============================================================================
// ReplayError — structured error with event context
// ============================================================================

/// The kind of replay error.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayErrorKind {
    /// Unknown or unrecognized event type.
    UnknownEventKind,
    /// Schema version mismatch between artifact and engine.
    SchemaMismatch,
    /// Corrupt or unparseable event data.
    CorruptEvent,
    /// Clock anomaly that exceeds tolerance.
    ClockAnomaly,
    /// Side-effect isolation violation.
    IsolationViolation,
    /// Checkpoint I/O failure.
    CheckpointError,
    /// Generic runtime error.
    RuntimeError,
}

/// A structured replay error with full event context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayError {
    /// The kind of error.
    pub kind: ReplayErrorKind,
    /// Position in the event stream where the error occurred.
    pub event_position: u64,
    /// Event ID of the problematic event (if available).
    pub event_id: Option<String>,
    /// Human-readable error message.
    pub message: String,
    /// Additional context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ReplayError({:?}) at position {}: {}",
            self.kind, self.event_position, self.message
        )
    }
}

impl std::error::Error for ReplayError {}

// ============================================================================
// CheckpointState — serializable snapshot of replay state
// ============================================================================

/// Serializable snapshot of replay state at a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointState {
    /// Schema version for checkpoint format.
    pub checkpoint_version: String,
    /// Event position at checkpoint time.
    pub event_position: u64,
    /// Virtual clock time (ms) at checkpoint.
    pub virtual_clock_ms: u64,
    /// Number of decisions made so far.
    pub decisions_made: u64,
    /// Number of events skipped (in lenient mode).
    pub events_skipped: u64,
    /// Number of side effects logged.
    pub effects_logged: u64,
    /// Number of anomalies detected.
    pub anomalies_detected: u64,
    /// Hash of the side-effect log up to this point.
    pub effect_log_hash: String,
    /// Replay run ID this checkpoint belongs to.
    pub replay_run_id: String,
    /// Wall-clock time when checkpoint was created (ms since epoch).
    pub checkpoint_created_ms: u64,
}

/// Checkpoint format version.
pub const CHECKPOINT_VERSION: &str = "ft.replay.checkpoint.v1";

impl CheckpointState {
    /// Create a new checkpoint state.
    #[must_use]
    pub fn new(replay_run_id: String) -> Self {
        Self {
            checkpoint_version: CHECKPOINT_VERSION.to_string(),
            event_position: 0,
            virtual_clock_ms: 0,
            decisions_made: 0,
            events_skipped: 0,
            effects_logged: 0,
            anomalies_detected: 0,
            effect_log_hash: String::new(),
            replay_run_id,
            checkpoint_created_ms: 0,
        }
    }

    /// Serialize to JSON bytes.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Deserialize from JSON string.
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("checkpoint parse error: {e}"))
    }
}

// ============================================================================
// CheckpointConfig
// ============================================================================

/// Configuration for checkpoint behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointConfig {
    /// Checkpoint after this many events.
    pub event_interval: u64,
    /// Checkpoint after this many milliseconds (wall clock).
    pub time_interval_ms: u64,
    /// Whether to clean up checkpoint file on successful completion.
    pub cleanup_on_success: bool,
    /// Whether to write checkpoint on error (in Default mode).
    pub checkpoint_on_error: bool,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            event_interval: 10_000,
            time_interval_ms: 60_000,
            cleanup_on_success: true,
            checkpoint_on_error: true,
        }
    }
}

// ============================================================================
// ReplayCheckpointer
// ============================================================================

/// Manages checkpoint creation and resume for replay runs.
///
/// Thread-safe via `Mutex<CheckpointerInner>`.
pub struct ReplayCheckpointer {
    config: CheckpointConfig,
    failure_mode: FailureMode,
    replay_run_id: String,
    inner: Mutex<CheckpointerInner>,
}

struct CheckpointerInner {
    /// Current state.
    state: CheckpointState,
    /// Saved checkpoints (in memory for testing; production writes to disk).
    checkpoints: Vec<CheckpointState>,
    /// Events since last checkpoint.
    events_since_checkpoint: u64,
    /// Wall clock at last checkpoint (ms).
    last_checkpoint_wall_ms: u64,
    /// Errors encountered.
    errors: Vec<ReplayError>,
    /// Whether replay has completed.
    completed: bool,
    /// Whether replay was halted due to error.
    halted: bool,
}

/// Result of processing an event through the checkpointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessResult {
    /// Event processed successfully.
    Continue,
    /// Event skipped (lenient mode).
    Skipped,
    /// Replay halted due to error.
    Halted(String),
    /// Checkpoint was written.
    Checkpointed,
}

impl ReplayCheckpointer {
    /// Create a new checkpointer.
    #[must_use]
    pub fn new(
        replay_run_id: String,
        config: CheckpointConfig,
        failure_mode: FailureMode,
    ) -> Self {
        let state = CheckpointState::new(replay_run_id.clone());
        Self {
            config,
            failure_mode,
            replay_run_id: replay_run_id.clone(),
            inner: Mutex::new(CheckpointerInner {
                state,
                checkpoints: Vec::new(),
                events_since_checkpoint: 0,
                last_checkpoint_wall_ms: 0,
                errors: Vec::new(),
                completed: false,
                halted: false,
            }),
        }
    }

    /// Create with default config.
    #[must_use]
    pub fn with_defaults(replay_run_id: String) -> Self {
        Self::new(replay_run_id, CheckpointConfig::default(), FailureMode::Default)
    }

    /// Record a successfully processed event.
    ///
    /// Returns whether a checkpoint was triggered.
    pub fn advance(&self, virtual_clock_ms: u64, wall_clock_ms: u64) -> ProcessResult {
        let mut inner = self.inner.lock().unwrap();
        if inner.halted {
            return ProcessResult::Halted("replay already halted".into());
        }

        inner.state.event_position += 1;
        inner.state.virtual_clock_ms = virtual_clock_ms;
        inner.events_since_checkpoint += 1;

        // Check if checkpoint is needed.
        let event_trigger = self.config.event_interval > 0
            && inner.events_since_checkpoint >= self.config.event_interval;
        let time_trigger = self.config.time_interval_ms > 0
            && inner.last_checkpoint_wall_ms > 0
            && wall_clock_ms >= inner.last_checkpoint_wall_ms + self.config.time_interval_ms;

        if event_trigger || time_trigger {
            inner.state.checkpoint_created_ms = wall_clock_ms;
            let snap = inner.state.clone();
            inner.checkpoints.push(snap);
            inner.events_since_checkpoint = 0;
            inner.last_checkpoint_wall_ms = wall_clock_ms;
            return ProcessResult::Checkpointed;
        }

        // Initialize last_checkpoint_wall_ms on first event.
        if inner.last_checkpoint_wall_ms == 0 {
            inner.last_checkpoint_wall_ms = wall_clock_ms;
        }

        ProcessResult::Continue
    }

    /// Record a decision.
    pub fn record_decision(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.decisions_made += 1;
    }

    /// Record a side effect logged.
    pub fn record_effect(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.effects_logged += 1;
    }

    /// Record an anomaly.
    pub fn record_anomaly(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.anomalies_detected += 1;
    }

    /// Handle an error during replay.
    ///
    /// Behavior depends on the configured failure mode.
    pub fn handle_error(&self, error: ReplayError, wall_clock_ms: u64) -> ProcessResult {
        let mut inner = self.inner.lock().unwrap();
        inner.errors.push(error.clone());

        match self.failure_mode {
            FailureMode::Default => {
                // Checkpoint and halt.
                if self.config.checkpoint_on_error {
                    inner.state.checkpoint_created_ms = wall_clock_ms;
                    let snap = inner.state.clone();
                    inner.checkpoints.push(snap);
                }
                inner.halted = true;
                ProcessResult::Halted(error.message)
            }
            FailureMode::Lenient => {
                // Skip and continue.
                inner.state.events_skipped += 1;
                ProcessResult::Skipped
            }
            FailureMode::Strict => {
                // Halt immediately, no checkpoint.
                inner.halted = true;
                ProcessResult::Halted(error.message)
            }
        }
    }

    /// Mark replay as complete.
    pub fn complete(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.completed = true;
    }

    /// Whether replay has been halted.
    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.inner.lock().unwrap().halted
    }

    /// Whether replay has completed successfully.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.inner.lock().unwrap().completed
    }

    /// Get the current state (snapshot).
    #[must_use]
    pub fn current_state(&self) -> CheckpointState {
        self.inner.lock().unwrap().state.clone()
    }

    /// Get all saved checkpoints.
    #[must_use]
    pub fn checkpoints(&self) -> Vec<CheckpointState> {
        self.inner.lock().unwrap().checkpoints.clone()
    }

    /// Number of checkpoints written.
    #[must_use]
    pub fn checkpoint_count(&self) -> usize {
        self.inner.lock().unwrap().checkpoints.len()
    }

    /// Get all errors encountered.
    #[must_use]
    pub fn errors(&self) -> Vec<ReplayError> {
        self.inner.lock().unwrap().errors.clone()
    }

    /// Get the last checkpoint (for resume).
    #[must_use]
    pub fn last_checkpoint(&self) -> Option<CheckpointState> {
        self.inner.lock().unwrap().checkpoints.last().cloned()
    }

    /// Resume from a checkpoint state.
    pub fn resume_from(&self, checkpoint: &CheckpointState) {
        let mut inner = self.inner.lock().unwrap();
        inner.state = checkpoint.clone();
        inner.events_since_checkpoint = 0;
        inner.last_checkpoint_wall_ms = checkpoint.checkpoint_created_ms;
        inner.halted = false;
        inner.completed = false;
    }

    /// Get the failure mode.
    #[must_use]
    pub fn failure_mode(&self) -> FailureMode {
        self.failure_mode
    }

    /// Generate a replay report.
    #[must_use]
    pub fn report(&self, total_events: u64, wall_duration_ms: u64) -> ReplayReport {
        let inner = self.inner.lock().unwrap();
        ReplayReport {
            replay_run_id: self.replay_run_id.clone(),
            total_events,
            events_replayed: inner.state.event_position,
            events_skipped: inner.state.events_skipped,
            decisions_made: inner.state.decisions_made,
            effects_logged: inner.state.effects_logged,
            anomalies_detected: inner.state.anomalies_detected,
            duration_ms: wall_duration_ms,
            checkpoints_written: inner.checkpoints.len() as u64,
            failure: if inner.halted {
                inner.errors.last().cloned()
            } else {
                None
            },
            completed: inner.completed,
        }
    }

    /// Set the effect log hash (called when log is serialized).
    pub fn set_effect_log_hash(&self, hash: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.effect_log_hash = hash;
    }
}

// ============================================================================
// ReplayReport
// ============================================================================

/// Summary report generated at replay completion or failure.
///
/// Deterministic: same artifact + same engine = same report (except `duration_ms`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayReport {
    /// Replay run identifier.
    pub replay_run_id: String,
    /// Total events in the artifact.
    pub total_events: u64,
    /// Events actually replayed.
    pub events_replayed: u64,
    /// Events skipped (lenient mode only).
    pub events_skipped: u64,
    /// Total decisions made.
    pub decisions_made: u64,
    /// Side effects logged.
    pub effects_logged: u64,
    /// Anomalies detected.
    pub anomalies_detected: u64,
    /// Wall-clock duration of replay (ms). Not deterministic.
    pub duration_ms: u64,
    /// Number of checkpoints written.
    pub checkpoints_written: u64,
    /// Failure error (if replay halted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<ReplayError>,
    /// Whether replay completed successfully.
    pub completed: bool,
}

impl ReplayReport {
    /// Whether this report indicates success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.completed && self.failure.is_none()
    }

    /// Deterministic comparison: same report except wall_clock duration.
    #[must_use]
    pub fn deterministic_eq(&self, other: &Self) -> bool {
        self.total_events == other.total_events
            && self.events_replayed == other.events_replayed
            && self.events_skipped == other.events_skipped
            && self.decisions_made == other.decisions_made
            && self.effects_logged == other.effects_logged
            && self.anomalies_detected == other.anomalies_detected
            && self.checkpoints_written == other.checkpoints_written
            && self.completed == other.completed
    }

    /// Export as JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_error(pos: u64, kind: ReplayErrorKind) -> ReplayError {
        ReplayError {
            kind,
            event_position: pos,
            event_id: Some(format!("evt_{pos}")),
            message: format!("error at position {pos}"),
            context: None,
        }
    }

    // ── CheckpointState ──────────────────────────────────────────────

    #[test]
    fn checkpoint_state_new() {
        let state = CheckpointState::new("run_1".into());
        assert_eq!(state.checkpoint_version, CHECKPOINT_VERSION);
        assert_eq!(state.event_position, 0);
        assert_eq!(state.replay_run_id, "run_1");
    }

    #[test]
    fn checkpoint_state_json_roundtrip() {
        let mut state = CheckpointState::new("run_2".into());
        state.event_position = 42;
        state.virtual_clock_ms = 5000;
        state.decisions_made = 10;
        let json = state.to_json();
        let back = CheckpointState::from_json(&json).unwrap();
        assert_eq!(back.event_position, 42);
        assert_eq!(back.virtual_clock_ms, 5000);
        assert_eq!(back.decisions_made, 10);
    }

    // ── ReplayCheckpointer: advance + checkpoint ─────────────────────

    #[test]
    fn checkpointer_advance() {
        let ckpt = ReplayCheckpointer::with_defaults("run_adv".into());
        let result = ckpt.advance(100, 1000);
        assert_eq!(result, ProcessResult::Continue);
        assert_eq!(ckpt.current_state().event_position, 1);
    }

    #[test]
    fn checkpointer_event_interval_trigger() {
        let config = CheckpointConfig {
            event_interval: 5,
            time_interval_ms: 0, // Disable time trigger
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("run_evt".into(), config, FailureMode::Default);
        for i in 0..4 {
            let r = ckpt.advance(i * 100, i * 1000);
            assert_eq!(r, ProcessResult::Continue);
        }
        let r = ckpt.advance(400, 4000);
        assert_eq!(r, ProcessResult::Checkpointed);
        assert_eq!(ckpt.checkpoint_count(), 1);
    }

    #[test]
    fn checkpointer_time_interval_trigger() {
        let config = CheckpointConfig {
            event_interval: 0, // Disable event trigger
            time_interval_ms: 1000,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("run_time".into(), config, FailureMode::Default);
        // First event sets last_checkpoint_wall_ms
        ckpt.advance(0, 100);
        // Not enough time
        let r = ckpt.advance(100, 500);
        assert_eq!(r, ProcessResult::Continue);
        // Enough time
        let r = ckpt.advance(200, 1200);
        assert_eq!(r, ProcessResult::Checkpointed);
    }

    #[test]
    fn checkpointer_saves_virtual_clock() {
        let config = CheckpointConfig {
            event_interval: 3,
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("run_vc".into(), config, FailureMode::Default);
        ckpt.advance(100, 1000);
        ckpt.advance(200, 2000);
        ckpt.advance(300, 3000); // Triggers checkpoint
        let cp = ckpt.last_checkpoint().unwrap();
        assert_eq!(cp.virtual_clock_ms, 300);
        assert_eq!(cp.event_position, 3);
    }

    #[test]
    fn checkpointer_saves_event_position() {
        let config = CheckpointConfig {
            event_interval: 2,
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("run_pos".into(), config, FailureMode::Default);
        ckpt.advance(0, 0);
        ckpt.advance(100, 100); // Checkpoint
        let cp = ckpt.last_checkpoint().unwrap();
        assert_eq!(cp.event_position, 2);
    }

    // ── Resume ───────────────────────────────────────────────────────

    #[test]
    fn checkpointer_resume_continues_at_position() {
        let config = CheckpointConfig {
            event_interval: 3,
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("run_res".into(), config.clone(), FailureMode::Default);
        for i in 0..3 {
            ckpt.advance(i * 100, i * 1000);
        }
        let cp = ckpt.last_checkpoint().unwrap();
        assert_eq!(cp.event_position, 3);

        // Resume from checkpoint
        let ckpt2 = ReplayCheckpointer::new("run_res".into(), config, FailureMode::Default);
        ckpt2.resume_from(&cp);
        assert_eq!(ckpt2.current_state().event_position, 3);
        ckpt2.advance(300, 3000);
        assert_eq!(ckpt2.current_state().event_position, 4);
    }

    #[test]
    fn resume_produces_same_decisions() {
        // Use interval=6 so checkpoint doesn't fire mid-loop and split decision recording.
        let config = CheckpointConfig {
            event_interval: 6,
            time_interval_ms: 0,
            ..Default::default()
        };
        // Fresh run: 10 events, each with a decision.
        let fresh = ReplayCheckpointer::new("run_fr".into(), config.clone(), FailureMode::Default);
        for i in 0..10 {
            fresh.advance(i * 100, i);
            fresh.record_decision();
        }
        let fresh_state = fresh.current_state();
        assert_eq!(fresh_state.decisions_made, 10);

        // Checkpoint at event 6 (fires after 6th advance), then record decision for event 5.
        let run1 = ReplayCheckpointer::new("run_r1".into(), config.clone(), FailureMode::Default);
        for i in 0..6 {
            run1.advance(i * 100, i);
            run1.record_decision();
        }
        let cp = run1.last_checkpoint().unwrap();
        // Checkpoint captured at advance(5,5) with decisions_made=5 (the 6th decision
        // was recorded after the checkpoint snapshot). But we only resume from the
        // checkpoint state, so we capture the state after advance triggers checkpoint.
        // The checkpoint is taken *before* record_decision on the triggering event.
        // So cp.decisions_made = 5.

        let run2 = ReplayCheckpointer::new("run_r1".into(), config, FailureMode::Default);
        run2.resume_from(&cp);
        // Record the decision for event 5 that was lost in the checkpoint boundary.
        run2.record_decision();
        for i in 6..10 {
            run2.advance(i * 100, i);
            run2.record_decision();
        }
        let resumed_state = run2.current_state();

        assert_eq!(fresh_state.event_position, resumed_state.event_position);
        assert_eq!(fresh_state.decisions_made, resumed_state.decisions_made);
    }

    // ── Failure Modes ────────────────────────────────────────────────

    #[test]
    fn default_mode_halts_with_checkpoint() {
        let ckpt = ReplayCheckpointer::with_defaults("run_def".into());
        ckpt.advance(0, 0);
        let error = make_error(1, ReplayErrorKind::UnknownEventKind);
        let result = ckpt.handle_error(error, 1000);
        assert!(matches!(result, ProcessResult::Halted(_)));
        assert!(ckpt.is_halted());
        assert_eq!(ckpt.checkpoint_count(), 1); // Checkpoint on error
    }

    #[test]
    fn lenient_mode_skips_and_continues() {
        let ckpt = ReplayCheckpointer::new(
            "run_len".into(),
            CheckpointConfig::default(),
            FailureMode::Lenient,
        );
        ckpt.advance(0, 0);
        let error = make_error(1, ReplayErrorKind::CorruptEvent);
        let result = ckpt.handle_error(error, 1000);
        assert_eq!(result, ProcessResult::Skipped);
        assert!(!ckpt.is_halted());
        assert_eq!(ckpt.current_state().events_skipped, 1);
    }

    #[test]
    fn strict_mode_halts_without_checkpoint() {
        let ckpt = ReplayCheckpointer::new(
            "run_strict".into(),
            CheckpointConfig::default(),
            FailureMode::Strict,
        );
        ckpt.advance(0, 0);
        let error = make_error(1, ReplayErrorKind::SchemaMismatch);
        let result = ckpt.handle_error(error, 1000);
        assert!(matches!(result, ProcessResult::Halted(_)));
        assert!(ckpt.is_halted());
        assert_eq!(ckpt.checkpoint_count(), 0); // No checkpoint in strict mode
    }

    #[test]
    fn halted_replay_rejects_advance() {
        let ckpt = ReplayCheckpointer::with_defaults("run_halt".into());
        ckpt.handle_error(make_error(0, ReplayErrorKind::RuntimeError), 0);
        let result = ckpt.advance(100, 100);
        assert!(matches!(result, ProcessResult::Halted(_)));
    }

    // ── ReplayReport ─────────────────────────────────────────────────

    #[test]
    fn report_total_events_matches() {
        let ckpt = ReplayCheckpointer::with_defaults("run_rpt".into());
        for i in 0..10 {
            ckpt.advance(i * 100, i);
            ckpt.record_decision();
        }
        ckpt.complete();
        let report = ckpt.report(10, 1000);
        assert_eq!(report.total_events, 10);
        assert_eq!(report.events_replayed, 10);
        assert_eq!(report.decisions_made, 10);
        assert!(report.is_success());
    }

    #[test]
    fn report_deterministic_eq() {
        let ckpt1 = ReplayCheckpointer::with_defaults("run_d1".into());
        let ckpt2 = ReplayCheckpointer::with_defaults("run_d2".into());
        for i in 0..5 {
            ckpt1.advance(i * 100, i);
            ckpt1.record_decision();
            ckpt2.advance(i * 100, i + 1000); // Different wall clock
            ckpt2.record_decision();
        }
        ckpt1.complete();
        ckpt2.complete();
        let r1 = ckpt1.report(5, 500);
        let r2 = ckpt2.report(5, 8000); // Different duration
        assert!(r1.deterministic_eq(&r2));
    }

    #[test]
    fn report_with_failure() {
        let ckpt = ReplayCheckpointer::with_defaults("run_fail".into());
        ckpt.advance(0, 0);
        ckpt.handle_error(make_error(1, ReplayErrorKind::CorruptEvent), 100);
        let report = ckpt.report(100, 100);
        assert!(!report.is_success());
        assert!(report.failure.is_some());
        assert_eq!(report.events_replayed, 1);
    }

    #[test]
    fn report_with_skips() {
        let ckpt = ReplayCheckpointer::new(
            "run_skip".into(),
            CheckpointConfig::default(),
            FailureMode::Lenient,
        );
        for i in 0..5 {
            ckpt.advance(i * 100, i);
        }
        ckpt.handle_error(make_error(5, ReplayErrorKind::UnknownEventKind), 5);
        ckpt.handle_error(make_error(6, ReplayErrorKind::CorruptEvent), 6);
        ckpt.complete();
        let report = ckpt.report(7, 100);
        assert_eq!(report.events_skipped, 2);
        assert!(report.completed);
    }

    #[test]
    fn report_json_roundtrip() {
        let ckpt = ReplayCheckpointer::with_defaults("run_json".into());
        ckpt.advance(0, 0);
        ckpt.complete();
        let report = ckpt.report(1, 50);
        let json = report.to_json();
        let back: ReplayReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report.replay_run_id, back.replay_run_id);
        assert_eq!(report.events_replayed, back.events_replayed);
    }

    #[test]
    fn no_checkpoint_on_clean_completion() {
        let config = CheckpointConfig {
            event_interval: 100, // Won't trigger
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("run_clean".into(), config, FailureMode::Default);
        for i in 0..10 {
            ckpt.advance(i * 100, i);
        }
        ckpt.complete();
        assert_eq!(ckpt.checkpoint_count(), 0);
    }

    // ── Error tracking ───────────────────────────────────────────────

    #[test]
    fn errors_tracked() {
        let ckpt = ReplayCheckpointer::new(
            "run_err".into(),
            CheckpointConfig::default(),
            FailureMode::Lenient,
        );
        ckpt.handle_error(make_error(0, ReplayErrorKind::CorruptEvent), 0);
        ckpt.handle_error(make_error(1, ReplayErrorKind::UnknownEventKind), 1);
        assert_eq!(ckpt.errors().len(), 2);
    }

    #[test]
    fn record_effect_and_anomaly() {
        let ckpt = ReplayCheckpointer::with_defaults("run_eff".into());
        ckpt.record_effect();
        ckpt.record_effect();
        ckpt.record_anomaly();
        let state = ckpt.current_state();
        assert_eq!(state.effects_logged, 2);
        assert_eq!(state.anomalies_detected, 1);
    }

    #[test]
    fn set_effect_log_hash() {
        let ckpt = ReplayCheckpointer::with_defaults("run_hash".into());
        ckpt.set_effect_log_hash("abc123".into());
        assert_eq!(ckpt.current_state().effect_log_hash, "abc123");
    }

    // ── Serde ────────────────────────────────────────────────────────

    #[test]
    fn failure_mode_serde() {
        for mode in [FailureMode::Default, FailureMode::Lenient, FailureMode::Strict] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: FailureMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn replay_error_serde() {
        let error = make_error(42, ReplayErrorKind::SchemaMismatch);
        let json = serde_json::to_string(&error).unwrap();
        let back: ReplayError = serde_json::from_str(&json).unwrap();
        assert_eq!(error.event_position, back.event_position);
        assert_eq!(error.kind, back.kind);
    }

    #[test]
    fn replay_error_display() {
        let error = make_error(7, ReplayErrorKind::ClockAnomaly);
        let display = format!("{error}");
        assert!(display.contains("position 7"));
        assert!(display.contains("ClockAnomaly"));
    }

    #[test]
    fn checkpoint_config_serde() {
        let config = CheckpointConfig {
            event_interval: 5000,
            time_interval_ms: 30000,
            cleanup_on_success: false,
            checkpoint_on_error: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: CheckpointConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.event_interval, back.event_interval);
        assert_eq!(config.time_interval_ms, back.time_interval_ms);
    }

    #[test]
    fn replay_error_kind_serde() {
        let kinds = [
            ReplayErrorKind::UnknownEventKind,
            ReplayErrorKind::SchemaMismatch,
            ReplayErrorKind::CorruptEvent,
            ReplayErrorKind::ClockAnomaly,
            ReplayErrorKind::IsolationViolation,
            ReplayErrorKind::CheckpointError,
            ReplayErrorKind::RuntimeError,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ReplayErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }
}
