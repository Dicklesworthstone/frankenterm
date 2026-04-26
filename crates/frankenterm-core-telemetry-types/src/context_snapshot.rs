//! ARS Context & Duration Snapshotting for regime-shift-aware environment capture.
//!
//! When BOCPD detects a regime shift or OSC 133 signals a command boundary,
//! this module precisely captures the execution environment (CWD, env vars,
//! process info, shell state) and tracks execution duration with μs precision.
//!
//! # Architecture
//!
//! ```text
//! BOCPD change-point ─┐
//!                     ├─► SnapshotTrigger ─► PaneSnapshotManager ─► ContextSnapshot
//! OSC 133 marker ─────┘                           │
//!                                                 ├─► DurationTracker (μs)
//!                                                 └─► snapshot history ring
//! ```
//!
//! # Integration
//!
//! - Consumes `PaneChangePoint` from [`crate::bocpd`]
//! - Consumes `Osc133Marker` / `ShellState` from [`crate::ingest`]
//! - Reuses `CapturedEnv` pattern from [`crate::session_pane_state`]
//!
//! # Performance
//!
//! Snapshot capture targets < 100μs per event. Duration tracking is O(1).

use std::collections::HashMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for context snapshotting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotConfig {
    /// Maximum snapshots retained per pane (ring eviction).
    pub max_snapshots_per_pane: usize,
    /// Maximum total snapshots across all panes.
    pub max_total_snapshots: usize,
    /// Minimum interval between snapshots for the same pane (ms).
    /// Prevents snapshot storms during rapid change-point bursts.
    pub min_interval_ms: u64,
    /// Maximum environment variables to capture per snapshot.
    pub max_env_vars: usize,
    /// Whether to capture environment variables.
    pub capture_env: bool,
    /// BOCPD posterior threshold to trigger a snapshot.
    /// Only change-points with probability >= this value trigger snapshots.
    pub bocpd_threshold: f64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            max_snapshots_per_pane: 64,
            max_total_snapshots: 4096,
            min_interval_ms: 500,
            max_env_vars: 32,
            capture_env: true,
            bocpd_threshold: 0.7,
        }
    }
}

// =============================================================================
// Snapshot trigger
// =============================================================================

/// What triggered a context snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SnapshotTrigger {
    /// BOCPD detected a regime shift.
    BocpdChangePoint {
        /// Observation index at the change-point.
        observation_index: u64,
        /// Posterior probability of the change.
        posterior_probability: f64,
    },
    /// OSC 133 prompt marker boundary.
    Osc133Boundary {
        /// The shell state transition that triggered this.
        transition: ShellTransition,
        /// Exit code if this was a CommandFinished event.
        exit_code: Option<i32>,
    },
    /// Manual/API-triggered snapshot.
    Manual {
        /// Reason for the manual snapshot.
        reason: String,
    },
}

/// Shell state transitions that trigger snapshots (derived from OSC 133).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ShellTransition {
    /// Command started executing (C marker).
    CommandStarted,
    /// Command finished (D marker).
    CommandFinished,
    /// Prompt became active (A marker after D).
    PromptRestored,
}

// =============================================================================
// Duration tracker
// =============================================================================

/// Tracks execution duration between regime boundaries with μs precision.
///
/// Records the start/end of execution phases and computes durations.
/// Uses epoch microseconds (u64) for serialization safety — `Instant` cannot
/// be serialized.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurationTracker {
    /// When this tracker was created (epoch μs).
    pub created_at_us: u64,
    /// When the current phase started (epoch μs).
    pub phase_start_us: u64,
    /// Accumulated durations for completed phases (μs).
    pub completed_phases: Vec<PhaseDuration>,
    /// Maximum phases to retain.
    max_phases: usize,
}

/// Duration of a completed execution phase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhaseDuration {
    /// Phase start time (epoch μs).
    pub start_us: u64,
    /// Phase end time (epoch μs).
    pub end_us: u64,
    /// What ended this phase.
    pub ended_by: PhaseEndReason,
}

/// Why an execution phase ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PhaseEndReason {
    /// BOCPD detected regime shift.
    RegimeShift,
    /// OSC 133 command finished.
    CommandFinished { exit_code: Option<i32> },
    /// Manual/external reset.
    ManualReset,
}

impl PhaseDuration {
    /// Duration in microseconds.
    #[must_use]
    pub fn duration_us(&self) -> u64 {
        self.end_us.saturating_sub(self.start_us)
    }

    /// Duration as f64 seconds.
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        self.duration_us() as f64 / 1_000_000.0
    }
}

impl DurationTracker {
    /// Create a new tracker starting now.
    #[must_use]
    pub fn new(max_phases: usize) -> Self {
        let now = epoch_us();
        Self {
            created_at_us: now,
            phase_start_us: now,
            completed_phases: Vec::new(),
            max_phases,
        }
    }

    /// Create a tracker with a specific start time (for testing).
    #[must_use]
    pub fn with_start(start_us: u64, max_phases: usize) -> Self {
        Self {
            created_at_us: start_us,
            phase_start_us: start_us,
            completed_phases: Vec::new(),
            max_phases,
        }
    }

    /// End the current phase and start a new one.
    ///
    /// Returns the completed phase duration.
    pub fn end_phase(&mut self, reason: PhaseEndReason) -> PhaseDuration {
        self.end_phase_at(epoch_us(), reason)
    }

    /// End the current phase at a specific time (for deterministic testing).
    pub fn end_phase_at(&mut self, now_us: u64, reason: PhaseEndReason) -> PhaseDuration {
        let phase = PhaseDuration {
            start_us: self.phase_start_us,
            end_us: now_us,
            ended_by: reason,
        };

        // Ring eviction: drop oldest if at capacity.
        if self.completed_phases.len() >= self.max_phases {
            self.completed_phases.remove(0);
        }
        self.completed_phases.push(phase.clone());

        // Start new phase.
        self.phase_start_us = now_us;
        phase
    }

    /// Elapsed time in the current phase (μs).
    #[must_use]
    pub fn current_phase_elapsed_us(&self) -> u64 {
        epoch_us().saturating_sub(self.phase_start_us)
    }

    /// Elapsed time at a specific timestamp (for testing).
    #[must_use]
    pub fn current_phase_elapsed_at(&self, now_us: u64) -> u64 {
        now_us.saturating_sub(self.phase_start_us)
    }

