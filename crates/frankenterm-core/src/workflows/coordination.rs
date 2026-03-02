//! Multi-pane coordination primitives and workflows.
//!
//! Provides pane grouping strategies, coordination helpers (pause/resume/reread),
//! multi-pane workflow orchestration, and the unstick workflow for read-only
//! code scanning.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// Multi-Pane Coordination Primitives (wa-nu4.4.4.1)
// ============================================================================

/// Strategy for grouping panes into coordination groups.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneGroupStrategy {
    /// Group by WezTerm domain (e.g., "local", "SSH:host").
    ByDomain,
    /// Group by inferred agent type (codex, claude_code, etc.).
    ByAgent,
    /// Group by project directory (cwd-based).
    ByProject,
    /// Explicit list of pane IDs.
    Explicit { pane_ids: Vec<u64> },
}

/// A named group of panes selected by a grouping strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneGroup {
    /// Group name (e.g., domain name, agent type, project path).
    pub name: String,
    /// Pane IDs belonging to this group.
    pub pane_ids: Vec<u64>,
    /// Strategy used to form this group.
    pub strategy: PaneGroupStrategy,
}

impl PaneGroup {
    /// Create a new pane group.
    #[must_use]
    pub fn new(name: impl Into<String>, pane_ids: Vec<u64>, strategy: PaneGroupStrategy) -> Self {
        Self {
            name: name.into(),
            pane_ids,
            strategy,
        }
    }

    /// Number of panes in this group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pane_ids.len()
    }

    /// Whether this group is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pane_ids.is_empty()
    }
}

/// Build pane groups from a list of pane records using the given strategy.
///
/// Returns groups sorted deterministically by name.
pub fn build_pane_groups(
    panes: &[crate::storage::PaneRecord],
    strategy: &PaneGroupStrategy,
) -> Vec<PaneGroup> {
    use std::collections::BTreeMap;

    match strategy {
        PaneGroupStrategy::ByDomain => {
            let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            for pane in panes {
                groups
                    .entry(pane.domain.clone())
                    .or_default()
                    .push(pane.pane_id);
            }
            groups
                .into_iter()
                .map(|(name, mut pane_ids)| {
                    pane_ids.sort_unstable();
                    PaneGroup::new(name, pane_ids, PaneGroupStrategy::ByDomain)
                })
                .collect()
        }
        PaneGroupStrategy::ByAgent => {
            let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            for pane in panes {
                let agent = pane
                    .title
                    .as_deref()
                    .and_then(infer_agent_from_title)
                    .unwrap_or("unknown")
                    .to_string();
                groups.entry(agent).or_default().push(pane.pane_id);
            }
            groups
                .into_iter()
                .map(|(name, mut pane_ids)| {
                    pane_ids.sort_unstable();
                    PaneGroup::new(name, pane_ids, PaneGroupStrategy::ByAgent)
                })
                .collect()
        }
        PaneGroupStrategy::ByProject => {
            let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            for pane in panes {
                let project = pane.cwd.as_deref().unwrap_or("unknown").to_string();
                groups.entry(project).or_default().push(pane.pane_id);
            }
            groups
                .into_iter()
                .map(|(name, mut pane_ids)| {
                    pane_ids.sort_unstable();
                    PaneGroup::new(name, pane_ids, PaneGroupStrategy::ByProject)
                })
                .collect()
        }
        PaneGroupStrategy::Explicit { pane_ids } => {
            let mut sorted = pane_ids.clone();
            sorted.sort_unstable();
            vec![PaneGroup::new("explicit", sorted, strategy.clone())]
        }
    }
}

/// Simple agent inference from pane title.
pub fn infer_agent_from_title(title: &str) -> Option<&'static str> {
    let lower = title.to_lowercase();
    if lower.contains("codex") {
        Some("codex")
    } else if lower.contains("claude") {
        Some("claude_code")
    } else if lower.contains("gemini") {
        Some("gemini")
    } else {
        None
    }
}

/// Result of attempting to acquire group locks.
#[derive(Debug, Clone)]
pub enum GroupLockResult {
    /// All pane locks acquired successfully.
    Acquired {
        /// Pane IDs that were locked.
        locked_panes: Vec<u64>,
    },
    /// Some panes were already locked; acquisition was rolled back.
    PartialFailure {
        /// Panes that were successfully locked (then released during rollback).
        would_have_locked: Vec<u64>,
        /// Panes that were already locked by other workflows.
        conflicts: Vec<GroupLockConflict>,
    },
}

impl GroupLockResult {
    /// Whether all locks were acquired.
    #[must_use]
    pub fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired { .. })
    }
}

/// Information about a lock conflict during group acquisition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupLockConflict {
    /// Pane that couldn't be locked.
    pub pane_id: u64,
    /// Workflow currently holding the lock.
    pub held_by_workflow: String,
    /// Execution ID holding the lock.
    pub held_by_execution: String,
}

impl PaneWorkflowLockManager {
    /// Attempt to acquire locks for all panes in a group (all-or-nothing).
    ///
    /// If any pane is already locked, all acquired locks are rolled back
    /// and `PartialFailure` is returned with conflict details.
    pub fn try_acquire_group(
        &self,
        pane_ids: &[u64],
        workflow_name: &str,
        execution_id: &str,
    ) -> GroupLockResult {
        let mut acquired = Vec::new();
        let mut conflicts = Vec::new();

        for &pane_id in pane_ids {
            match self.try_acquire(pane_id, workflow_name, execution_id) {
                LockAcquisitionResult::Acquired => {
                    acquired.push(pane_id);
                }
                LockAcquisitionResult::AlreadyLocked {
                    held_by_workflow,
                    held_by_execution,
                    ..
                } => {
                    conflicts.push(GroupLockConflict {
                        pane_id,
                        held_by_workflow,
                        held_by_execution,
                    });
                }
            }
        }

        if conflicts.is_empty() {
            GroupLockResult::Acquired {
                locked_panes: acquired,
            }
        } else {
            // Rollback: release all locks we acquired
            for pane_id in &acquired {
                self.release(*pane_id, execution_id);
            }
            GroupLockResult::PartialFailure {
                would_have_locked: acquired,
                conflicts,
            }
        }
    }

    /// Release locks for all panes in a group.
    pub fn release_group(&self, pane_ids: &[u64], execution_id: &str) -> usize {
        let mut released = 0;
        for &pane_id in pane_ids {
            if self.release(pane_id, execution_id) {
                released += 1;
            }
        }
        released
    }
}

/// Precondition that a pane must satisfy before a broadcast action is executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BroadcastPrecondition {
    /// Pane must have a shell prompt active (from OSC 133).
    PromptActive,
    /// Pane must NOT be in alternate screen mode (vim, less, etc.).
    NotAltScreen,
    /// Pane must NOT have a recent capture gap.
    NoRecentGap,
    /// Pane must NOT be reserved by another workflow.
    NotReserved,
}

