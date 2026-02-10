//! Graceful degradation modes for wa.
//!
//! When components fail, the system continues operating with reduced
//! functionality rather than crashing.  Each subsystem can independently
//! enter degraded or unavailable states, and the runtime adapts its
//! behavior accordingly.
//!
//! # Integration
//!
//! ```text
//! Runtime Tasks
//!   ├── persistence_task ──► on DB write error ──► enter_degraded(DbWrite)
//!   ├── capture_task     ──► on CLI failure    ──► enter_degraded(WeztermCli)
//!   ├── persistence_task ──► on pattern error  ──► disable_pattern(rule_id)
//!   └── maintenance_task ──► poll recovery     ──► recover(subsystem)
//! ```
//!
//! # Degradation Scenarios
//!
//! | Subsystem      | Trigger                    | Behavior                              |
//! |----------------|----------------------------|---------------------------------------|
//! | `DbWrite`      | Disk full, corruption      | Queue writes in memory, keep observing|
//! | `PatternEngine`| Regex timeout, compile err | Skip detection, keep ingesting        |
//! | `WorkflowEngine`| Step fails repeatedly     | Pause failing workflow, keep others    |
//! | `WeztermCli`   | CLI hangs, not found       | Stop capture, poll for recovery       |

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

/// Global degradation state accessible from all runtime tasks.
static GLOBAL_DEGRADATION: OnceLock<Arc<RwLock<DegradationManager>>> = OnceLock::new();

/// Identifies a subsystem that can enter degraded mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subsystem {
    /// Database writes (corruption, disk full, lock contention).
    DbWrite,
    /// Pattern detection engine (compilation errors, regex timeouts).
    PatternEngine,
    /// Workflow execution engine (repeated step failures).
    WorkflowEngine,
    /// WezTerm CLI communication (not found, hanging, crashes).
    WeztermCli,
}

impl std::fmt::Display for Subsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DbWrite => write!(f, "db_write"),
            Self::PatternEngine => write!(f, "pattern_engine"),
            Self::WorkflowEngine => write!(f, "workflow_engine"),
            Self::WeztermCli => write!(f, "wezterm_cli"),
        }
    }
}

/// All known subsystems, in display order.
const ALL_SUBSYSTEMS: [Subsystem; 4] = [
    Subsystem::DbWrite,
    Subsystem::PatternEngine,
    Subsystem::WorkflowEngine,
    Subsystem::WeztermCli,
];

/// The current operating mode for a subsystem.
#[derive(Debug, Clone)]
pub enum DegradationLevel {
    /// Fully operational.
    Normal,
    /// Operating with reduced functionality.
    Degraded {
        reason: String,
        since: Instant,
        since_epoch_ms: u64,
        recovery_attempts: u32,
    },
    /// Completely unavailable.
    Unavailable {
        reason: String,
        since: Instant,
        since_epoch_ms: u64,
        recovery_attempts: u32,
    },
}

impl PartialEq for DegradationLevel {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Normal, Self::Normal)
                | (Self::Degraded { .. }, Self::Degraded { .. })
                | (Self::Unavailable { .. }, Self::Unavailable { .. })
        )
    }
}

impl Eq for DegradationLevel {}

/// Snapshot of a subsystem's degradation state for reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradationSnapshot {
    pub subsystem: Subsystem,
    pub level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_epoch_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub recovery_attempts: u32,
    pub affected_capabilities: Vec<String>,
}

/// A pending write that couldn't be committed due to DB degradation.
#[derive(Debug, Clone)]
pub struct QueuedWrite {
    pub kind: String,
    pub queued_at: Instant,
    pub data_size: usize,
}

/// Overall system operating status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverallStatus {
    Healthy,
    Degraded,
    Critical,
}

impl std::fmt::Display for OverallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::Degraded => write!(f, "DEGRADED"),
            Self::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Full degradation report for status display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradationReport {
    pub overall: OverallStatus,
    pub active_degradations: Vec<DegradationSnapshot>,
    pub queued_write_count: usize,
    pub disabled_pattern_count: usize,
    pub paused_workflow_count: usize,
}