    /// Total completed phases.
    #[must_use]
    pub fn phase_count(&self) -> usize {
        self.completed_phases.len()
    }

    /// Mean phase duration across all completed phases (μs).
    #[must_use]
    pub fn mean_duration_us(&self) -> f64 {
        if self.completed_phases.is_empty() {
            return 0.0;
        }
        let sum: u64 = self
            .completed_phases
            .iter()
            .map(PhaseDuration::duration_us)
            .sum();
        sum as f64 / self.completed_phases.len() as f64
    }

    /// p50 phase duration (μs). Returns 0 if no phases.
    #[must_use]
    pub fn p50_duration_us(&self) -> u64 {
        percentile_duration(&self.completed_phases, 50)
    }

    /// p95 phase duration (μs). Returns 0 if no phases.
    #[must_use]
    pub fn p95_duration_us(&self) -> u64 {
        percentile_duration(&self.completed_phases, 95)
    }

    /// p99 phase duration (μs). Returns 0 if no phases.
    #[must_use]
    pub fn p99_duration_us(&self) -> u64 {
        percentile_duration(&self.completed_phases, 99)
    }
}

/// Compute the Nth percentile duration from a list of phases.
fn percentile_duration(phases: &[PhaseDuration], pct: u32) -> u64 {
    if phases.is_empty() {
        return 0;
    }
    let mut durations: Vec<u64> = phases.iter().map(PhaseDuration::duration_us).collect();
    durations.sort_unstable();
    let idx = ((pct as f64 / 100.0) * (durations.len() as f64 - 1.0)).round() as usize;
    durations[idx.min(durations.len() - 1)]
}

// =============================================================================
// Context snapshot
// =============================================================================

/// Schema version for context snapshots.
pub const CONTEXT_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Maximum serialized size target per snapshot (32KB).
pub const CONTEXT_SNAPSHOT_SIZE_BUDGET: usize = 32_768;

/// A complete environment context snapshot at a specific point in time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextSnapshot {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// Unique snapshot ID (monotonic counter per pane).
    pub snapshot_id: u64,
    /// Pane ID this snapshot belongs to.
    pub pane_id: u64,
    /// When this snapshot was captured (epoch μs).
    pub captured_at_us: u64,
    /// What triggered this snapshot.
    pub trigger: SnapshotTrigger,

    /// Current working directory at capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Domain classification (local, SSH, remote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Foreground process info.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<SnapshotProcessInfo>,
    /// Curated + redacted environment variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<SnapshotEnv>,
    /// Terminal dimensions at capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<SnapshotTerminalState>,

    /// Duration of the phase that just ended (μs), if this is a phase-end snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_duration_us: Option<u64>,
    /// BOCPD output features at the time of the snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_features: Option<SnapshotOutputFeatures>,
    /// Correlation ID for tracing this snapshot through the ARS pipeline.
    pub correlation_id: String,
}

/// Process information captured at snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotProcessInfo {
    /// Process name.
    pub name: String,
    /// Process ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// First few command-line arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
}

/// Curated environment variables with redaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotEnv {
    /// Captured variables (only safe ones).
    pub vars: HashMap<String, String>,
    /// Count of redacted (sensitive) variables.
    pub redacted_count: usize,
}

/// Terminal state at snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotTerminalState {
    /// Terminal rows.
    pub rows: u16,
    /// Terminal columns.
    pub cols: u16,
    /// Whether alternate screen is active.
    pub is_alt_screen: bool,
    /// Terminal title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// BOCPD output features serialized into the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotOutputFeatures {
    /// Lines per second.
    pub output_rate: f64,
    /// Bytes per second.
    pub byte_rate: f64,
    /// Shannon entropy (0–8 bits).
    pub entropy: f64,
    /// Unique line ratio (0–1).
    pub unique_line_ratio: f64,
    /// ANSI escape density (0–1).
    pub ansi_density: f64,
}

// =============================================================================
// Environment capture
// =============================================================================

/// Environment variables that are safe to capture in snapshots.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "TERM",
    "LANG",
    "EDITOR",
    "FT_WORKSPACE",
    "FT_OUTPUT_FORMAT",
    "VISUAL",
    "USER",
    "HOSTNAME",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "VIRTUAL_ENV",
    "CONDA_DEFAULT_ENV",
    "RUST_LOG",
    "CARGO_HOME",
    "GOPATH",
    "NODE_PATH",
];

/// Patterns that indicate a sensitive variable.
const SENSITIVE_PATTERNS: &[&str] = &[
    "SECRET",
    "TOKEN",
    "KEY",
    "PASSWORD",
    "CREDENTIAL",
    "AUTH",
    "API_KEY",
    "PRIVATE",
    "PASSWD",
    "BEARER",
    "CERT",
];

/// Capture safe environment variables from a provided map.
///
/// Filters to `SAFE_ENV_VARS` and redacts any matching `SENSITIVE_PATTERNS`.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn capture_env(vars: &HashMap<String, String>, max_vars: usize) -> SnapshotEnv {
    let mut captured = HashMap::new();
    let mut redacted_count = 0;

    for (name, value) in vars {
        let upper = name.to_uppercase();
        if SENSITIVE_PATTERNS.iter().any(|pat| upper.contains(pat)) {
            redacted_count += 1;
            continue;
        }
        if SAFE_ENV_VARS
            .iter()
            .any(|safe| safe.eq_ignore_ascii_case(name))
            && captured.len() < max_vars
        {
            captured.insert(name.clone(), value.clone());
        }
    }

    SnapshotEnv {
        vars: captured,
        redacted_count,
    }
}

// =============================================================================
// Per-pane snapshot manager
// =============================================================================

/// Manages context snapshots for a single pane.
///
/// Integrates BOCPD change-point signals and OSC 133 shell transitions
/// to capture environment context with μs-precision duration tracking.
pub struct PaneSnapshotManager {
    /// Pane ID.
    pub pane_id: u64,
    /// Configuration.
    config: SnapshotConfig,
    /// Duration tracker.
    pub duration: DurationTracker,
    /// Snapshot history (ring buffer — oldest evicted first).
    snapshots: Vec<ContextSnapshot>,
    /// Monotonic snapshot counter.
    next_snapshot_id: u64,
    /// Timestamp of last snapshot (epoch μs) for rate limiting.
    last_snapshot_us: u64,
    /// Current CWD (updated externally).
    current_cwd: Option<String>,
    /// Current domain.
    current_domain: Option<String>,
    /// Current process info.
    current_process: Option<SnapshotProcessInfo>,
    /// Current terminal state.
    current_terminal: Option<SnapshotTerminalState>,
    /// Current environment snapshot.
    current_env: Option<SnapshotEnv>,
}