impl BroadcastPrecondition {
    /// Check if the pane capabilities satisfy this precondition.
    #[must_use]
    pub fn check(&self, caps: &crate::policy::PaneCapabilities) -> bool {
        match self {
            Self::PromptActive => caps.prompt_active,
            Self::NotAltScreen => !caps.alt_screen.unwrap_or(false),
            Self::NoRecentGap => !caps.has_recent_gap,
            Self::NotReserved => !caps.is_reserved,
        }
    }

    /// Human-readable label for this precondition.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::PromptActive => "prompt_active",
            Self::NotAltScreen => "not_alt_screen",
            Self::NoRecentGap => "no_recent_gap",
            Self::NotReserved => "not_reserved",
        }
    }
}

/// Default safe broadcast preconditions.
///
/// These prevent "spray and pray" broadcasting:
/// - Prompt must be active
/// - Not in alternate screen
/// - No recent capture gap
/// - Not reserved by another workflow
#[must_use]
pub fn default_broadcast_preconditions() -> Vec<BroadcastPrecondition> {
    vec![
        BroadcastPrecondition::PromptActive,
        BroadcastPrecondition::NotAltScreen,
        BroadcastPrecondition::NoRecentGap,
        BroadcastPrecondition::NotReserved,
    ]
}

/// Check all preconditions against pane capabilities.
///
/// Returns a list of failed precondition labels.
pub fn check_preconditions(
    preconditions: &[BroadcastPrecondition],
    caps: &crate::policy::PaneCapabilities,
) -> Vec<&'static str> {
    preconditions
        .iter()
        .filter(|p| !p.check(caps))
        .map(|p| p.label())
        .collect()
}

/// Outcome of a broadcast action on a single pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PaneBroadcastOutcome {
    /// Action was allowed and executed.
    Allowed {
        /// Time taken for this pane's action in milliseconds.
        elapsed_ms: u64,
    },
    /// Action was denied by policy.
    Denied {
        /// Reason for denial.
        reason: String,
    },
    /// Preconditions were not met.
    PreconditionFailed {
        /// List of failed precondition labels.
        failed: Vec<String>,
    },
    /// Action was skipped (pane was locked by another workflow).
    Skipped {
        /// Reason for skipping.
        reason: String,
    },
    /// Verification after action failed.
    VerificationFailed {
        /// What went wrong during verification.
        reason: String,
    },
}

/// Full broadcast result across all targeted panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastResult {
    /// Workflow or action name.
    pub action: String,
    /// Per-pane outcomes, keyed by pane ID.
    pub outcomes: Vec<PaneBroadcastEntry>,
    /// Total elapsed time in milliseconds.
    pub total_elapsed_ms: u64,
}

/// A single entry in a broadcast result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneBroadcastEntry {
    /// Pane ID.
    pub pane_id: u64,
    /// Outcome for this pane.
    pub outcome: PaneBroadcastOutcome,
}

impl BroadcastResult {
    /// Create a new broadcast result.
    #[must_use]
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            outcomes: Vec::new(),
            total_elapsed_ms: 0,
        }
    }

    /// Add a pane outcome.
    pub fn add_outcome(&mut self, pane_id: u64, outcome: PaneBroadcastOutcome) {
        self.outcomes.push(PaneBroadcastEntry { pane_id, outcome });
    }

    /// Count of panes where the action was allowed.
    #[must_use]
    pub fn allowed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::Allowed { .. }))
            .count()
    }

    /// Count of panes where the action was denied.
    #[must_use]
    pub fn denied_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::Denied { .. }))
            .count()
    }

    /// Count of panes where preconditions failed.
    #[must_use]
    pub fn precondition_failed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::PreconditionFailed { .. }))
            .count()
    }

    /// Count of panes that were skipped.
    #[must_use]
    pub fn skipped_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::Skipped { .. }))
            .count()
    }

    /// Whether all targeted panes were allowed.
    #[must_use]
    pub fn all_allowed(&self) -> bool {
        !self.outcomes.is_empty() && self.allowed_count() == self.outcomes.len()
    }
}

// ============================================================================
// Multi-Pane Coordination Workflows (wa-nu4.4.4.2)
// ============================================================================

/// Configuration for the `coordinate_agents` family of multi-pane workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinateAgentsConfig {
    /// Strategy for selecting which panes to target.
    pub strategy: PaneGroupStrategy,
    /// Preconditions each pane must meet before receiving a broadcast action.
    #[serde(default = "default_broadcast_preconditions")]
    pub preconditions: Vec<BroadcastPrecondition>,
    /// Whether to abort the entire operation if group lock acquisition fails.
    #[serde(default)]
    pub abort_on_lock_failure: bool,
}

impl Default for CoordinateAgentsConfig {
    fn default() -> Self {
        Self {
            strategy: PaneGroupStrategy::ByAgent,
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        }
    }
}

/// Agent-specific text to send for context refresh.
#[must_use]
pub fn agent_reread_prompt(agent_hint: &str) -> &'static str {
    match agent_hint {
        "codex" => "Read the AGENTS.md file and follow the instructions for resuming context.\n",
        "claude_code" => "/read AGENTS.md\n",
        "gemini" => "Please read AGENTS.md and follow any instructions for context recovery.\n",
        _ => "cat AGENTS.md\n",
    }
}

/// Agent-specific safe pause keystrokes.
#[must_use]
pub fn agent_pause_text(agent_hint: &str) -> &'static str {
    match agent_hint {
        // For AI coding agents, Ctrl-C is the safest interrupt
        "codex" | "claude_code" | "gemini" => "\x03",
        // For unknown panes, also Ctrl-C
        _ => "\x03",
    }
}

/// Result of a multi-pane coordination operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinationResult {
    /// The operation that was performed.
    pub operation: String,
    /// Per-group results.
    pub groups: Vec<GroupCoordinationEntry>,
    /// Aggregate broadcast result across all groups.
    pub broadcast: BroadcastResult,
}

/// Per-group result within a coordination operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCoordinationEntry {
    /// Group name (e.g., domain name, agent type, project path).
    pub group_name: String,
    /// Number of panes in this group.
    pub pane_count: usize,
    /// Number of panes that received the action.
    pub acted_count: usize,
    /// Number of panes that failed preconditions.
    pub precondition_failed_count: usize,
    /// Number of panes that were skipped (lock conflicts, etc.).
    pub skipped_count: usize,
}

impl CoordinationResult {
    /// Create a new coordination result for the given operation.
    #[must_use]
    pub fn new(operation: impl Into<String>) -> Self {
        let op = operation.into();
        Self {
            operation: op.clone(),
            groups: Vec::new(),
            broadcast: BroadcastResult::new(op),
        }
    }