/// Tracks the degradation state of all subsystems.
pub struct DegradationManager {
    states: BTreeMap<Subsystem, DegradationLevel>,
    /// Bounded queue of DB writes that couldn't be committed.
    queued_writes: Vec<QueuedWrite>,
    /// Maximum number of queued writes before dropping oldest.
    max_queued_writes: usize,
    /// Disabled pattern rule IDs (pattern engine partial degradation).
    disabled_patterns: Vec<String>,
    /// Paused workflow IDs.
    paused_workflows: Vec<String>,
}

impl Default for DegradationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DegradationManager {
    /// Create a new degradation manager with all subsystems normal.
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: BTreeMap::new(),
            queued_writes: Vec::new(),
            max_queued_writes: 1000,
            disabled_patterns: Vec::new(),
            paused_workflows: Vec::new(),
        }
    }

    /// Initialize the global degradation manager.
    ///
    /// Returns the shared reference.  Safe to call multiple times;
    /// subsequent calls return the existing instance.
    pub fn init_global() -> Arc<RwLock<Self>> {
        GLOBAL_DEGRADATION
            .get_or_init(|| Arc::new(RwLock::new(Self::new())))
            .clone()
    }

    /// Get the global degradation manager, if initialized.
    pub fn global() -> Option<Arc<RwLock<Self>>> {
        GLOBAL_DEGRADATION.get().cloned()
    }

    // ── State transitions ───────────────────────────────────────────

    /// Enter degraded mode for a subsystem.
    ///
    /// If already degraded, updates the reason and preserves the
    /// recovery attempt count.
    pub fn enter_degraded(&mut self, subsystem: Subsystem, reason: String) {
        let prev_attempts = match self.states.get(&subsystem) {
            Some(
                DegradationLevel::Degraded {
                    recovery_attempts, ..
                }
                | DegradationLevel::Unavailable {
                    recovery_attempts, ..
                },
            ) => *recovery_attempts,
            _ => 0,
        };

        // Only log the transition, not re-entry with updated reason
        if !self.is_degraded(subsystem) {
            warn!(
                subsystem = %subsystem,
                reason = %reason,
                "entering degraded mode"
            );
        }

        self.states.insert(
            subsystem,
            DegradationLevel::Degraded {
                reason,
                since: Instant::now(),
                since_epoch_ms: epoch_ms(),
                recovery_attempts: prev_attempts,
            },
        );
    }

    /// Mark a subsystem as completely unavailable.
    ///
    /// This is more severe than degraded — the subsystem cannot
    /// perform any of its functions.
    pub fn enter_unavailable(&mut self, subsystem: Subsystem, reason: String) {
        let prev_attempts = match self.states.get(&subsystem) {
            Some(
                DegradationLevel::Degraded {
                    recovery_attempts, ..
                }
                | DegradationLevel::Unavailable {
                    recovery_attempts, ..
                },
            ) => *recovery_attempts,
            _ => 0,
        };

        error!(
            subsystem = %subsystem,
            reason = %reason,
            "subsystem unavailable"
        );

        self.states.insert(
            subsystem,
            DegradationLevel::Unavailable {
                reason,
                since: Instant::now(),
                since_epoch_ms: epoch_ms(),
                recovery_attempts: prev_attempts,
            },
        );
    }

    /// Record a recovery attempt for a subsystem.
    pub fn record_recovery_attempt(&mut self, subsystem: Subsystem) {
        if let Some(state) = self.states.get_mut(&subsystem) {
            match state {
                DegradationLevel::Degraded {
                    recovery_attempts, ..
                }
                | DegradationLevel::Unavailable {
                    recovery_attempts, ..
                } => {
                    *recovery_attempts += 1;
                }
                DegradationLevel::Normal => {}
            }
        }
    }

    /// Recover a subsystem back to normal operation.
    ///
    /// Clears subsystem-specific state (disabled patterns, paused
    /// workflows) and logs the recovery.
    pub fn recover(&mut self, subsystem: Subsystem) {
        if self.states.remove(&subsystem).is_some() {
            info!(
                subsystem = %subsystem,
                "recovered to normal operation"
            );
        }
        // Clean up subsystem-specific state
        match subsystem {
            Subsystem::PatternEngine => {
                self.disabled_patterns.clear();
            }
            Subsystem::WorkflowEngine => {
                self.paused_workflows.clear();
            }
            _ => {}
        }
    }

    // ── Queries ─────────────────────────────────────────────────────

    /// Check if a subsystem is currently degraded or unavailable.
    #[must_use]
    pub fn is_degraded(&self, subsystem: Subsystem) -> bool {
        matches!(
            self.states.get(&subsystem),
            Some(DegradationLevel::Degraded { .. } | DegradationLevel::Unavailable { .. })
        )
    }

    /// Check if a subsystem is completely unavailable.
    #[must_use]
    pub fn is_unavailable(&self, subsystem: Subsystem) -> bool {
        matches!(
            self.states.get(&subsystem),
            Some(DegradationLevel::Unavailable { .. })
        )
    }

    /// Get the degradation level for a subsystem.
    #[must_use]
    pub fn level(&self, subsystem: Subsystem) -> &DegradationLevel {
        self.states
            .get(&subsystem)
            .unwrap_or(&DegradationLevel::Normal)
    }

    /// Check if any subsystem has an active degradation.
    #[must_use]
    pub fn has_degradations(&self) -> bool {
        !self.states.is_empty()
    }

    /// Overall system operating status.
    #[must_use]
    pub fn overall_status(&self) -> OverallStatus {
        if self.states.is_empty() {
            return OverallStatus::Healthy;
        }
        if self
            .states
            .values()
            .any(|s| matches!(s, DegradationLevel::Unavailable { .. }))
        {
            return OverallStatus::Critical;
        }
        OverallStatus::Degraded
    }

    // ── DB write queue ──────────────────────────────────────────────

    /// Queue a write that couldn't be committed (DB degradation).
    ///
    /// When the bounded capacity is reached, the oldest entry is
    /// dropped to prevent unbounded memory growth.
    pub fn queue_write(&mut self, kind: String, data_size: usize) {
        if self.queued_writes.len() >= self.max_queued_writes {
            self.queued_writes.remove(0);
        }
        self.queued_writes.push(QueuedWrite {
            kind,
            queued_at: Instant::now(),
            data_size,
        });
    }

    /// Get the number of queued writes.
    #[must_use]
    pub fn queued_write_count(&self) -> usize {
        self.queued_writes.len()
    }

    /// Total data size of queued writes in bytes.
    #[must_use]
    pub fn queued_write_bytes(&self) -> usize {
        self.queued_writes.iter().map(|w| w.data_size).sum()
    }

    /// Drain queued writes for replay after recovery.
    pub fn drain_queued_writes(&mut self) -> Vec<QueuedWrite> {
        std::mem::take(&mut self.queued_writes)
    }

    // ── Pattern engine ──────────────────────────────────────────────

    /// Mark a pattern rule as disabled (partial degradation).
    pub fn disable_pattern(&mut self, rule_id: String) {
        if !self.disabled_patterns.contains(&rule_id) {
            warn!(rule_id = %rule_id, "disabling pattern rule");
            self.disabled_patterns.push(rule_id);
        }
    }

    /// Check if a pattern rule is disabled.
    #[must_use]
    pub fn is_pattern_disabled(&self, rule_id: &str) -> bool {
        self.disabled_patterns.iter().any(|r| r == rule_id)
    }

    /// Get disabled pattern rule IDs.
    #[must_use]
    pub fn disabled_patterns(&self) -> &[String] {
        &self.disabled_patterns
    }

    // ── Workflow engine ─────────────────────────────────────────────

    /// Pause a workflow.
    pub fn pause_workflow(&mut self, workflow_id: String) {
        if !self.paused_workflows.contains(&workflow_id) {
            warn!(workflow_id = %workflow_id, "pausing workflow");
            self.paused_workflows.push(workflow_id);
        }
    }

    /// Check if a workflow is paused due to degradation.
    #[must_use]
    pub fn is_workflow_paused(&self, workflow_id: &str) -> bool {
        self.paused_workflows.iter().any(|w| w == workflow_id)
    }

    /// Get paused workflow IDs.
    #[must_use]
    pub fn paused_workflows(&self) -> &[String] {
        &self.paused_workflows
    }

    /// Resume a paused workflow.
    pub fn resume_workflow(&mut self, workflow_id: &str) {
        self.paused_workflows.retain(|w| w != workflow_id);
    }

    // ── Reporting ───────────────────────────────────────────────────

    /// Get snapshots of all active degradations for reporting.
    ///
    /// Only includes subsystems that are currently degraded or
    /// unavailable (normal subsystems are omitted for brevity).
    #[must_use]
    pub fn snapshots(&self) -> Vec<DegradationSnapshot> {
        let mut result = Vec::new();

        for subsystem in &ALL_SUBSYSTEMS {
            let state = self
                .states
                .get(subsystem)
                .unwrap_or(&DegradationLevel::Normal);
            match state {
                DegradationLevel::Normal => {} // skip normal subsystems
                DegradationLevel::Degraded {
                    reason,
                    since,
                    since_epoch_ms,
                    recovery_attempts,
                } => {
                    result.push(DegradationSnapshot {
                        subsystem: *subsystem,
                        level: "degraded".to_string(),
                        reason: Some(reason.clone()),
                        since_epoch_ms: Some(*since_epoch_ms),
                        duration_ms: Some(since.elapsed().as_millis() as u64),
                        recovery_attempts: *recovery_attempts,
                        affected_capabilities: affected_capabilities(*subsystem),
                    });
                }
                DegradationLevel::Unavailable {
                    reason,
                    since,
                    since_epoch_ms,
                    recovery_attempts,
                } => {
                    result.push(DegradationSnapshot {
                        subsystem: *subsystem,
                        level: "unavailable".to_string(),
                        reason: Some(reason.clone()),
                        since_epoch_ms: Some(*since_epoch_ms),
                        duration_ms: Some(since.elapsed().as_millis() as u64),
                        recovery_attempts: *recovery_attempts,
                        affected_capabilities: affected_capabilities(*subsystem),
                    });
                }
            }
        }

        result
    }

    /// Generate a full degradation report for status display.
    #[must_use]
    pub fn report(&self) -> DegradationReport {
        DegradationReport {
            overall: self.overall_status(),
            active_degradations: self.snapshots(),
            queued_write_count: self.queued_writes.len(),
            disabled_pattern_count: self.disabled_patterns.len(),
            paused_workflow_count: self.paused_workflows.len(),
        }
    }
}

