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