    /// Total panes that were successfully acted upon.
    #[must_use]
    pub fn total_acted(&self) -> usize {
        self.groups.iter().map(|g| g.acted_count).sum()
    }

    /// Total panes across all groups.
    #[must_use]
    pub fn total_panes(&self) -> usize {
        self.groups.iter().map(|g| g.pane_count).sum()
    }
}

/// Evaluate which panes in a group pass preconditions and return per-pane outcomes.
///
/// This is the core "filter before broadcast" logic that prevents spraying actions
/// to panes that aren't ready. Returns a vec of (pane_id, `Option<outcome>`) where
/// `None` means the pane passed all preconditions.
#[must_use]
pub fn evaluate_pane_preconditions<S: ::std::hash::BuildHasher>(
    pane_ids: &[u64],
    capabilities: &std::collections::HashMap<u64, crate::policy::PaneCapabilities, S>,
    preconditions: &[BroadcastPrecondition],
) -> Vec<(u64, Option<PaneBroadcastOutcome>)> {
    pane_ids
        .iter()
        .map(|&pid| {
            match capabilities.get(&pid) {
                Some(caps) => {
                    let failed = check_preconditions(preconditions, caps);
                    if failed.is_empty() {
                        (pid, None) // passed all preconditions
                    } else {
                        (
                            pid,
                            Some(PaneBroadcastOutcome::PreconditionFailed {
                                failed: failed.iter().map(|s| (*s).to_string()).collect(),
                            }),
                        )
                    }
                }
                None => (
                    pid,
                    Some(PaneBroadcastOutcome::Skipped {
                        reason: "no capabilities available for pane".to_string(),
                    }),
                ),
            }
        })
        .collect()
}

/// Plan a `reread_context` coordination: determine which panes would receive
/// a context refresh prompt and which would be filtered out.
///
/// This is a dry-run / planning function that does not execute any actions.
#[must_use]
pub fn plan_reread_context<S: ::std::hash::BuildHasher>(
    panes: &[crate::storage::PaneRecord],
    capabilities: &std::collections::HashMap<u64, crate::policy::PaneCapabilities, S>,
    config: &CoordinateAgentsConfig,
) -> CoordinationResult {
    let groups = build_pane_groups(panes, &config.strategy);
    let mut result = CoordinationResult::new("reread_context");

    for group in &groups {
        let evals =
            evaluate_pane_preconditions(&group.pane_ids, capabilities, &config.preconditions);
        let mut acted = 0usize;
        let mut precond_failed = 0usize;
        let mut skipped = 0usize;

        for (pid, outcome) in &evals {
            match outcome {
                None => {
                    acted += 1;
                    result
                        .broadcast
                        .add_outcome(*pid, PaneBroadcastOutcome::Allowed { elapsed_ms: 0 });
                }
                Some(o @ PaneBroadcastOutcome::PreconditionFailed { .. }) => {
                    precond_failed += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
                Some(o) => {
                    skipped += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
            }
        }

        result.groups.push(GroupCoordinationEntry {
            group_name: group.name.clone(),
            pane_count: group.pane_ids.len(),
            acted_count: acted,
            precondition_failed_count: precond_failed,
            skipped_count: skipped,
        });
    }

    result
}

/// Plan a `pause_all` coordination: determine which panes would receive
/// a safe pause signal.
#[must_use]
pub fn plan_pause_all<S: ::std::hash::BuildHasher>(
    panes: &[crate::storage::PaneRecord],
    capabilities: &std::collections::HashMap<u64, crate::policy::PaneCapabilities, S>,
    config: &CoordinateAgentsConfig,
) -> CoordinationResult {
    let groups = build_pane_groups(panes, &config.strategy);
    let mut result = CoordinationResult::new("pause_all");

    for group in &groups {
        // For pause_all, we only require NotAltScreen — we deliberately send
        // to panes even if a command is running (that's the point of pausing).
        let pause_preconditions: Vec<BroadcastPrecondition> = config
            .preconditions
            .iter()
            .filter(|p| matches!(p, BroadcastPrecondition::NotAltScreen))
            .cloned()
            .collect();

        let evals =
            evaluate_pane_preconditions(&group.pane_ids, capabilities, &pause_preconditions);
        let mut acted = 0usize;
        let mut precond_failed = 0usize;
        let mut skipped = 0usize;

        for (pid, outcome) in &evals {
            match outcome {
                None => {
                    acted += 1;
                    result
                        .broadcast
                        .add_outcome(*pid, PaneBroadcastOutcome::Allowed { elapsed_ms: 0 });
                }
                Some(o @ PaneBroadcastOutcome::PreconditionFailed { .. }) => {
                    precond_failed += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
                Some(o) => {
                    skipped += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
            }
        }

        result.groups.push(GroupCoordinationEntry {
            group_name: group.name.clone(),
            pane_count: group.pane_ids.len(),
            acted_count: acted,
            precondition_failed_count: precond_failed,
            skipped_count: skipped,
        });
    }

    result
}

/// Resolve the text to send for each pane in a `reread_context` operation.
///
/// Returns a map from pane_id to the prompt text, using agent-specific prompts
/// when the agent type can be inferred from the pane title.
#[must_use]
pub fn resolve_reread_prompts(
    panes: &[crate::storage::PaneRecord],
) -> std::collections::HashMap<u64, &'static str> {
    panes
        .iter()
        .map(|p| {
            let agent = p
                .title
                .as_deref()
                .and_then(infer_agent_from_title)
                .unwrap_or("unknown");
            (p.pane_id, agent_reread_prompt(agent))
        })
        .collect()
}

/// Resolve the text to send for each pane in a `pause_all` operation.
#[must_use]
pub fn resolve_pause_texts(
    panes: &[crate::storage::PaneRecord],
) -> std::collections::HashMap<u64, &'static str> {
    panes
        .iter()
        .map(|p| {
            let agent = p
                .title
                .as_deref()
                .and_then(infer_agent_from_title)
                .unwrap_or("unknown");
            (p.pane_id, agent_pause_text(agent))
        })
        .collect()
}

// ============================================================================
// Unstick Workflow: read-only code scanning (wa-nu4.4.4.4)
// ============================================================================

/// Category of code pattern scanned by the unstick workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnstickFindingKind {
    /// TODO / FIXME / HACK comment.
    TodoComment,
    /// `unwrap()` / `expect()` / `panic!()` call.
    PanicSite,
    /// Suspicious error handling (e.g., `let _ = ...` on Result).
    SuppressedError,
}

impl UnstickFindingKind {
    /// Human-readable label for display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TodoComment => "TODO/FIXME",
            Self::PanicSite => "panic site",
            Self::SuppressedError => "suppressed error",
        }
    }
}