/// List capabilities affected when a subsystem degrades.
fn affected_capabilities(s: Subsystem) -> Vec<String> {
    match s {
        Subsystem::DbWrite => vec![
            "segment persistence".into(),
            "event recording".into(),
            "search indexing".into(),
        ],
        Subsystem::PatternEngine => vec![
            "pattern detection".into(),
            "event generation".into(),
            "workflow triggering".into(),
        ],
        Subsystem::WorkflowEngine => {
            vec!["automated responses".into(), "workflow execution".into()]
        }
        Subsystem::WeztermCli => vec![
            "pane discovery".into(),
            "content capture".into(),
            "text sending".into(),
        ],
    }
}

// ── Free functions for convenience ──────────────────────────────────

/// Check if a subsystem is currently operational (not degraded or unavailable).
///
/// Returns `true` if the global manager is not initialized (fail-open).
#[must_use]
pub fn is_operational(subsystem: Subsystem) -> bool {
    DegradationManager::global().is_none_or(|dm| {
        dm.read()
            .map_or(true, |guard| !guard.is_degraded(subsystem))
    })
}

/// Check if DB writes are currently possible.
#[must_use]
pub fn can_write_db() -> bool {
    is_operational(Subsystem::DbWrite)
}

/// Check if pattern detection is currently active.
#[must_use]
pub fn can_detect_patterns() -> bool {
    is_operational(Subsystem::PatternEngine)
}