impl PaneSnapshotManager {
    /// Create a new manager for a specific pane.
    #[must_use]
    pub fn new(pane_id: u64, config: SnapshotConfig) -> Self {
        Self {
            pane_id,
            duration: DurationTracker::new(config.max_snapshots_per_pane),
            snapshots: Vec::new(),
            next_snapshot_id: 0,
            last_snapshot_us: 0,
            current_cwd: None,
            current_domain: None,
            current_process: None,
            current_terminal: None,
            current_env: None,
            config,
        }
    }

    /// Update the current CWD context.
    pub fn set_cwd(&mut self, cwd: String, domain: Option<String>) {
        self.current_cwd = Some(cwd);
        self.current_domain = domain;
    }

    /// Update the current process context.
    pub fn set_process(&mut self, info: SnapshotProcessInfo) {
        self.current_process = Some(info);
    }

    /// Update the current terminal state.
    pub fn set_terminal(&mut self, state: SnapshotTerminalState) {
        self.current_terminal = Some(state);
    }

    /// Update the environment variables.
    #[allow(clippy::implicit_hasher)]
    pub fn set_env(&mut self, vars: &HashMap<String, String>) {
        self.current_env = Some(capture_env(vars, self.config.max_env_vars));
    }

    /// Handle a BOCPD change-point detection.
    ///
    /// Returns the snapshot if one was created (rate limiting may suppress it).
    pub fn on_bocpd_change_point(
        &mut self,
        observation_index: u64,
        posterior_probability: f64,
        features: Option<SnapshotOutputFeatures>,
    ) -> Option<ContextSnapshot> {
        if posterior_probability < self.config.bocpd_threshold {
            trace!(
                pane_id = self.pane_id,
                probability = posterior_probability,
                threshold = self.config.bocpd_threshold,
                "BOCPD change-point below threshold, skipping snapshot"
            );
            return None;
        }

        let now_us = epoch_us();
        if !self.rate_limit_ok(now_us) {
            trace!(pane_id = self.pane_id, "Snapshot rate-limited, skipping");
            return None;
        }

        // End the current duration phase.
        let phase = self
            .duration
            .end_phase_at(now_us, PhaseEndReason::RegimeShift);

        let trigger = SnapshotTrigger::BocpdChangePoint {
            observation_index,
            posterior_probability,
        };

        Some(self.create_snapshot(now_us, trigger, Some(phase.duration_us()), features))
    }

    /// Handle an OSC 133 shell state transition.
    ///
    /// Returns the snapshot if one was created.
    pub fn on_osc133_transition(
        &mut self,
        transition: ShellTransition,
        exit_code: Option<i32>,
    ) -> Option<ContextSnapshot> {
        let now_us = epoch_us();
        if !self.rate_limit_ok(now_us) {
            return None;
        }

        let phase_duration = match transition {
            ShellTransition::CommandFinished => {
                let phase = self
                    .duration
                    .end_phase_at(now_us, PhaseEndReason::CommandFinished { exit_code });
                Some(phase.duration_us())
            }
            ShellTransition::CommandStarted | ShellTransition::PromptRestored => {
                // Start a new phase but don't record duration from the old one
                // (prompt/start transitions are boundaries, not phase-ends).
                self.duration.phase_start_us = now_us;
                None
            }
        };

        let trigger = SnapshotTrigger::Osc133Boundary {
            transition,
            exit_code,
        };

        Some(self.create_snapshot(now_us, trigger, phase_duration, None))
    }

    /// Handle an OSC 133 transition at a specific time (for deterministic testing).
    pub fn on_osc133_transition_at(
        &mut self,
        transition: ShellTransition,
        exit_code: Option<i32>,
        now_us: u64,
    ) -> Option<ContextSnapshot> {
        if !self.rate_limit_ok(now_us) {
            return None;
        }

        let phase_duration = match transition {
            ShellTransition::CommandFinished => {
                let phase = self
                    .duration
                    .end_phase_at(now_us, PhaseEndReason::CommandFinished { exit_code });
                Some(phase.duration_us())
            }
            ShellTransition::CommandStarted | ShellTransition::PromptRestored => {
                self.duration.phase_start_us = now_us;
                None
            }
        };

        let trigger = SnapshotTrigger::Osc133Boundary {
            transition,
            exit_code,
        };

        Some(self.create_snapshot(now_us, trigger, phase_duration, None))
    }

    /// Create a manual snapshot.
    pub fn manual_snapshot(&mut self, reason: String) -> Option<ContextSnapshot> {
        let now_us = epoch_us();
        if !self.rate_limit_ok(now_us) {
            return None;
        }
        let trigger = SnapshotTrigger::Manual { reason };
        Some(self.create_snapshot(now_us, trigger, None, None))
    }

    /// Create a manual snapshot at a specific time (for testing).
    pub fn manual_snapshot_at(&mut self, reason: String, now_us: u64) -> Option<ContextSnapshot> {
        if !self.rate_limit_ok(now_us) {
            return None;
        }
        let trigger = SnapshotTrigger::Manual { reason };
        Some(self.create_snapshot(now_us, trigger, None, None))
    }

    /// Get all retained snapshots (oldest first).
    #[must_use]
    pub fn snapshots(&self) -> &[ContextSnapshot] {
        &self.snapshots
    }

    /// Get the most recent snapshot.
    #[must_use]
    pub fn latest_snapshot(&self) -> Option<&ContextSnapshot> {
        self.snapshots.last()
    }