/// A single finding from the unstick scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstickFinding {
    /// Category of the finding.
    pub kind: UnstickFindingKind,
    /// Relative file path from the repo root.
    pub file: String,
    /// One-based line number.
    pub line: u32,
    /// Short snippet of the matched code (bounded to 200 chars).
    pub snippet: String,
    /// Suggested next action for the agent.
    pub suggestion: String,
}

/// Configuration for the unstick scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstickConfig {
    /// Root directory to scan (must be absolute).
    pub root: std::path::PathBuf,
    /// Maximum number of findings per category.
    #[serde(default = "default_max_findings_per_kind")]
    pub max_findings_per_kind: usize,
    /// Maximum total findings across all categories.
    #[serde(default = "default_max_total_findings")]
    pub max_total_findings: usize,
    /// File extensions to scan (e.g., ["rs", "py", "ts"]).
    #[serde(default = "default_scan_extensions")]
    pub extensions: Vec<String>,
}

fn default_max_findings_per_kind() -> usize {
    10
}

fn default_max_total_findings() -> usize {
    25
}

fn default_scan_extensions() -> Vec<String> {
    vec![
        "rs".to_string(),
        "py".to_string(),
        "ts".to_string(),
        "js".to_string(),
        "go".to_string(),
    ]
}

impl Default for UnstickConfig {
    fn default() -> Self {
        Self {
            root: std::path::PathBuf::from("."),
            max_findings_per_kind: default_max_findings_per_kind(),
            max_total_findings: default_max_total_findings(),
            extensions: default_scan_extensions(),
        }
    }
}

/// Result of an unstick scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstickReport {
    /// Findings grouped by kind.
    pub findings: Vec<UnstickFinding>,
    /// Total files scanned.
    pub files_scanned: usize,
    /// Whether the scan was truncated due to limits.
    pub truncated: bool,
    /// Which scanner was used ("ast-grep" or "text").
    pub scanner: String,
    /// Summary counts by kind.
    pub counts: std::collections::BTreeMap<String, usize>,
}

impl UnstickReport {
    /// Create an empty report.
    #[must_use]
    pub fn empty(scanner: &str) -> Self {
        Self {
            findings: Vec::new(),
            files_scanned: 0,
            truncated: false,
            scanner: scanner.to_string(),
            counts: std::collections::BTreeMap::new(),
        }
    }

    /// Total number of findings.
    #[must_use]
    pub fn total_findings(&self) -> usize {
        self.findings.len()
    }

    /// Format as a concise human-readable summary.
    #[must_use]
    pub fn human_summary(&self) -> String {
        if self.findings.is_empty() {
            return "No actionable findings.".to_string();
        }

        let mut lines = Vec::new();
        lines.push(format!(
            "Found {} items across {} files (scanner: {}):",
            self.findings.len(),
            self.files_scanned,
            self.scanner,
        ));

        for (kind, count) in &self.counts {
            lines.push(format!("  {kind}: {count}"));
        }

        lines.push(String::new());

        // Show top findings (up to 10)
        for (i, f) in self.findings.iter().take(10).enumerate() {
            lines.push(format!(
                "  {}. [{}] {}:{} — {}",
                i + 1,
                f.kind.label(),
                f.file,
                f.line,
                truncate_snippet(&f.snippet, 80),
            ));
            lines.push(format!("     → {}", f.suggestion));
        }

        if self.findings.len() > 10 {
            lines.push(format!(
                "  ... and {} more (use --format json for full list)",
                self.findings.len() - 10
            ));
        }

        if self.truncated {
            lines.push("  (results truncated due to limits)".to_string());
        }

        lines.join("\n")
    }
}

/// Truncate a snippet to a max length, adding "..." if needed.
pub fn truncate_snippet(s: &str, max_len: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else if max_len == 0 {
        String::new()
    } else if max_len <= 3 {
        ".".repeat(max_len)
    } else {
        let prefix_budget = max_len - 3;
        let mut end = 0usize;
        for ch in trimmed.chars() {
            let next = end.saturating_add(ch.len_utf8());
            if next > prefix_budget {
                break;
            }
            end = next;
        }

        let mut out = trimmed[..end].to_string();
        out.push_str("...");
        out
    }
}

/// Regex patterns for text-based scanning (fallback when ast-grep is not available).
///
/// Hoisted into `LazyLock` statics so each regex is compiled exactly once.
pub struct TextScanPatterns {
    todo: &'static regex::Regex,
    panic_site: &'static regex::Regex,
    suppressed_error: &'static regex::Regex,
}

static TEXT_SCAN_TODO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(TODO|FIXME|HACK|XXX)\b").expect("valid regex"));
static TEXT_SCAN_PANIC_SITE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(unwrap|expect|panic!)\s*\(").expect("valid regex"));
static TEXT_SCAN_SUPPRESSED_ERROR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"let\s+_\s*=.*\?|let\s+_\s*=.*\.unwrap").expect("valid regex")
});

impl Default for TextScanPatterns {
    fn default() -> Self {
        Self {
            todo: &TEXT_SCAN_TODO,
            panic_site: &TEXT_SCAN_PANIC_SITE,
            suppressed_error: &TEXT_SCAN_SUPPRESSED_ERROR,
        }
    }
}

impl TextScanPatterns {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Scan a single file for findings using text-based patterns.
#[allow(clippy::implicit_hasher)]
pub fn scan_file_text(
    path: &std::path::Path,
    root: &std::path::Path,
    patterns: &TextScanPatterns,
    max_per_kind: usize,
    kind_counts: &mut std::collections::HashMap<UnstickFindingKind, usize>,
) -> Vec<UnstickFinding> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let rel_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();

    let mut findings = Vec::new();

    for (line_num, line) in content.lines().enumerate() {
        let line_num = (line_num + 1) as u32;

        // Check TODO/FIXME
        if patterns.todo.is_match(line) {
            let count = kind_counts
                .entry(UnstickFindingKind::TodoComment)
                .or_insert(0);
            if *count < max_per_kind {
                *count += 1;
                findings.push(UnstickFinding {
                    kind: UnstickFindingKind::TodoComment,
                    file: rel_path.clone(),
                    line: line_num,
                    snippet: line.trim().chars().take(200).collect(),
                    suggestion: "Address this TODO item or convert to a tracked issue".to_string(),
                });
            }
        }

        // Check panic sites
        if patterns.panic_site.is_match(line) {
            let count = kind_counts
                .entry(UnstickFindingKind::PanicSite)
                .or_insert(0);
            if *count < max_per_kind {
                *count += 1;
                findings.push(UnstickFinding {
                    kind: UnstickFindingKind::PanicSite,
                    file: rel_path.clone(),
                    line: line_num,
                    snippet: line.trim().chars().take(200).collect(),
                    suggestion: "Replace with proper error handling (? operator or match)"
                        .to_string(),
                });
            }
        }

        // Check suppressed errors
        if patterns.suppressed_error.is_match(line) {
            let count = kind_counts
                .entry(UnstickFindingKind::SuppressedError)
                .or_insert(0);
            if *count < max_per_kind {
                *count += 1;
                findings.push(UnstickFinding {
                    kind: UnstickFindingKind::SuppressedError,
                    file: rel_path.clone(),
                    line: line_num,
                    snippet: line.trim().chars().take(200).collect(),
                    suggestion: "Handle this error explicitly instead of suppressing it"
                        .to_string(),
                });
            }
        }
    }