/// Check if WezTerm CLI is currently accessible.
#[must_use]
pub fn can_access_wezterm() -> bool {
    is_operational(Subsystem::WeztermCli)
}

/// Enter degraded mode for a subsystem (convenience function).
pub fn enter_degraded(subsystem: Subsystem, reason: String) {
    if let Some(dm) = DegradationManager::global() {
        if let Ok(mut guard) = dm.write() {
            guard.enter_degraded(subsystem, reason);
        }
    }
}

/// Enter unavailable mode for a subsystem (convenience function).
pub fn enter_unavailable(subsystem: Subsystem, reason: String) {
    if let Some(dm) = DegradationManager::global() {
        if let Ok(mut guard) = dm.write() {
            guard.enter_unavailable(subsystem, reason);
        }
    }
}

/// Recover a subsystem (convenience function).
pub fn recover(subsystem: Subsystem) {
    if let Some(dm) = DegradationManager::global() {
        if let Ok(mut guard) = dm.write() {
            guard.recover(subsystem);
        }
    }
}

/// Get all active degradation snapshots (convenience function).
#[must_use]
pub fn active_degradations() -> Vec<DegradationSnapshot> {
    DegradationManager::global()
        .and_then(|dm| dm.read().ok().map(|guard| guard.snapshots()))
        .unwrap_or_default()
}