    /// Number of snapshots retained.
    #[must_use]
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.len()
    }

    /// Total snapshots ever created for this pane.
    #[must_use]
    pub fn total_created(&self) -> u64 {
        self.next_snapshot_id
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    fn rate_limit_ok(&self, now_us: u64) -> bool {
        let min_interval_us = self.config.min_interval_ms * 1000;
        now_us.saturating_sub(self.last_snapshot_us) >= min_interval_us
    }

    fn create_snapshot(
        &mut self,
        now_us: u64,
        trigger: SnapshotTrigger,
        phase_duration_us: Option<u64>,
        output_features: Option<SnapshotOutputFeatures>,
    ) -> ContextSnapshot {
        let snapshot_id = self.next_snapshot_id;
        self.next_snapshot_id += 1;
        self.last_snapshot_us = now_us;

        let correlation_id = format!("ctx-{}-{}-{}", self.pane_id, snapshot_id, now_us);

        let snapshot = ContextSnapshot {
            schema_version: CONTEXT_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id,
            pane_id: self.pane_id,
            captured_at_us: now_us,
            trigger,
            cwd: self.current_cwd.clone(),
            domain: self.current_domain.clone(),
            process: self.current_process.clone(),
            env: if self.config.capture_env {
                self.current_env.clone()
            } else {
                None
            },
            terminal: self.current_terminal.clone(),
            phase_duration_us,
            output_features,
            correlation_id,
        };

        debug!(
            pane_id = self.pane_id,
            snapshot_id = snapshot_id,
            correlation_id = %snapshot.correlation_id,
            "Context snapshot created"
        );

        // Ring eviction.
        if self.snapshots.len() >= self.config.max_snapshots_per_pane {
            self.snapshots.remove(0);
        }
        self.snapshots.push(snapshot.clone());

        snapshot
    }
}

// =============================================================================
// Multi-pane snapshot manager
// =============================================================================

/// Manages context snapshots across all panes.
pub struct SnapshotRegistry {
    /// Per-pane managers.
    panes: HashMap<u64, PaneSnapshotManager>,
    /// Global configuration.
    config: SnapshotConfig,
}

impl SnapshotRegistry {
    /// Create a new registry.
    #[must_use]
    pub fn new(config: SnapshotConfig) -> Self {
        Self {
            panes: HashMap::new(),
            config,
        }
    }

    /// Get or create a per-pane manager.
    pub fn pane_manager(&mut self, pane_id: u64) -> &mut PaneSnapshotManager {
        self.panes
            .entry(pane_id)
            .or_insert_with(|| PaneSnapshotManager::new(pane_id, self.config.clone()))
    }

    /// Get a per-pane manager (read-only).
    #[must_use]
    pub fn get_pane(&self, pane_id: u64) -> Option<&PaneSnapshotManager> {
        self.panes.get(&pane_id)
    }

    /// Remove a pane's manager (pane closed).
    pub fn remove_pane(&mut self, pane_id: u64) -> Option<PaneSnapshotManager> {
        self.panes.remove(&pane_id)
    }

    /// Number of tracked panes.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Total snapshot count across all panes.
    #[must_use]
    pub fn total_snapshot_count(&self) -> u64 {
        self.panes.values().map(|m| m.total_created()).sum()
    }

    /// All pane IDs being tracked.
    #[must_use]
    pub fn pane_ids(&self) -> Vec<u64> {
        self.panes.keys().copied().collect()
    }

    /// Registry-level summary snapshot.
    #[must_use]
    pub fn summary(&self) -> RegistrySummary {
        let pane_summaries: Vec<PaneSnapshotSummary> = self
            .panes
            .iter()
            .map(|(&pane_id, mgr)| PaneSnapshotSummary {
                pane_id,
                snapshot_count: mgr.snapshot_count() as u64,
                total_created: mgr.total_created(),
                mean_phase_duration_us: mgr.duration.mean_duration_us(),
                p50_phase_duration_us: mgr.duration.p50_duration_us(),
                p95_phase_duration_us: mgr.duration.p95_duration_us(),
            })
            .collect();

        RegistrySummary {
            pane_count: self.panes.len() as u64,
            total_snapshots: self.total_snapshot_count(),
            panes: pane_summaries,
        }
    }
}

/// Summary of the entire snapshot registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrySummary {
    /// Number of tracked panes.
    pub pane_count: u64,
    /// Total snapshots created.
    pub total_snapshots: u64,
    /// Per-pane summaries.
    pub panes: Vec<PaneSnapshotSummary>,
}

/// Summary of a single pane's snapshot state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSnapshotSummary {
    /// Pane ID.
    pub pane_id: u64,
    /// Currently retained snapshots.
    pub snapshot_count: u64,
    /// Total ever created.
    pub total_created: u64,
    /// Mean phase duration (μs).
    pub mean_phase_duration_us: f64,
    /// p50 phase duration (μs).
    pub p50_phase_duration_us: u64,
    /// p95 phase duration (μs).
    pub p95_phase_duration_us: u64,
}

// =============================================================================
// Utility
// =============================================================================