    findings
}

/// Check whether `sg` (ast-grep) is available on the system.
#[must_use]
pub fn is_ast_grep_available() -> bool {
    std::process::Command::new("sg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the unstick scan using text-based patterns (always available).
///
/// This is the fallback scanner when ast-grep is not installed.
/// It scans files matching the configured extensions and returns
/// a bounded set of findings.
#[must_use]
pub fn run_unstick_scan_text(config: &UnstickConfig) -> UnstickReport {
    let patterns = TextScanPatterns::new();
    let mut kind_counts: std::collections::HashMap<UnstickFindingKind, usize> =
        std::collections::HashMap::new();
    let mut all_findings = Vec::new();
    let mut files_scanned = 0usize;
    let mut truncated = false;

    // Walk the directory tree using a stack-based approach (no walkdir dep)
    let mut dir_stack: Vec<(std::path::PathBuf, usize)> = vec![(config.root.clone(), 0)];

    while let Some((dir, depth)) = dir_stack.pop() {
        if depth > 10 || all_findings.len() >= config.max_total_findings {
            if all_findings.len() >= config.max_total_findings {
                truncated = true;
            }
            break;
        }

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            if all_findings.len() >= config.max_total_findings {
                truncated = true;
                break;
            }

            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                // Skip hidden dirs, target, node_modules, vendor
                if !name.starts_with('.')
                    && name != "target"
                    && name != "node_modules"
                    && name != "vendor"
                {
                    dir_stack.push((path, depth + 1));
                }
                continue;
            }

            if !path.is_file() {
                continue;
            }

            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();

            if !config.extensions.contains(&ext) {
                continue;
            }

            files_scanned += 1;

            let file_findings = scan_file_text(
                &path,
                &config.root,
                &patterns,
                config.max_findings_per_kind,
                &mut kind_counts,
            );

            for f in file_findings {
                if all_findings.len() >= config.max_total_findings {
                    truncated = true;
                    break;
                }
                all_findings.push(f);
            }
        }
    }

    // Build counts summary
    let counts: std::collections::BTreeMap<String, usize> = kind_counts
        .iter()
        .map(|(k, v)| (k.label().to_string(), *v))
        .collect();

    UnstickReport {
        findings: all_findings,
        files_scanned,
        truncated,
        scanner: "text".to_string(),
        counts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PaneCapabilities;
    use crate::storage::PaneRecord;
    use std::collections::HashMap;

    fn make_pane(pane_id: u64, domain: &str, title: Option<&str>, cwd: Option<&str>) -> PaneRecord {
        PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: domain.to_string(),
            window_id: None,
            tab_id: None,
            title: title.map(|s| s.to_string()),
            cwd: cwd.map(|s| s.to_string()),
            tty_name: None,
            first_seen_at: 0,
            last_seen_at: 0,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    fn caps_ready() -> PaneCapabilities {
        PaneCapabilities {
            prompt_active: true,
            command_running: false,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        }
    }

    // ========================================================================
    // PaneGroupStrategy serde roundtrip
    // ========================================================================

    #[test]
    fn pane_group_strategy_serde_roundtrip() {
        for strategy in [
            PaneGroupStrategy::ByDomain,
            PaneGroupStrategy::ByAgent,
            PaneGroupStrategy::ByProject,
            PaneGroupStrategy::Explicit {
                pane_ids: vec![1, 2, 3],
            },
        ] {
            let json = serde_json::to_string(&strategy).unwrap();
            let parsed: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(strategy, parsed);
        }
    }

    // ========================================================================
    // build_pane_groups
    // ========================================================================

    #[test]
    fn build_pane_groups_by_domain() {
        let panes = vec![
            make_pane(1, "local", None, None),
            make_pane(2, "SSH:host1", None, None),
            make_pane(3, "local", None, None),
            make_pane(4, "SSH:host1", None, None),
        ];
        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByDomain);
        assert_eq!(groups.len(), 2);
        // BTreeMap sorts keys, so "SSH:host1" < "local"
        assert_eq!(groups[0].name, "SSH:host1");
        assert_eq!(groups[0].pane_ids, vec![2, 4]);
        assert_eq!(groups[1].name, "local");
        assert_eq!(groups[1].pane_ids, vec![1, 3]);
    }

    #[test]
    fn build_pane_groups_by_agent() {
        let panes = vec![
            make_pane(1, "local", Some("codex-cli session"), None),
            make_pane(2, "local", Some("Claude Code"), None),
            make_pane(3, "local", Some("zsh"), None),
            make_pane(4, "local", Some("Gemini Assistant"), None),
        ];
        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByAgent);
        assert_eq!(groups.len(), 4);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"claude_code"));
        assert!(names.contains(&"gemini"));
        assert!(names.contains(&"unknown"));
    }

    #[test]
    fn build_pane_groups_by_project() {
        let panes = vec![
            make_pane(1, "local", None, Some("/home/user/proj-a")),
            make_pane(2, "local", None, Some("/home/user/proj-b")),
            make_pane(3, "local", None, Some("/home/user/proj-a")),
            make_pane(4, "local", None, None),
        ];
        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByProject);
        assert_eq!(groups.len(), 3);
        let a_group = groups
            .iter()
            .find(|g| g.name == "/home/user/proj-a")
            .unwrap();
        assert_eq!(a_group.pane_ids, vec![1, 3]);
        let unknown_group = groups.iter().find(|g| g.name == "unknown").unwrap();
        assert_eq!(unknown_group.pane_ids, vec![4]);
    }

    #[test]
    fn build_pane_groups_explicit() {
        let panes = vec![
            make_pane(1, "local", None, None),
            make_pane(2, "local", None, None),
        ];
        let strategy = PaneGroupStrategy::Explicit {
            pane_ids: vec![5, 3, 1],
        };
        let groups = build_pane_groups(&panes, &strategy);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "explicit");
        assert_eq!(groups[0].pane_ids, vec![1, 3, 5]); // sorted
    }

    #[test]
    fn build_pane_groups_empty_input() {
        let groups = build_pane_groups(&[], &PaneGroupStrategy::ByDomain);
        assert!(groups.is_empty());
    }

    // ========================================================================
    // infer_agent_from_title
    // ========================================================================

    #[test]
    fn infer_agent_codex() {
        assert_eq!(infer_agent_from_title("codex-cli session"), Some("codex"));
        assert_eq!(infer_agent_from_title("CODEX"), Some("codex"));
    }

    #[test]
    fn infer_agent_claude() {
        assert_eq!(infer_agent_from_title("Claude Code"), Some("claude_code"));
        assert_eq!(
            infer_agent_from_title("claude-code v2"),
            Some("claude_code")
        );
    }

    #[test]
    fn infer_agent_gemini() {
        assert_eq!(infer_agent_from_title("Gemini Assistant"), Some("gemini"));
    }

    #[test]
    fn infer_agent_unknown() {
        assert_eq!(infer_agent_from_title("zsh"), None);
        assert_eq!(infer_agent_from_title(""), None);
    }

    // ========================================================================
    // BroadcastPrecondition::check
    // ========================================================================

    #[test]
    fn precondition_prompt_active() {
        let mut caps = caps_ready();
        assert!(BroadcastPrecondition::PromptActive.check(&caps));
        caps.prompt_active = false;
        assert!(!BroadcastPrecondition::PromptActive.check(&caps));
    }

    #[test]
    fn precondition_not_alt_screen() {
        let mut caps = caps_ready();
        assert!(BroadcastPrecondition::NotAltScreen.check(&caps));
        caps.alt_screen = Some(true);
        assert!(!BroadcastPrecondition::NotAltScreen.check(&caps));
        // None => treated as not in alt screen (false)
        caps.alt_screen = None;
        assert!(BroadcastPrecondition::NotAltScreen.check(&caps));
    }

    #[test]
    fn precondition_no_recent_gap() {
        let mut caps = caps_ready();
        assert!(BroadcastPrecondition::NoRecentGap.check(&caps));
        caps.has_recent_gap = true;
        assert!(!BroadcastPrecondition::NoRecentGap.check(&caps));
    }

    #[test]
    fn precondition_not_reserved() {
        let mut caps = caps_ready();
        assert!(BroadcastPrecondition::NotReserved.check(&caps));
        caps.is_reserved = true;
        assert!(!BroadcastPrecondition::NotReserved.check(&caps));
    }

    // ========================================================================
    // check_preconditions
    // ========================================================================

    #[test]
    fn check_preconditions_all_pass() {
        let caps = caps_ready();
        let preconds = default_broadcast_preconditions();
        let failed = check_preconditions(&preconds, &caps);
        assert!(failed.is_empty());
    }

    #[test]
    fn check_preconditions_multiple_failures() {
        let caps = PaneCapabilities {
            prompt_active: false,
            alt_screen: Some(true),
            has_recent_gap: true,
            is_reserved: true,
            ..Default::default()
        };
        let preconds = default_broadcast_preconditions();
        let failed = check_preconditions(&preconds, &caps);
        assert_eq!(failed.len(), 4);
        assert!(failed.contains(&"prompt_active"));
        assert!(failed.contains(&"not_alt_screen"));
        assert!(failed.contains(&"no_recent_gap"));
        assert!(failed.contains(&"not_reserved"));
    }

    // ========================================================================
    // BroadcastResult counters
    // ========================================================================

    #[test]
    fn broadcast_result_counters() {
        let mut result = BroadcastResult::new("test_action");
        assert!(result.outcomes.is_empty());
        assert!(!result.all_allowed()); // empty => not all_allowed

        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 10 });
        result.add_outcome(
            2,
            PaneBroadcastOutcome::Denied {
                reason: "policy".into(),
            },
        );
        result.add_outcome(
            3,
            PaneBroadcastOutcome::PreconditionFailed {
                failed: vec!["prompt_active".into()],
            },
        );
        result.add_outcome(
            4,
            PaneBroadcastOutcome::Skipped {
                reason: "locked".into(),
            },
        );

        assert_eq!(result.allowed_count(), 1);
        assert_eq!(result.denied_count(), 1);
        assert_eq!(result.precondition_failed_count(), 1);
        assert_eq!(result.skipped_count(), 1);
        assert!(!result.all_allowed());
    }

    #[test]
    fn broadcast_result_all_allowed() {
        let mut result = BroadcastResult::new("test");
        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 5 });
        result.add_outcome(2, PaneBroadcastOutcome::Allowed { elapsed_ms: 10 });
        assert!(result.all_allowed());
    }

    // ========================================================================
    // evaluate_pane_preconditions
    // ========================================================================

    #[test]
    fn evaluate_pane_preconditions_mixed() {
        let mut caps_map: HashMap<u64, PaneCapabilities> = HashMap::new();
        caps_map.insert(1, caps_ready());
        caps_map.insert(
            2,
            PaneCapabilities {
                prompt_active: false,
                alt_screen: Some(true),
                ..Default::default()
            },
        );
        // Pane 3 has no capabilities entry

        let preconds = default_broadcast_preconditions();
        let results = evaluate_pane_preconditions(&[1, 2, 3], &caps_map, &preconds);

        assert_eq!(results.len(), 3);

        // Pane 1: passes all preconditions
        assert!(results[0].1.is_none());

        // Pane 2: fails preconditions
        match &results[1].1 {
            Some(PaneBroadcastOutcome::PreconditionFailed { failed }) => {
                assert!(failed.contains(&"prompt_active".to_string()));
                assert!(failed.contains(&"not_alt_screen".to_string()));
            }
            other => panic!("Expected PreconditionFailed, got {other:?}"),
        }

        // Pane 3: skipped (no capabilities)
        match &results[2].1 {
            Some(PaneBroadcastOutcome::Skipped { reason }) => {
                assert!(reason.contains("no capabilities"));
            }
            other => panic!("Expected Skipped, got {other:?}"),
        }
    }

    // ========================================================================
    // plan_reread_context / plan_pause_all
    // ========================================================================

    #[test]
    fn plan_reread_context_basic() {
        let panes = vec![
            make_pane(1, "local", Some("codex"), None),
            make_pane(2, "local", Some("Claude Code"), None),
        ];
        let mut caps: HashMap<u64, PaneCapabilities> = HashMap::new();
        caps.insert(1, caps_ready());
        caps.insert(2, caps_ready());

        let config = CoordinateAgentsConfig::default();
        let result = plan_reread_context(&panes, &caps, &config);

        assert_eq!(result.operation, "reread_context");
        assert_eq!(result.total_acted(), 2);
        assert_eq!(result.broadcast.allowed_count(), 2);
    }

    #[test]
    fn plan_reread_context_with_failing_pane() {
        let panes = vec![
            make_pane(1, "local", Some("codex"), None),
            make_pane(2, "local", Some("Claude Code"), None),
        ];
        let mut caps: HashMap<u64, PaneCapabilities> = HashMap::new();
        caps.insert(1, caps_ready());
        caps.insert(
            2,
            PaneCapabilities {
                prompt_active: false,
                ..Default::default()
            },
        );

        let config = CoordinateAgentsConfig::default();
        let result = plan_reread_context(&panes, &caps, &config);

        assert_eq!(result.broadcast.allowed_count(), 1);
        assert_eq!(result.broadcast.precondition_failed_count(), 1);
    }

    #[test]
    fn plan_pause_all_only_checks_alt_screen() {
        let panes = vec![
            make_pane(1, "local", None, None),
            make_pane(2, "local", None, None),
        ];
        let mut caps: HashMap<u64, PaneCapabilities> = HashMap::new();
        // Pane 1: prompt not active, but that's fine for pause
        caps.insert(
            1,
            PaneCapabilities {
                prompt_active: false,
                alt_screen: Some(false),
                ..Default::default()
            },
        );
        // Pane 2: in alt screen — should fail
        caps.insert(2, PaneCapabilities::alt_screen());

        let config = CoordinateAgentsConfig::default();
        let result = plan_pause_all(&panes, &caps, &config);

        assert_eq!(result.broadcast.allowed_count(), 1);
        assert_eq!(result.broadcast.precondition_failed_count(), 1);
    }

    // ========================================================================
    // resolve_reread_prompts / resolve_pause_texts
    // ========================================================================

    #[test]
    fn resolve_reread_prompts_agent_specific() {
        let panes = vec![
            make_pane(1, "local", Some("codex-cli"), None),
            make_pane(2, "local", Some("Claude Code"), None),
            make_pane(3, "local", Some("Gemini session"), None),
            make_pane(4, "local", Some("zsh"), None),
        ];
        let prompts = resolve_reread_prompts(&panes);
        assert!(prompts[&1].contains("AGENTS.md"));
        assert!(prompts[&2].contains("/read"));
        assert!(prompts[&3].contains("AGENTS.md"));
        assert!(prompts[&4].contains("cat"));
    }

    #[test]
    fn resolve_pause_texts_are_ctrl_c() {
        let panes = vec![
            make_pane(1, "local", Some("codex"), None),
            make_pane(2, "local", Some("anything"), None),
        ];
        let texts = resolve_pause_texts(&panes);
        // All should be Ctrl-C (\x03)
        for text in texts.values() {
            assert_eq!(*text, "\x03");
        }
    }

    // ========================================================================
    // agent_reread_prompt / agent_pause_text
    // ========================================================================

    #[test]
    fn agent_reread_prompt_coverage() {
        assert!(agent_reread_prompt("codex").contains("AGENTS.md"));
        assert!(agent_reread_prompt("claude_code").contains("/read"));
        assert!(agent_reread_prompt("gemini").contains("AGENTS.md"));
        assert!(agent_reread_prompt("unknown").contains("cat"));
    }

    #[test]
    fn agent_pause_text_all_ctrl_c() {
        for hint in &["codex", "claude_code", "gemini", "unknown", "random"] {
            assert_eq!(agent_pause_text(hint), "\x03");
        }
    }

    // ========================================================================
    // truncate_snippet
    // ========================================================================

    #[test]
    fn truncate_snippet_short_string() {
        assert_eq!(truncate_snippet("hello", 10), "hello");
    }

    #[test]
    fn truncate_snippet_exact_length() {
        assert_eq!(truncate_snippet("12345", 5), "12345");
    }

    #[test]
    fn truncate_snippet_overflow() {
        let result = truncate_snippet("hello world", 8);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 8);
    }

    #[test]
    fn truncate_snippet_zero_max() {
        assert_eq!(truncate_snippet("anything", 0), "");
    }

    #[test]
    fn truncate_snippet_small_max() {
        assert_eq!(truncate_snippet("hello", 1), ".");
        assert_eq!(truncate_snippet("hello", 2), "..");
        assert_eq!(truncate_snippet("hello", 3), "...");
    }

    #[test]
    fn truncate_snippet_trims_whitespace() {
        assert_eq!(truncate_snippet("  hello  ", 100), "hello");
    }

    #[test]
    fn truncate_snippet_multibyte_utf8() {
        // Ensure we don't split in the middle of a UTF-8 character
        let s = "日本語テスト"; // 6 chars, 18 bytes
        let result = truncate_snippet(s, 10);
        assert!(result.ends_with("..."));
        // Should not panic or produce invalid UTF-8
        assert!(result.is_char_boundary(result.len()));
    }

    // ========================================================================
    // UnstickFindingKind serde
    // ========================================================================

    #[test]
    fn unstick_finding_kind_serde_roundtrip() {
        for kind in [
            UnstickFindingKind::TodoComment,
            UnstickFindingKind::PanicSite,
            UnstickFindingKind::SuppressedError,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: UnstickFindingKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn unstick_finding_kind_labels() {
        assert_eq!(UnstickFindingKind::TodoComment.label(), "TODO/FIXME");
        assert_eq!(UnstickFindingKind::PanicSite.label(), "panic site");
        assert_eq!(
            UnstickFindingKind::SuppressedError.label(),
            "suppressed error"
        );
    }

    // ========================================================================
    // UnstickReport
    // ========================================================================

    #[test]
    fn unstick_report_empty() {
        let report = UnstickReport::empty("text");
        assert_eq!(report.total_findings(), 0);
        assert_eq!(report.human_summary(), "No actionable findings.");
    }

    #[test]
    fn unstick_report_human_summary_with_findings() {
        let mut report = UnstickReport::empty("text");
        report.files_scanned = 5;
        report.findings.push(UnstickFinding {
            kind: UnstickFindingKind::TodoComment,
            file: "src/main.rs".to_string(),
            line: 42,
            snippet: "// TODO: fix this".to_string(),
            suggestion: "Address this TODO".to_string(),
        });
        report.counts.insert("TODO/FIXME".to_string(), 1);
        let summary = report.human_summary();
        assert!(summary.contains("Found 1 items"));
        assert!(summary.contains("src/main.rs:42"));
    }

    // ========================================================================
    // TextScanPatterns / scan_file_text
    // ========================================================================

    #[test]
    fn scan_file_text_finds_todo() {
        let dir = std::env::temp_dir().join("ft_coord_test_todo");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test.rs");
        std::fs::write(&file, "fn main() {\n    // TODO: fix this\n}\n").unwrap();

        let patterns = TextScanPatterns::new();
        let mut kind_counts = HashMap::new();
        let findings = scan_file_text(&file, &dir, &patterns, 10, &mut kind_counts);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, UnstickFindingKind::TodoComment);
        assert_eq!(findings[0].line, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_file_text_finds_panic_site() {
        let dir = std::env::temp_dir().join("ft_coord_test_panic");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test.rs");
        std::fs::write(
            &file,
            "let x = foo.unwrap();\nlet y = bar.expect(\"msg\");\n",
        )
        .unwrap();

        let patterns = TextScanPatterns::new();
        let mut kind_counts = HashMap::new();
        let findings = scan_file_text(&file, &dir, &patterns, 10, &mut kind_counts);

        assert!(
            findings
                .iter()
                .any(|f| f.kind == UnstickFindingKind::PanicSite)
        );
        assert!(findings.len() >= 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_file_text_respects_max_per_kind() {
        let dir = std::env::temp_dir().join("ft_coord_test_max");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test.rs");
        let content = (0..20)
            .map(|i| format!("// TODO item {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file, content).unwrap();

        let patterns = TextScanPatterns::new();
        let mut kind_counts = HashMap::new();
        let findings = scan_file_text(&file, &dir, &patterns, 3, &mut kind_counts);

        // Only 3 TODOs should be captured
        let todo_count = findings
            .iter()
            .filter(|f| f.kind == UnstickFindingKind::TodoComment)
            .count();
        assert_eq!(todo_count, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ========================================================================
    // CoordinateAgentsConfig serde
    // ========================================================================

    #[test]
    fn coordinate_agents_config_defaults() {
        let config = CoordinateAgentsConfig::default();
        assert_eq!(config.strategy, PaneGroupStrategy::ByAgent);
        assert_eq!(config.preconditions.len(), 4);
        assert!(!config.abort_on_lock_failure);
    }

    #[test]
    fn coordinate_agents_config_serde_roundtrip() {
        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::ByDomain,
            preconditions: vec![BroadcastPrecondition::PromptActive],
            abort_on_lock_failure: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: CoordinateAgentsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.strategy, PaneGroupStrategy::ByDomain);
        assert!(parsed.abort_on_lock_failure);
        assert_eq!(parsed.preconditions.len(), 1);
    }

    // ========================================================================
    // PaneGroup helpers
    // ========================================================================

    #[test]
    fn pane_group_len_and_is_empty() {
        let group = PaneGroup::new("test", vec![1, 2, 3], PaneGroupStrategy::ByDomain);
        assert_eq!(group.len(), 3);
        assert!(!group.is_empty());

        let empty = PaneGroup::new("empty", vec![], PaneGroupStrategy::ByDomain);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
    }

    // ========================================================================
    // CoordinationResult helpers
    // ========================================================================

    #[test]
    fn coordination_result_totals() {
        let mut result = CoordinationResult::new("test_op");
        assert_eq!(result.total_panes(), 0);
        assert_eq!(result.total_acted(), 0);

        result.groups.push(GroupCoordinationEntry {
            group_name: "group1".to_string(),
            pane_count: 5,
            acted_count: 3,
            precondition_failed_count: 1,
            skipped_count: 1,
        });
        result.groups.push(GroupCoordinationEntry {
            group_name: "group2".to_string(),
            pane_count: 2,
            acted_count: 2,
            precondition_failed_count: 0,
            skipped_count: 0,
        });

        assert_eq!(result.total_panes(), 7);
        assert_eq!(result.total_acted(), 5);
    }

    // ========================================================================
    // GroupLockResult with PaneWorkflowLockManager
    // ========================================================================

    #[test]
    fn try_acquire_group_all_free() {
        let mgr = PaneWorkflowLockManager::new();
        let result = mgr.try_acquire_group(&[1, 2, 3], "workflow_a", "exec-1");
        assert!(result.is_acquired());
        if let GroupLockResult::Acquired { locked_panes } = result {
            assert_eq!(locked_panes, vec![1, 2, 3]);
        }
        // Cleanup
        mgr.release_group(&[1, 2, 3], "exec-1");
    }

    #[test]
    fn try_acquire_group_partial_conflict_rolls_back() {
        let mgr = PaneWorkflowLockManager::new();

        // Lock pane 2 first
        assert!(mgr.try_acquire(2, "workflow_x", "exec-x").is_acquired());

        // Now try to acquire group [1, 2, 3] — should fail on pane 2 and roll back
        let result = mgr.try_acquire_group(&[1, 2, 3], "workflow_a", "exec-1");
        assert!(!result.is_acquired());

        if let GroupLockResult::PartialFailure {
            conflicts,
            would_have_locked,
        } = &result
        {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].pane_id, 2);
            assert_eq!(conflicts[0].held_by_workflow, "workflow_x");
            // Pane 1 was acquired then rolled back
            assert!(would_have_locked.contains(&1));
        }

        // Pane 1 should be free again (rolled back)
        assert!(mgr.is_locked(1).is_none());
        // Pane 2 still locked by original holder
        assert!(mgr.is_locked(2).is_some());

        mgr.release(2, "exec-x");
    }

    #[test]
    fn release_group_returns_count() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire_group(&[1, 2, 3], "wf", "e1");
        let released = mgr.release_group(&[1, 2, 3], "e1");
        assert_eq!(released, 3);
        // Releasing again returns 0
        let released = mgr.release_group(&[1, 2, 3], "e1");
        assert_eq!(released, 0);
    }

    // ========================================================================
    // PaneBroadcastOutcome serde
    // ========================================================================

    #[test]
    fn pane_broadcast_outcome_serde_roundtrip() {
        let outcomes = vec![
            PaneBroadcastOutcome::Allowed { elapsed_ms: 42 },
            PaneBroadcastOutcome::Denied {
                reason: "policy".into(),
            },
            PaneBroadcastOutcome::PreconditionFailed {
                failed: vec!["prompt_active".into()],
            },
            PaneBroadcastOutcome::Skipped {
                reason: "locked".into(),
            },
            PaneBroadcastOutcome::VerificationFailed {
                reason: "timeout".into(),
            },
        ];
        for outcome in outcomes {
            let json = serde_json::to_string(&outcome).unwrap();
            let parsed: PaneBroadcastOutcome = serde_json::from_str(&json).unwrap();
            // Just check round-trip doesn't panic; structural equality requires PartialEq
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    // ========================================================================
    // BroadcastPrecondition serde
    // ========================================================================

    #[test]
    fn broadcast_precondition_serde_roundtrip() {
        for p in [
            BroadcastPrecondition::PromptActive,
            BroadcastPrecondition::NotAltScreen,
            BroadcastPrecondition::NoRecentGap,
            BroadcastPrecondition::NotReserved,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let parsed: BroadcastPrecondition = serde_json::from_str(&json).unwrap();
            assert_eq!(p.label(), parsed.label());
        }
    }

    // ========================================================================
    // UnstickConfig defaults
    // ========================================================================

    #[test]
    fn unstick_config_defaults() {
        let config = UnstickConfig::default();
        assert_eq!(config.max_findings_per_kind, 10);
        assert_eq!(config.max_total_findings, 25);
        assert!(config.extensions.contains(&"rs".to_string()));
        assert!(config.extensions.contains(&"py".to_string()));
    }
}