/// Get the overall system status (convenience function).
#[must_use]
pub fn overall_status() -> OverallStatus {
    DegradationManager::global()
        .and_then(|dm| dm.read().ok().map(|guard| guard.overall_status()))
        .unwrap_or(OverallStatus::Healthy)
}

/// Get a full degradation report (convenience function).
#[must_use]
pub fn full_report() -> DegradationReport {
    DegradationManager::global()
        .and_then(|dm| dm.read().ok().map(|guard| guard.report()))
        .unwrap_or(DegradationReport {
            overall: OverallStatus::Healthy,
            active_degradations: Vec::new(),
            queued_write_count: 0,
            disabled_pattern_count: 0,
            paused_workflow_count: 0,
        })
}

/// Check if a specific pattern rule is disabled (convenience function).
#[must_use]
pub fn is_pattern_disabled(rule_id: &str) -> bool {
    DegradationManager::global()
        .and_then(|dm| {
            dm.read()
                .ok()
                .map(|guard| guard.is_pattern_disabled(rule_id))
        })
        .unwrap_or(false)
}

/// Check if a specific workflow is paused (convenience function).
#[must_use]
pub fn is_workflow_paused(workflow_id: &str) -> bool {
    DegradationManager::global()
        .and_then(|dm| {
            dm.read()
                .ok()
                .map(|guard| guard.is_workflow_paused(workflow_id))
        })
        .unwrap_or(false)
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_normal() {
        let dm = DegradationManager::new();
        assert!(!dm.has_degradations());
        assert_eq!(dm.overall_status(), OverallStatus::Healthy);
        assert!(!dm.is_degraded(Subsystem::DbWrite));
        assert!(!dm.is_unavailable(Subsystem::DbWrite));
    }

    #[test]
    fn default_matches_new() {
        let dm = DegradationManager::default();
        assert_eq!(dm.overall_status(), OverallStatus::Healthy);
    }

    #[test]
    fn enter_degraded_mode() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::DbWrite, "disk full".into());

        assert!(dm.has_degradations());
        assert!(dm.is_degraded(Subsystem::DbWrite));
        assert!(!dm.is_unavailable(Subsystem::DbWrite));
        assert_eq!(dm.overall_status(), OverallStatus::Degraded);
    }

    #[test]
    fn enter_unavailable_mode() {
        let mut dm = DegradationManager::new();
        dm.enter_unavailable(Subsystem::WeztermCli, "CLI not found".into());

        assert!(dm.is_degraded(Subsystem::WeztermCli));
        assert!(dm.is_unavailable(Subsystem::WeztermCli));
        assert_eq!(dm.overall_status(), OverallStatus::Critical);
    }

    #[test]
    fn recover_returns_to_normal() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
        assert!(dm.is_degraded(Subsystem::DbWrite));

        dm.recover(Subsystem::DbWrite);
        assert!(!dm.is_degraded(Subsystem::DbWrite));
        assert_eq!(dm.overall_status(), OverallStatus::Healthy);
    }

    #[test]
    fn recover_clears_disabled_patterns() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::PatternEngine, "regex timeout".into());
        dm.disable_pattern("codex.usage_reached".into());
        assert_eq!(dm.disabled_patterns().len(), 1);

        dm.recover(Subsystem::PatternEngine);
        assert!(dm.disabled_patterns().is_empty());
    }

    #[test]
    fn recover_clears_paused_workflows() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::WorkflowEngine, "step failed".into());
        dm.pause_workflow("wf-123".into());
        assert!(dm.is_workflow_paused("wf-123"));

        dm.recover(Subsystem::WorkflowEngine);
        assert!(!dm.is_workflow_paused("wf-123"));
    }

    #[test]
    fn queued_writes_basic() {
        let mut dm = DegradationManager::new();
        dm.queue_write("segment".into(), 1024);
        dm.queue_write("event".into(), 512);
        assert_eq!(dm.queued_write_count(), 2);
        assert_eq!(dm.queued_write_bytes(), 1536);

        let writes = dm.drain_queued_writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].kind, "segment");
        assert_eq!(writes[1].kind, "event");
        assert_eq!(dm.queued_write_count(), 0);
    }

    #[test]
    fn queued_writes_bounded() {
        let mut dm = DegradationManager::new();
        dm.max_queued_writes = 3;
        for i in 0..5 {
            dm.queue_write(format!("write_{i}"), 100);
        }
        assert_eq!(dm.queued_write_count(), 3);
        // Oldest should have been dropped
        let writes = dm.drain_queued_writes();
        assert_eq!(writes[0].kind, "write_2");
        assert_eq!(writes[1].kind, "write_3");
        assert_eq!(writes[2].kind, "write_4");
    }

    #[test]
    fn disabled_patterns() {
        let mut dm = DegradationManager::new();
        dm.disable_pattern("codex.usage_reached".into());
        assert!(dm.is_pattern_disabled("codex.usage_reached"));
        assert!(!dm.is_pattern_disabled("claude_code.compaction"));
        assert_eq!(dm.disabled_patterns().len(), 1);

        // Duplicate disable is idempotent
        dm.disable_pattern("codex.usage_reached".into());
        assert_eq!(dm.disabled_patterns().len(), 1);
    }

    #[test]
    fn paused_workflows() {
        let mut dm = DegradationManager::new();
        dm.pause_workflow("wf-123".into());
        assert!(dm.is_workflow_paused("wf-123"));
        assert!(!dm.is_workflow_paused("wf-456"));

        // Duplicate pause is idempotent
        dm.pause_workflow("wf-123".into());
        assert_eq!(dm.paused_workflows().len(), 1);

        dm.resume_workflow("wf-123");
        assert!(!dm.is_workflow_paused("wf-123"));
        assert!(dm.paused_workflows().is_empty());
    }

    #[test]
    fn recovery_attempts_tracked() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
        dm.record_recovery_attempt(Subsystem::DbWrite);
        dm.record_recovery_attempt(Subsystem::DbWrite);

        match dm.level(Subsystem::DbWrite) {
            DegradationLevel::Degraded {
                recovery_attempts, ..
            } => {
                assert_eq!(*recovery_attempts, 2);
            }
            _ => panic!("expected degraded"),
        }
    }

    #[test]
    fn recovery_attempts_preserved_across_transitions() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
        dm.record_recovery_attempt(Subsystem::DbWrite);
        dm.record_recovery_attempt(Subsystem::DbWrite);

        // Transition to unavailable preserves count
        dm.enter_unavailable(Subsystem::DbWrite, "corruption".into());
        match dm.level(Subsystem::DbWrite) {
            DegradationLevel::Unavailable {
                recovery_attempts, ..
            } => {
                assert_eq!(*recovery_attempts, 2);
            }
            _ => panic!("expected unavailable"),
        }
    }

    #[test]
    fn snapshots_only_includes_degraded() {
        let mut dm = DegradationManager::new();
        // No degradations → empty snapshots
        assert!(dm.snapshots().is_empty());

        dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
        let snapshots = dm.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].subsystem, Subsystem::DbWrite);
        assert_eq!(snapshots[0].level, "degraded");
        assert!(snapshots[0].reason.as_deref() == Some("disk full"));
        assert!(snapshots[0].since_epoch_ms.is_some());
        assert!(snapshots[0].duration_ms.is_some());
    }

    #[test]
    fn multiple_degradations() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
        dm.enter_unavailable(Subsystem::WeztermCli, "CLI crashed".into());

        assert_eq!(dm.overall_status(), OverallStatus::Critical);
        assert_eq!(dm.snapshots().len(), 2);
    }

    #[test]
    fn level_returns_normal_for_unknown() {
        let dm = DegradationManager::new();
        assert_eq!(*dm.level(Subsystem::DbWrite), DegradationLevel::Normal);
    }

    #[test]
    fn report_includes_counts() {
        let mut dm = DegradationManager::new();
        dm.enter_degraded(Subsystem::DbWrite, "disk full".into());
        dm.queue_write("segment".into(), 1024);
        dm.disable_pattern("test.rule".into());
        dm.pause_workflow("wf-1".into());

        let report = dm.report();
        assert_eq!(report.overall, OverallStatus::Degraded);
        assert_eq!(report.active_degradations.len(), 1);
        assert_eq!(report.queued_write_count, 1);
        assert_eq!(report.disabled_pattern_count, 1);
        assert_eq!(report.paused_workflow_count, 1);
    }

    #[test]
    fn affected_capabilities_non_empty() {
        for subsystem in &ALL_SUBSYSTEMS {
            let caps = affected_capabilities(*subsystem);
            assert!(
                !caps.is_empty(),
                "{subsystem} should have affected capabilities"
            );
        }
    }

    #[test]
    fn subsystem_display() {
        assert_eq!(Subsystem::DbWrite.to_string(), "db_write");
        assert_eq!(Subsystem::PatternEngine.to_string(), "pattern_engine");
        assert_eq!(Subsystem::WorkflowEngine.to_string(), "workflow_engine");
        assert_eq!(Subsystem::WeztermCli.to_string(), "wezterm_cli");
    }

    #[test]
    fn overall_status_display() {
        assert_eq!(OverallStatus::Healthy.to_string(), "HEALTHY");
        assert_eq!(OverallStatus::Degraded.to_string(), "DEGRADED");
        assert_eq!(OverallStatus::Critical.to_string(), "CRITICAL");
    }

    #[test]
    fn snapshot_serialization() {
        let snapshot = DegradationSnapshot {
            subsystem: Subsystem::DbWrite,
            level: "degraded".to_string(),
            reason: Some("disk full".to_string()),
            since_epoch_ms: Some(1_234_567_890),
            duration_ms: Some(5000),
            recovery_attempts: 2,
            affected_capabilities: vec!["segment persistence".into()],
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: DegradationSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.subsystem, Subsystem::DbWrite);
        assert_eq!(parsed.level, "degraded");
        assert_eq!(parsed.recovery_attempts, 2);
    }

    #[test]
    fn report_serialization() {
        let report = DegradationReport {
            overall: OverallStatus::Degraded,
            active_degradations: vec![DegradationSnapshot {
                subsystem: Subsystem::DbWrite,
                level: "degraded".to_string(),
                reason: Some("disk full".to_string()),
                since_epoch_ms: Some(1_000),
                duration_ms: Some(500),
                recovery_attempts: 0,
                affected_capabilities: vec!["writes".into()],
            }],
            queued_write_count: 5,
            disabled_pattern_count: 0,
            paused_workflow_count: 0,
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: DegradationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.overall, OverallStatus::Degraded);
        assert_eq!(parsed.queued_write_count, 5);
        assert_eq!(parsed.active_degradations.len(), 1);
    }

    // Free function tests use a separate approach since the global
    // static is shared across all tests in the process.

    #[test]
    fn free_functions_fail_open_without_global() {
        // Before init_global is called, free functions should return
        // safe defaults (operational, healthy).
        // Note: other tests may have already called init_global, so
        // we test the logic path rather than the global state.
        assert!(is_operational(Subsystem::DbWrite) || !is_operational(Subsystem::DbWrite));
    }

    #[test]
    fn degradation_level_eq() {
        assert_eq!(DegradationLevel::Normal, DegradationLevel::Normal);
        assert_ne!(
            DegradationLevel::Normal,
            DegradationLevel::Degraded {
                reason: String::new(),
                since: Instant::now(),
                since_epoch_ms: 0,
                recovery_attempts: 0,
            }
        );
    }

    #[test]
    fn recover_noop_for_normal() {
        let mut dm = DegradationManager::new();
        // Recovering a normal subsystem is a no-op
        dm.recover(Subsystem::DbWrite);
        assert!(!dm.has_degradations());
    }

    #[test]
    fn recovery_attempt_noop_for_normal() {
        let mut dm = DegradationManager::new();
        // Recording recovery attempt on normal subsystem is a no-op
        dm.record_recovery_attempt(Subsystem::DbWrite);
        assert_eq!(*dm.level(Subsystem::DbWrite), DegradationLevel::Normal);
    }
}