/// Current time as epoch microseconds.
fn epoch_us() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SnapshotConfig {
        SnapshotConfig {
            max_snapshots_per_pane: 8,
            max_total_snapshots: 64,
            min_interval_ms: 0, // Disable rate limiting for tests
            max_env_vars: 16,
            capture_env: true,
            bocpd_threshold: 0.7,
        }
    }

    fn base_us() -> u64 {
        1_700_000_000_000_000 // ~2023 epoch μs
    }

    // -------------------------------------------------------------------------
    // SnapshotConfig defaults
    // -------------------------------------------------------------------------

    #[test]
    fn config_default_values() {
        let cfg = SnapshotConfig::default();
        assert_eq!(cfg.max_snapshots_per_pane, 64);
        assert_eq!(cfg.max_total_snapshots, 4096);
        assert_eq!(cfg.min_interval_ms, 500);
        assert_eq!(cfg.max_env_vars, 32);
        assert!(cfg.capture_env);
        assert!((cfg.bocpd_threshold - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = SnapshotConfig {
            max_snapshots_per_pane: 32,
            min_interval_ms: 100,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: SnapshotConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.max_snapshots_per_pane, 32);
        assert_eq!(decoded.min_interval_ms, 100);
    }

    // -------------------------------------------------------------------------
    // DurationTracker
    // -------------------------------------------------------------------------

    #[test]
    fn duration_tracker_new() {
        let tracker = DurationTracker::with_start(base_us(), 16);
        assert_eq!(tracker.phase_count(), 0);
        assert_eq!(tracker.created_at_us, base_us());
        assert_eq!(tracker.phase_start_us, base_us());
    }

    #[test]
    fn duration_tracker_end_phase() {
        let mut tracker = DurationTracker::with_start(base_us(), 16);
        let phase = tracker.end_phase_at(base_us() + 5_000_000, PhaseEndReason::RegimeShift);
        assert_eq!(phase.duration_us(), 5_000_000);
        assert_eq!(phase.start_us, base_us());
        assert_eq!(phase.end_us, base_us() + 5_000_000);
        assert_eq!(tracker.phase_count(), 1);
        assert_eq!(tracker.phase_start_us, base_us() + 5_000_000);
    }

    #[test]
    fn duration_tracker_multiple_phases() {
        let mut tracker = DurationTracker::with_start(0, 16);
        tracker.end_phase_at(100, PhaseEndReason::RegimeShift);
        tracker.end_phase_at(350, PhaseEndReason::CommandFinished { exit_code: Some(0) });
        tracker.end_phase_at(600, PhaseEndReason::ManualReset);
        assert_eq!(tracker.phase_count(), 3);
        // Durations: 100, 250, 250
        assert!((tracker.mean_duration_us() - 200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn duration_tracker_ring_eviction() {
        let mut tracker = DurationTracker::with_start(0, 3);
        for i in 1..=5 {
            tracker.end_phase_at(i * 100, PhaseEndReason::RegimeShift);
        }
        // Max 3 phases, should have last 3.
        assert_eq!(tracker.phase_count(), 3);
        assert_eq!(tracker.completed_phases[0].start_us, 200);
        assert_eq!(tracker.completed_phases[0].end_us, 300);
    }

    #[test]
    fn duration_tracker_percentiles_empty() {
        let tracker = DurationTracker::with_start(0, 16);
        assert_eq!(tracker.p50_duration_us(), 0);
        assert_eq!(tracker.p95_duration_us(), 0);
        assert_eq!(tracker.p99_duration_us(), 0);
        assert!((tracker.mean_duration_us() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn duration_tracker_percentiles_single() {
        let mut tracker = DurationTracker::with_start(0, 16);
        tracker.end_phase_at(1000, PhaseEndReason::RegimeShift);
        assert_eq!(tracker.p50_duration_us(), 1000);
        assert_eq!(tracker.p95_duration_us(), 1000);
        assert_eq!(tracker.p99_duration_us(), 1000);
    }

    #[test]
    fn duration_tracker_percentiles_varied() {
        let mut tracker = DurationTracker::with_start(0, 100);
        // Create 10 phases with durations 100, 200, ..., 1000
        let mut t = 0;
        for i in 1..=10 {
            t += i * 100;
            tracker.end_phase_at(t, PhaseEndReason::RegimeShift);
        }
        let p50 = tracker.p50_duration_us();
        let p95 = tracker.p95_duration_us();
        let p99 = tracker.p99_duration_us();
        // p50 should be around median, p95/p99 near max
        assert!(p50 > 0);
        assert!(p95 >= p50);
        assert!(p99 >= p95);
    }

    #[test]
    fn duration_tracker_current_phase_elapsed() {
        let tracker = DurationTracker::with_start(1000, 16);
        assert_eq!(tracker.current_phase_elapsed_at(2500), 1500);
        assert_eq!(tracker.current_phase_elapsed_at(1000), 0);
        // Underflow safety
        assert_eq!(tracker.current_phase_elapsed_at(500), 0);
    }

    #[test]
    fn phase_duration_secs() {
        let phase = PhaseDuration {
            start_us: 0,
            end_us: 2_500_000,
            ended_by: PhaseEndReason::RegimeShift,
        };
        assert!((phase.duration_secs() - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn phase_duration_saturating() {
        let phase = PhaseDuration {
            start_us: 100,
            end_us: 50,
            ended_by: PhaseEndReason::RegimeShift,
        };
        assert_eq!(phase.duration_us(), 0);
    }

    // -------------------------------------------------------------------------
    // Environment capture
    // -------------------------------------------------------------------------

    #[test]
    fn capture_env_safe_vars() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        vars.insert("SHELL".to_string(), "/bin/bash".to_string());
        vars.insert("RANDOM_VAR".to_string(), "ignored".to_string());
        let env = capture_env(&vars, 32);
        assert_eq!(env.vars.len(), 2);
        assert_eq!(env.vars["HOME"], "/home/user");
        assert_eq!(env.vars["SHELL"], "/bin/bash");
        assert_eq!(env.redacted_count, 0);
    }

    #[test]
    fn capture_env_redacts_sensitive() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        vars.insert("API_KEY".to_string(), "sk-secret".to_string());
        vars.insert("AWS_SECRET_ACCESS_KEY".to_string(), "hidden".to_string());
        let env = capture_env(&vars, 32);
        assert_eq!(env.vars.len(), 1);
        assert!(env.vars.contains_key("HOME"));
        assert!(!env.vars.contains_key("API_KEY"));
        assert_eq!(env.redacted_count, 2);
    }

    #[test]
    fn capture_env_max_vars_limit() {
        let mut vars = HashMap::new();
        for name in SAFE_ENV_VARS {
            vars.insert(name.to_string(), "value".to_string());
        }
        let env = capture_env(&vars, 3);
        assert_eq!(env.vars.len(), 3);
    }

    #[test]
    fn capture_env_empty_input() {
        let vars = HashMap::new();
        let env = capture_env(&vars, 32);
        assert!(env.vars.is_empty());
        assert_eq!(env.redacted_count, 0);
    }

    #[test]
    fn capture_env_case_insensitive_safe() {
        let mut vars = HashMap::new();
        vars.insert("home".to_string(), "/home/user".to_string());
        let env = capture_env(&vars, 32);
        assert_eq!(env.vars.len(), 1);
    }

    #[test]
    fn capture_env_sensitive_in_safe_name() {
        // A var named "PATH_TOKEN" should be redacted because it contains TOKEN
        let mut vars = HashMap::new();
        vars.insert("PATH_TOKEN".to_string(), "secret".to_string());
        let env = capture_env(&vars, 32);
        assert_eq!(env.vars.len(), 0);
        assert_eq!(env.redacted_count, 1);
    }

    // -------------------------------------------------------------------------
    // SnapshotTrigger serde
    // -------------------------------------------------------------------------

    #[test]
    fn trigger_bocpd_serde() {
        let trigger = SnapshotTrigger::BocpdChangePoint {
            observation_index: 42,
            posterior_probability: 0.85,
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let decoded: SnapshotTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, trigger);
    }

    #[test]
    fn trigger_osc133_serde() {
        let trigger = SnapshotTrigger::Osc133Boundary {
            transition: ShellTransition::CommandFinished,
            exit_code: Some(1),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let decoded: SnapshotTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, trigger);
    }

    #[test]
    fn trigger_manual_serde() {
        let trigger = SnapshotTrigger::Manual {
            reason: "debug capture".to_string(),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let decoded: SnapshotTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, trigger);
    }

    // -------------------------------------------------------------------------
    // PaneSnapshotManager — BOCPD integration
    // -------------------------------------------------------------------------

    #[test]
    fn manager_bocpd_creates_snapshot() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        mgr.set_cwd("/project".to_string(), Some("local".to_string()));

        let snap = mgr.on_bocpd_change_point(10, 0.9, None);
        assert!(snap.is_some());
        let snap = snap.unwrap();
        assert_eq!(snap.pane_id, 0);
        assert_eq!(snap.snapshot_id, 0);
        assert_eq!(snap.cwd.as_deref(), Some("/project"));
        assert!(snap.phase_duration_us.is_some());
        let is_bocpd = matches!(snap.trigger, SnapshotTrigger::BocpdChangePoint { .. });
        assert!(is_bocpd);
    }

    #[test]
    fn manager_bocpd_below_threshold_no_snapshot() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        let snap = mgr.on_bocpd_change_point(10, 0.5, None);
        assert!(snap.is_none());
        assert_eq!(mgr.snapshot_count(), 0);
    }

    #[test]
    fn manager_bocpd_with_features() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        let features = SnapshotOutputFeatures {
            output_rate: 100.0,
            byte_rate: 5000.0,
            entropy: 4.5,
            unique_line_ratio: 0.8,
            ansi_density: 0.1,
        };
        let snap = mgr.on_bocpd_change_point(5, 0.95, Some(features)).unwrap();
        assert!(snap.output_features.is_some());
        let f = snap.output_features.unwrap();
        assert!((f.output_rate - 100.0).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // PaneSnapshotManager — OSC 133 integration
    // -------------------------------------------------------------------------

    #[test]
    fn manager_osc133_command_finished() {
        let mut mgr = PaneSnapshotManager::new(1, test_config());
        let snap = mgr.on_osc133_transition(ShellTransition::CommandFinished, Some(0));
        assert!(snap.is_some());
        let snap = snap.unwrap();
        assert_eq!(snap.pane_id, 1);
        let is_osc = matches!(
            snap.trigger,
            SnapshotTrigger::Osc133Boundary {
                transition: ShellTransition::CommandFinished,
                exit_code: Some(0),
            }
        );
        assert!(is_osc);
        assert!(snap.phase_duration_us.is_some());
    }

    #[test]
    fn manager_osc133_command_started() {
        let mut mgr = PaneSnapshotManager::new(2, test_config());
        let snap = mgr.on_osc133_transition(ShellTransition::CommandStarted, None);
        assert!(snap.is_some());
        let snap = snap.unwrap();
        // CommandStarted does not end a phase.
        assert!(snap.phase_duration_us.is_none());
    }

    #[test]
    fn manager_osc133_prompt_restored() {
        let mut mgr = PaneSnapshotManager::new(3, test_config());
        let snap = mgr.on_osc133_transition(ShellTransition::PromptRestored, None);
        assert!(snap.is_some());
        let snap = snap.unwrap();
        assert!(snap.phase_duration_us.is_none());
    }

    #[test]
    fn manager_osc133_deterministic_at() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        mgr.duration = DurationTracker::with_start(1000, 64);

        let snap = mgr
            .on_osc133_transition_at(ShellTransition::CommandFinished, Some(42), 5000)
            .unwrap();
        assert_eq!(snap.phase_duration_us, Some(4000));
    }

    // -------------------------------------------------------------------------
    // PaneSnapshotManager — manual snapshots
    // -------------------------------------------------------------------------

    #[test]
    fn manager_manual_snapshot() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        mgr.set_cwd("/tmp".to_string(), None);
        let snap = mgr.manual_snapshot("test".to_string()).unwrap();
        assert_eq!(snap.cwd.as_deref(), Some("/tmp"));
        let is_manual = matches!(snap.trigger, SnapshotTrigger::Manual { .. });
        assert!(is_manual);
        assert!(snap.phase_duration_us.is_none());
    }

    #[test]
    fn manager_manual_snapshot_at() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        let snap = mgr.manual_snapshot_at("reason".to_string(), 9999).unwrap();
        assert_eq!(snap.captured_at_us, 9999);
    }

    // -------------------------------------------------------------------------
    // PaneSnapshotManager — rate limiting
    // -------------------------------------------------------------------------

    #[test]
    fn manager_rate_limiting() {
        let config = SnapshotConfig {
            min_interval_ms: 1000, // 1 second
            ..test_config()
        };
        let mut mgr = PaneSnapshotManager::new(0, config);

        // First snapshot should succeed.
        let snap1 = mgr.manual_snapshot_at("first".to_string(), 1_000_000);
        assert!(snap1.is_some());

        // Second within 1 second should be rate-limited.
        let snap2 = mgr.manual_snapshot_at("second".to_string(), 1_500_000);
        assert!(snap2.is_none());

        // After 1 second, should succeed.
        let snap3 = mgr.manual_snapshot_at("third".to_string(), 2_000_000);
        assert!(snap3.is_some());
    }

    // -------------------------------------------------------------------------
    // PaneSnapshotManager — ring eviction
    // -------------------------------------------------------------------------

    #[test]
    fn manager_ring_eviction() {
        let config = SnapshotConfig {
            max_snapshots_per_pane: 3,
            ..test_config()
        };
        let mut mgr = PaneSnapshotManager::new(0, config);

        for i in 0..5u64 {
            mgr.manual_snapshot_at(format!("snap-{i}"), i * 1000);
        }
        assert_eq!(mgr.snapshot_count(), 3);
        assert_eq!(mgr.total_created(), 5);
        // Oldest retained should be snap-2.
        assert_eq!(mgr.snapshots()[0].snapshot_id, 2);
    }

    // -------------------------------------------------------------------------
    // PaneSnapshotManager — context updates
    // -------------------------------------------------------------------------

    #[test]
    fn manager_context_updates_propagate() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        mgr.set_cwd("/a".to_string(), Some("local".to_string()));
        mgr.set_process(SnapshotProcessInfo {
            name: "claude-code".to_string(),
            pid: Some(1234),
            argv: Some(vec!["--model".to_string(), "opus".to_string()]),
        });
        mgr.set_terminal(SnapshotTerminalState {
            rows: 40,
            cols: 120,
            is_alt_screen: false,
            title: Some("test".to_string()),
        });

        let mut env_vars = HashMap::new();
        env_vars.insert("HOME".to_string(), "/home/u".to_string());
        mgr.set_env(&env_vars);

        let snap = mgr.manual_snapshot_at("test".to_string(), 1000).unwrap();
        assert_eq!(snap.cwd.as_deref(), Some("/a"));
        assert_eq!(snap.domain.as_deref(), Some("local"));
        assert_eq!(snap.process.as_ref().unwrap().name, "claude-code");
        assert_eq!(snap.process.as_ref().unwrap().pid, Some(1234));
        assert_eq!(snap.terminal.as_ref().unwrap().rows, 40);
        assert_eq!(snap.env.as_ref().unwrap().vars["HOME"], "/home/u");
    }

    #[test]
    fn manager_env_disabled() {
        let config = SnapshotConfig {
            capture_env: false,
            ..test_config()
        };
        let mut mgr = PaneSnapshotManager::new(0, config);

        let mut env_vars = HashMap::new();
        env_vars.insert("HOME".to_string(), "/home/u".to_string());
        mgr.set_env(&env_vars);

        let snap = mgr.manual_snapshot_at("test".to_string(), 1000).unwrap();
        assert!(snap.env.is_none());
    }

    // -------------------------------------------------------------------------
    // ContextSnapshot serialization
    // -------------------------------------------------------------------------

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = ContextSnapshot {
            schema_version: CONTEXT_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: 42,
            pane_id: 7,
            captured_at_us: 1_700_000_000_000_000,
            trigger: SnapshotTrigger::BocpdChangePoint {
                observation_index: 100,
                posterior_probability: 0.92,
            },
            cwd: Some("/project".to_string()),
            domain: Some("local".to_string()),
            process: Some(SnapshotProcessInfo {
                name: "bash".to_string(),
                pid: Some(5678),
                argv: None,
            }),
            env: Some(SnapshotEnv {
                vars: {
                    let mut m = HashMap::new();
                    m.insert("HOME".to_string(), "/home/u".to_string());
                    m
                },
                redacted_count: 1,
            }),
            terminal: Some(SnapshotTerminalState {
                rows: 24,
                cols: 80,
                is_alt_screen: false,
                title: Some("test".to_string()),
            }),
            phase_duration_us: Some(5_000_000),
            output_features: Some(SnapshotOutputFeatures {
                output_rate: 50.0,
                byte_rate: 2500.0,
                entropy: 3.5,
                unique_line_ratio: 0.9,
                ansi_density: 0.05,
            }),
            correlation_id: "ctx-7-42-1700000000000000".to_string(),
        };

        let json = serde_json::to_string_pretty(&snap).unwrap();
        let decoded: ContextSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.snapshot_id, snap.snapshot_id);
        assert_eq!(decoded.pane_id, snap.pane_id);
        assert_eq!(decoded.cwd, snap.cwd);
        assert_eq!(decoded.domain, snap.domain);
        assert_eq!(decoded.process, snap.process);
        assert_eq!(decoded.env, snap.env);
        assert_eq!(decoded.terminal, snap.terminal);
        assert_eq!(decoded.phase_duration_us, snap.phase_duration_us);
        assert_eq!(decoded.correlation_id, snap.correlation_id);
    }

    #[test]
    fn snapshot_size_budget() {
        let snap = ContextSnapshot {
            schema_version: CONTEXT_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: 0,
            pane_id: 0,
            captured_at_us: base_us(),
            trigger: SnapshotTrigger::BocpdChangePoint {
                observation_index: 999,
                posterior_probability: 0.99,
            },
            cwd: Some("/very/long/path/to/some/deeply/nested/project/directory".to_string()),
            domain: Some("SSH:remote-host.example.com".to_string()),
            process: Some(SnapshotProcessInfo {
                name: "claude-code".to_string(),
                pid: Some(99999),
                argv: Some(vec![
                    "--model".to_string(),
                    "opus-4".to_string(),
                    "--verbose".to_string(),
                ]),
            }),
            env: Some(SnapshotEnv {
                vars: {
                    let mut m = HashMap::new();
                    for name in SAFE_ENV_VARS.iter().take(16) {
                        m.insert(name.to_string(), "some_value_here_abcdef".to_string());
                    }
                    m
                },
                redacted_count: 5,
            }),
            terminal: Some(SnapshotTerminalState {
                rows: 50,
                cols: 200,
                is_alt_screen: true,
                title: Some("very long terminal title for testing".to_string()),
            }),
            phase_duration_us: Some(60_000_000),
            output_features: Some(SnapshotOutputFeatures {
                output_rate: 150.0,
                byte_rate: 7500.0,
                entropy: 6.2,
                unique_line_ratio: 0.75,
                ansi_density: 0.15,
            }),
            correlation_id: "ctx-0-0-1700000000000000".to_string(),
        };

        let serialized = serde_json::to_string(&snap).unwrap();
        assert!(
            serialized.len() < CONTEXT_SNAPSHOT_SIZE_BUDGET,
            "Snapshot size {} exceeds budget {}",
            serialized.len(),
            CONTEXT_SNAPSHOT_SIZE_BUDGET
        );
    }

    #[test]
    fn snapshot_optional_fields_skipped() {
        let snap = ContextSnapshot {
            schema_version: CONTEXT_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: 0,
            pane_id: 0,
            captured_at_us: 0,
            trigger: SnapshotTrigger::Manual {
                reason: "test".to_string(),
            },
            cwd: None,
            domain: None,
            process: None,
            env: None,
            terminal: None,
            phase_duration_us: None,
            output_features: None,
            correlation_id: "ctx-0-0-0".to_string(),
        };

        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("\"cwd\""));
        assert!(!json.contains("\"domain\""));
        assert!(!json.contains("\"process\""));
        assert!(!json.contains("\"env\""));
        assert!(!json.contains("\"terminal\""));
        assert!(!json.contains("\"phase_duration_us\""));
        assert!(!json.contains("\"output_features\""));
    }

    // -------------------------------------------------------------------------
    // SnapshotRegistry
    // -------------------------------------------------------------------------

    #[test]
    fn registry_new_empty() {
        let registry = SnapshotRegistry::new(test_config());
        assert_eq!(registry.pane_count(), 0);
        assert_eq!(registry.total_snapshot_count(), 0);
    }

    #[test]
    fn registry_pane_manager_creates_on_access() {
        let mut registry = SnapshotRegistry::new(test_config());
        let mgr = registry.pane_manager(42);
        assert_eq!(mgr.pane_id, 42);
        assert_eq!(registry.pane_count(), 1);
    }

    #[test]
    fn registry_get_pane_readonly() {
        let mut registry = SnapshotRegistry::new(test_config());
        assert!(registry.get_pane(42).is_none());
        registry.pane_manager(42);
        assert!(registry.get_pane(42).is_some());
    }

    #[test]
    fn registry_remove_pane() {
        let mut registry = SnapshotRegistry::new(test_config());
        registry.pane_manager(42);
        let removed = registry.remove_pane(42);
        assert!(removed.is_some());
        assert_eq!(registry.pane_count(), 0);
        assert!(registry.remove_pane(42).is_none());
    }

    #[test]
    fn registry_multi_pane_tracking() {
        let mut registry = SnapshotRegistry::new(test_config());
        registry.pane_manager(0);
        registry.pane_manager(1);
        registry.pane_manager(2);

        assert_eq!(registry.pane_count(), 3);
        let mut ids = registry.pane_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn registry_summary() {
        let mut registry = SnapshotRegistry::new(test_config());
        let mgr = registry.pane_manager(0);
        mgr.manual_snapshot_at("s1".to_string(), 1000);
        mgr.manual_snapshot_at("s2".to_string(), 2000);

        let mgr2 = registry.pane_manager(1);
        mgr2.manual_snapshot_at("s3".to_string(), 3000);

        let summary = registry.summary();
        assert_eq!(summary.pane_count, 2);
        assert_eq!(summary.total_snapshots, 3);
        assert_eq!(summary.panes.len(), 2);
    }

    // -------------------------------------------------------------------------
    // Correlation ID format
    // -------------------------------------------------------------------------

    #[test]
    fn correlation_id_format() {
        let mut mgr = PaneSnapshotManager::new(7, test_config());
        let snap = mgr.manual_snapshot_at("test".to_string(), 12345).unwrap();
        assert!(snap.correlation_id.starts_with("ctx-7-0-"));
        assert!(snap.correlation_id.contains("12345"));
    }

    #[test]
    fn correlation_id_increments() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        let s1 = mgr.manual_snapshot_at("a".to_string(), 100).unwrap();
        let s2 = mgr.manual_snapshot_at("b".to_string(), 200).unwrap();
        assert_ne!(s1.correlation_id, s2.correlation_id);
        assert!(s1.correlation_id.contains("-0-"));
        assert!(s2.correlation_id.contains("-1-"));
    }

    // -------------------------------------------------------------------------
    // SnapshotOutputFeatures
    // -------------------------------------------------------------------------

    #[test]
    fn output_features_serde() {
        let features = SnapshotOutputFeatures {
            output_rate: 100.0,
            byte_rate: 5000.0,
            entropy: 4.5,
            unique_line_ratio: 0.8,
            ansi_density: 0.1,
        };
        let json = serde_json::to_string(&features).unwrap();
        let decoded: SnapshotOutputFeatures = serde_json::from_str(&json).unwrap();
        assert!((decoded.output_rate - 100.0).abs() < f64::EPSILON);
        assert!((decoded.entropy - 4.5).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // Edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn manager_latest_snapshot() {
        let mut mgr = PaneSnapshotManager::new(0, test_config());
        assert!(mgr.latest_snapshot().is_none());
        mgr.manual_snapshot_at("a".to_string(), 100);
        mgr.manual_snapshot_at("b".to_string(), 200);
        assert_eq!(mgr.latest_snapshot().unwrap().snapshot_id, 1);
    }

    #[test]
    fn manager_total_created_vs_retained() {
        let config = SnapshotConfig {
            max_snapshots_per_pane: 2,
            ..test_config()
        };
        let mut mgr = PaneSnapshotManager::new(0, config);
        for i in 0..10u64 {
            mgr.manual_snapshot_at(format!("s{i}"), i * 100);
        }
        assert_eq!(mgr.snapshot_count(), 2);
        assert_eq!(mgr.total_created(), 10);
    }

    #[test]
    fn shell_transition_variants() {
        // Verify all variants serialize/deserialize.
        for transition in [
            ShellTransition::CommandStarted,
            ShellTransition::CommandFinished,
            ShellTransition::PromptRestored,
        ] {
            let json = serde_json::to_string(&transition).unwrap();
            let decoded: ShellTransition = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, transition);
        }
    }

    #[test]
    fn phase_end_reason_variants() {
        let reasons = vec![
            PhaseEndReason::RegimeShift,
            PhaseEndReason::CommandFinished { exit_code: Some(0) },
            PhaseEndReason::CommandFinished { exit_code: None },
            PhaseEndReason::ManualReset,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let decoded: PhaseEndReason = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, reason);
        }
    }

    #[test]
    fn snapshot_env_serde() {
        let env = SnapshotEnv {
            vars: {
                let mut m = HashMap::new();
                m.insert("HOME".to_string(), "/home/test".to_string());
                m
            },
            redacted_count: 3,
        };
        let json = serde_json::to_string(&env).unwrap();
        let decoded: SnapshotEnv = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.vars["HOME"], "/home/test");
        assert_eq!(decoded.redacted_count, 3);
    }

    #[test]
    fn snapshot_process_info_minimal() {
        let info = SnapshotProcessInfo {
            name: "bash".to_string(),
            pid: None,
            argv: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("\"pid\""));
        assert!(!json.contains("\"argv\""));
        let decoded: SnapshotProcessInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, "bash");
    }
}
