//! Expanded robot API surface for NTM-gap command domains (ft-3681t.4.1).
//!
//! Defines request/response types for robot command families that cover
//! NTM operational workflows not yet present in the existing `ft robot`
//! surface. Each command family maps to existing FrankenTerm infrastructure
//! and carries an explicit NTM equivalence annotation for parity tracking.
//!
//! # Command Families
//!
//! ```text
//! RobotNtmCommand
//!   ├── Checkpoint  — session snapshot save/list/show/delete/rollback
//!   ├── Context     — context-window budget status/rotate/compact
//!   ├── Work        — dependency-aware claim/release/list/assign
//!   ├── Fleet       — swarm scale/status/rebalance
//!   └── Profile     — session profile list/show/apply
//! ```
//!
//! # NTM Equivalence
//!
//! Each command carries an [`NtmEquivalence`] that maps it to one or more
//! NTM commands from the capability census (ft-3681t.1.1), enabling the
//! parity corpus and shadow comparator to track convergence progress.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Top-level command dispatch
// =============================================================================

/// Expanded robot command families covering NTM operational workflow gaps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "family", content = "command", rename_all = "snake_case")]
pub enum RobotNtmCommand {
    /// Session checkpoint management.
    Checkpoint(CheckpointCommand),
    /// Context-window budget management.
    Context(ContextCommand),
    /// Dependency-aware work queue operations.
    Work(WorkCommand),
    /// Fleet scaling and rebalancing.
    Fleet(FleetCommand),
    /// Session profile management.
    Profile(ProfileCommand),
}

impl RobotNtmCommand {
    /// The command family name as used in `ft robot <family> <action>`.
    #[must_use]
    pub fn family_name(&self) -> &'static str {
        match self {
            Self::Checkpoint(_) => "checkpoint",
            Self::Context(_) => "context",
            Self::Work(_) => "work",
            Self::Fleet(_) => "fleet",
            Self::Profile(_) => "profile",
        }
    }

    /// The specific action name within the family.
    #[must_use]
    pub fn action_name(&self) -> &'static str {
        match self {
            Self::Checkpoint(c) => c.action_name(),
            Self::Context(c) => c.action_name(),
            Self::Work(c) => c.action_name(),
            Self::Fleet(c) => c.action_name(),
            Self::Profile(c) => c.action_name(),
        }
    }

    /// NTM equivalence mapping for parity tracking.
    #[must_use]
    pub fn ntm_equivalence(&self) -> NtmEquivalence {
        match self {
            Self::Checkpoint(c) => c.ntm_equivalence(),
            Self::Context(c) => c.ntm_equivalence(),
            Self::Work(c) => c.ntm_equivalence(),
            Self::Fleet(c) => c.ntm_equivalence(),
            Self::Profile(c) => c.ntm_equivalence(),
        }
    }

    /// Whether this command mutates state (vs read-only query).
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        match self {
            Self::Checkpoint(c) => c.is_mutation(),
            Self::Context(c) => c.is_mutation(),
            Self::Work(c) => c.is_mutation(),
            Self::Fleet(c) => c.is_mutation(),
            Self::Profile(c) => c.is_mutation(),
        }
    }
}

// =============================================================================
// NTM equivalence mapping
// =============================================================================

/// Maps an `ft robot` command to its NTM equivalent(s) from the capability census.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NtmEquivalence {
    /// The NTM command(s) this replaces (e.g., `"ntm checkpoint save"`).
    pub ntm_commands: Vec<String>,
    /// Census domain (e.g., "F. Session Persistence & Recovery").
    pub census_domain: String,
    /// Convergence classification.
    pub classification: ConvergenceClassification,
}

/// How this command relates to NTM capability convergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvergenceClassification {
    /// Direct 1:1 replacement of an NTM command.
    DirectReplacement,
    /// Covers the same capability but with improved semantics.
    Upgrade,
    /// New capability that NTM did not have.
    Novel,
    /// Partial coverage — some NTM semantics not yet replicated.
    Partial,
}

impl ConvergenceClassification {
    /// Human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DirectReplacement => "direct-replacement",
            Self::Upgrade => "upgrade",
            Self::Novel => "novel",
            Self::Partial => "partial",
        }
    }
}

// =============================================================================
// Checkpoint commands (NTM domain F: Session Persistence & Recovery)
// =============================================================================

/// Checkpoint management commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CheckpointCommand {
    /// Save a checkpoint of the current session state.
    Save(CheckpointSaveRequest),
    /// List available checkpoints.
    List(CheckpointListRequest),
    /// Show details of a specific checkpoint.
    Show(CheckpointShowRequest),
    /// Delete a checkpoint.
    Delete(CheckpointDeleteRequest),
    /// Rollback to a previous checkpoint.
    Rollback(CheckpointRollbackRequest),
}

impl CheckpointCommand {
    fn action_name(&self) -> &'static str {
        match self {
            Self::Save(_) => "save",
            Self::List(_) => "list",
            Self::Show(_) => "show",
            Self::Delete(_) => "delete",
            Self::Rollback(_) => "rollback",
        }
    }

    fn is_mutation(&self) -> bool {
        !matches!(self, Self::List(_) | Self::Show(_))
    }

    fn ntm_equivalence(&self) -> NtmEquivalence {
        let (cmds, classification) = match self {
            Self::Save(_) => (
                vec!["ntm checkpoint save"],
                ConvergenceClassification::Upgrade,
            ),
            Self::List(_) => (
                vec!["ntm checkpoint list"],
                ConvergenceClassification::DirectReplacement,
            ),
            Self::Show(_) => (
                vec!["ntm checkpoint show"],
                ConvergenceClassification::Upgrade,
            ),
            Self::Delete(_) => (
                vec!["ntm checkpoint delete"],
                ConvergenceClassification::DirectReplacement,
            ),
            Self::Rollback(_) => (
                vec!["ntm rollback", "ntm checkpoint restore"],
                ConvergenceClassification::Upgrade,
            ),
        };
        NtmEquivalence {
            ntm_commands: cmds.into_iter().map(String::from).collect(),
            census_domain: "F. Session Persistence & Recovery".to_string(),
            classification,
        }
    }
}

/// Request to save a session checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSaveRequest {
    /// Optional human-readable label for this checkpoint.
    #[serde(default)]
    pub label: Option<String>,
    /// Whether to include scrollback content (larger but more complete).
    #[serde(default)]
    pub include_scrollback: bool,
    /// Specific pane IDs to checkpoint (empty = all panes).
    #[serde(default)]
    pub pane_ids: Vec<u64>,
}

/// Request to list checkpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointListRequest {
    /// Maximum number of checkpoints to return.
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    /// Offset for pagination.
    #[serde(default)]
    pub offset: usize,
}

/// Request to show a specific checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointShowRequest {
    /// Checkpoint ID to show.
    pub checkpoint_id: String,
}

/// Request to delete a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointDeleteRequest {
    /// Checkpoint ID to delete.
    pub checkpoint_id: String,
}

/// Request to rollback to a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRollbackRequest {
    /// Checkpoint ID to rollback to.
    pub checkpoint_id: String,
    /// If true, preview the rollback without applying.
    #[serde(default)]
    pub dry_run: bool,
}

/// Response data for checkpoint save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSaveData {
    /// Generated checkpoint ID.
    pub checkpoint_id: String,
    /// Label if provided.
    pub label: Option<String>,
    /// Number of panes captured.
    pub pane_count: usize,
    /// Total bytes persisted.
    pub bytes_persisted: u64,
    /// Whether scrollback was included.
    pub scrollback_included: bool,
    /// Timestamp (epoch ms).
    pub created_at: u64,
}

/// Response data for checkpoint list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointListData {
    /// Available checkpoints (newest first).
    pub checkpoints: Vec<CheckpointSummary>,
    /// Total checkpoint count (for pagination).
    pub total: usize,
}

/// Summary of a single checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSummary {
    /// Checkpoint ID.
    pub checkpoint_id: String,
    /// Optional label.
    pub label: Option<String>,
    /// Number of panes in this checkpoint.
    pub pane_count: usize,
    /// Size in bytes.
    pub size_bytes: u64,
    /// When created (epoch ms).
    pub created_at: u64,
}

/// Response data for checkpoint show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointShowData {
    /// Checkpoint ID.
    pub checkpoint_id: String,
    /// Optional label.
    pub label: Option<String>,
    /// Pane snapshots in this checkpoint.
    pub panes: Vec<CheckpointPaneSnapshot>,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Content hash (BLAKE3) for dedup.
    pub content_hash: String,
    /// When created (epoch ms).
    pub created_at: u64,
}

/// Per-pane data within a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPaneSnapshot {
    /// Pane ID.
    pub pane_id: u64,
    /// Pane title at checkpoint time.
    pub title: String,
    /// Working directory at checkpoint time.
    pub working_dir: Option<String>,
    /// Whether scrollback was captured for this pane.
    pub has_scrollback: bool,
    /// Number of scrollback lines captured.
    pub scrollback_lines: usize,
}

/// Response data for checkpoint delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointDeleteData {
    /// Deleted checkpoint ID.
    pub checkpoint_id: String,
    /// Bytes freed.
    pub bytes_freed: u64,
}

/// Response data for checkpoint rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRollbackData {
    /// Checkpoint rolled back to.
    pub checkpoint_id: String,
    /// Number of panes restored.
    pub panes_restored: usize,
    /// Number of panes that could not be restored (e.g., layout changed).
    pub panes_skipped: usize,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Warnings generated during rollback.
    #[serde(default)]
    pub warnings: Vec<String>,
}

// =============================================================================
// Context commands (NTM domain H: Context & Memory Management)
// =============================================================================

/// Context-window budget management commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ContextCommand {
    /// Show context budget status for one or all panes.
    Status(ContextStatusRequest),
    /// Trigger context rotation for a pane (send compaction signal).
    Rotate(ContextRotateRequest),
    /// Get compaction history for a pane.
    History(ContextHistoryRequest),
}

impl ContextCommand {
    fn action_name(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::Rotate(_) => "rotate",
            Self::History(_) => "history",
        }
    }

    fn is_mutation(&self) -> bool {
        matches!(self, Self::Rotate(_))
    }

    fn ntm_equivalence(&self) -> NtmEquivalence {
        let (cmds, classification) = match self {
            Self::Status(_) => (
                vec!["ntm context status", "ntm memory"],
                ConvergenceClassification::Upgrade,
            ),
            Self::Rotate(_) => (
                vec!["ntm rotate context"],
                ConvergenceClassification::Upgrade,
            ),
            Self::History(_) => (
                vec!["ntm context history"],
                ConvergenceClassification::Novel,
            ),
        };
        NtmEquivalence {
            ntm_commands: cmds.into_iter().map(String::from).collect(),
            census_domain: "H. Context & Memory Management".to_string(),
            classification,
        }
    }
}

/// Request for context budget status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextStatusRequest {
    /// Specific pane ID (None = fleet-wide summary).
    #[serde(default)]
    pub pane_id: Option<u64>,
}

/// Request to trigger context rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRotateRequest {
    /// Pane ID to rotate.
    pub pane_id: u64,
    /// Rotation strategy hint.
    #[serde(default)]
    pub strategy: RotationStrategy,
}

/// Strategy for context rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RotationStrategy {
    /// Let the agent choose its default compaction approach.
    #[default]
    AgentDefault,
    /// Aggressive compaction to free maximum context.
    Aggressive,
    /// Gentle compaction preserving recent context.
    Gentle,
}

/// Request for compaction history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHistoryRequest {
    /// Pane ID.
    pub pane_id: u64,
    /// Maximum entries to return.
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

/// Response data for context status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextStatusData {
    /// Per-pane context budget snapshots.
    pub panes: Vec<PaneContextStatus>,
    /// Fleet-wide pressure summary.
    pub fleet_pressure: FleetContextPressure,
}

/// Context status for a single pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneContextStatus {
    /// Pane ID.
    pub pane_id: u64,
    /// Current pressure tier.
    pub pressure_tier: String,
    /// Estimated utilization ratio (0.0..1.0).
    pub utilization: f64,
    /// Estimated tokens consumed.
    pub tokens_consumed: u64,
    /// Estimated token budget.
    pub token_budget: u64,
    /// Number of compaction events so far.
    pub compaction_count: u32,
    /// Time since last compaction (ms), if any.
    pub ms_since_last_compaction: Option<u64>,
}

/// Fleet-wide context pressure summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetContextPressure {
    /// Total panes tracked.
    pub total_panes: usize,
    /// Panes in green pressure.
    pub green_count: usize,
    /// Panes in yellow pressure.
    pub yellow_count: usize,
    /// Panes in red pressure.
    pub red_count: usize,
    /// Panes in black (critical) pressure.
    pub black_count: usize,
}

/// Response data for context rotate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRotateData {
    /// Pane ID that was rotated.
    pub pane_id: u64,
    /// Whether rotation signal was accepted.
    pub accepted: bool,
    /// Reason if not accepted.
    pub reason: Option<String>,
    /// Strategy used.
    pub strategy: RotationStrategy,
}

/// Response data for context history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHistoryData {
    /// Pane ID.
    pub pane_id: u64,
    /// Compaction events (newest first).
    pub events: Vec<CompactionEvent>,
}

/// A single compaction event in the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// When the compaction occurred (epoch ms).
    pub timestamp_ms: u64,
    /// Utilization before compaction.
    pub utilization_before: f64,
    /// Utilization after compaction.
    pub utilization_after: f64,
    /// Tokens freed by this compaction.
    pub tokens_freed: u64,
    /// What triggered the compaction.
    pub trigger: String,
}

// =============================================================================
// Work commands (NTM domain J: Workflow & Task Orchestration)
// =============================================================================

/// Dependency-aware work queue commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkCommand {
    /// Claim a work item for an agent.
    Claim(WorkClaimRequest),
    /// Release a claimed work item back to the queue.
    Release(WorkReleaseRequest),
    /// Complete a work item.
    Complete(WorkCompleteRequest),
    /// List work items with optional filters.
    List(WorkListRequest),
    /// Show the ready set (items available for claiming).
    Ready(WorkReadyRequest),
    /// Assign a work item to a specific agent.
    Assign(WorkAssignRequest),
}

impl WorkCommand {
    fn action_name(&self) -> &'static str {
        match self {
            Self::Claim(_) => "claim",
            Self::Release(_) => "release",
            Self::Complete(_) => "complete",
            Self::List(_) => "list",
            Self::Ready(_) => "ready",
            Self::Assign(_) => "assign",
        }
    }

    fn is_mutation(&self) -> bool {
        !matches!(self, Self::List(_) | Self::Ready(_))
    }

    fn ntm_equivalence(&self) -> NtmEquivalence {
        let (cmds, classification) = match self {
            Self::Claim(_) => (vec!["ntm work claim"], ConvergenceClassification::Upgrade),
            Self::Release(_) => (
                vec!["ntm work release"],
                ConvergenceClassification::DirectReplacement,
            ),
            Self::Complete(_) => (
                vec!["ntm work complete"],
                ConvergenceClassification::DirectReplacement,
            ),
            Self::List(_) => (
                vec!["ntm work list", "ntm marching-orders"],
                ConvergenceClassification::Upgrade,
            ),
            Self::Ready(_) => (vec!["ntm work ready"], ConvergenceClassification::Novel),
            Self::Assign(_) => (vec!["ntm work assign"], ConvergenceClassification::Upgrade),
        };
        NtmEquivalence {
            ntm_commands: cmds.into_iter().map(String::from).collect(),
            census_domain: "J. Workflow & Task Orchestration".to_string(),
            classification,
        }
    }
}

/// Request to claim a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkClaimRequest {
    /// Work item ID to claim.
    pub item_id: String,
    /// Agent slot ID claiming the work.
    pub agent_id: String,
}

/// Request to release a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkReleaseRequest {
    /// Work item ID to release.
    pub item_id: String,
    /// Reason for release.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request to complete a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCompleteRequest {
    /// Work item ID to complete.
    pub item_id: String,
    /// Completion summary.
    #[serde(default)]
    pub summary: Option<String>,
    /// Evidence references (commit hashes, artifact paths).
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// Request to list work items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkListRequest {
    /// Filter by status.
    #[serde(default)]
    pub status_filter: Option<String>,
    /// Filter by assigned agent.
    #[serde(default)]
    pub agent_filter: Option<String>,
    /// Filter by label.
    #[serde(default)]
    pub label_filter: Option<String>,
    /// Maximum items to return.
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

/// Request for the ready set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkReadyRequest {
    /// Optional agent ID for capability-filtered ready set.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Maximum items to return.
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

/// Request to assign work to a specific agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAssignRequest {
    /// Work item ID.
    pub item_id: String,
    /// Agent to assign to.
    pub agent_id: String,
    /// Assignment strategy override.
    #[serde(default)]
    pub strategy: Option<String>,
}

/// Response data for work claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkClaimData {
    /// Work item ID claimed.
    pub item_id: String,
    /// Agent that claimed it.
    pub agent_id: String,
    /// Item title.
    pub title: String,
    /// Item priority.
    pub priority: u32,
    /// Timestamp of claim (epoch ms).
    pub claimed_at: u64,
}

/// Response data for work release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkReleaseData {
    /// Work item ID released.
    pub item_id: String,
    /// New status after release.
    pub new_status: String,
}

/// Response data for work complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCompleteData {
    /// Work item ID completed.
    pub item_id: String,
    /// Items unblocked by this completion.
    pub unblocked: Vec<String>,
}

/// Response data for work list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkListData {
    /// Work items matching the filter.
    pub items: Vec<WorkItemSummary>,
    /// Total count (for pagination).
    pub total: usize,
}

/// Summary of a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemSummary {
    /// Work item ID.
    pub id: String,
    /// Title.
    pub title: String,
    /// Priority.
    pub priority: u32,
    /// Current status.
    pub status: String,
    /// Assigned agent (if any).
    pub assigned_to: Option<String>,
    /// Labels.
    pub labels: Vec<String>,
    /// Number of unmet dependencies.
    pub blocked_by_count: usize,
    /// Number of items this unblocks.
    pub unblocks_count: usize,
}

/// Response data for work ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkReadyData {
    /// Ready items (sorted by priority).
    pub items: Vec<WorkItemSummary>,
    /// Total ready count.
    pub total_ready: usize,
    /// Total blocked count.
    pub total_blocked: usize,
}

/// Response data for work assign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAssignData {
    /// Work item ID assigned.
    pub item_id: String,
    /// Agent assigned to.
    pub agent_id: String,
    /// Assignment strategy used.
    pub strategy_used: String,
}

// =============================================================================
// Fleet commands (NTM domain R: Multi-Project Orchestration + B: Agent Management)
// =============================================================================

/// Fleet scaling and rebalancing commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FleetCommand {
    /// Show fleet status (agents, allocations, health).
    Status(FleetStatusRequest),
    /// Scale agents up or down.
    Scale(FleetScaleRequest),
    /// Rebalance work across agents.
    Rebalance(FleetRebalanceRequest),
    /// List agent slots and their assignments.
    Agents(FleetAgentsRequest),
}

impl FleetCommand {
    fn action_name(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::Scale(_) => "scale",
            Self::Rebalance(_) => "rebalance",
            Self::Agents(_) => "agents",
        }
    }

    fn is_mutation(&self) -> bool {
        matches!(self, Self::Scale(_) | Self::Rebalance(_))
    }

    fn ntm_equivalence(&self) -> NtmEquivalence {
        let (cmds, classification) = match self {
            Self::Status(_) => (
                vec!["ntm swarm status", "ntm controller status"],
                ConvergenceClassification::Upgrade,
            ),
            Self::Scale(_) => (
                vec!["ntm agent scale", "ntm spawn"],
                ConvergenceClassification::Upgrade,
            ),
            Self::Rebalance(_) => (
                vec!["ntm swarm rebalance"],
                ConvergenceClassification::Novel,
            ),
            Self::Agents(_) => (
                vec!["ntm agent list", "ntm status"],
                ConvergenceClassification::Upgrade,
            ),
        };
        NtmEquivalence {
            ntm_commands: cmds.into_iter().map(String::from).collect(),
            census_domain: "R. Multi-Project Orchestration / B. Agent Management".to_string(),
            classification,
        }
    }
}

/// Request for fleet status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStatusRequest {
    /// Include per-agent detail (vs summary only).
    #[serde(default)]
    pub detailed: bool,
}

/// Request to scale agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetScaleRequest {
    /// Target agent program (e.g., "claude_code", "codex").
    pub program: String,
    /// Desired count.
    pub target_count: u32,
    /// If true, preview only.
    #[serde(default)]
    pub dry_run: bool,
}

/// Request to rebalance work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRebalanceRequest {
    /// Rebalance strategy.
    #[serde(default)]
    pub strategy: RebalanceStrategy,
    /// If true, preview only.
    #[serde(default)]
    pub dry_run: bool,
}

/// Strategy for fleet rebalancing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RebalanceStrategy {
    /// Redistribute based on load (default).
    #[default]
    LoadBased,
    /// Redistribute based on agent capability scores.
    CapabilityBased,
    /// Round-robin assignment.
    RoundRobin,
}

/// Request to list agent slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetAgentsRequest {
    /// Filter by program type.
    #[serde(default)]
    pub program_filter: Option<String>,
    /// Filter by state (idle/busy/stalled).
    #[serde(default)]
    pub state_filter: Option<String>,
}

/// Response data for fleet status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStatusData {
    /// Total agent slots.
    pub total_agents: usize,
    /// Active (non-idle) agents.
    pub active_agents: usize,
    /// Idle agents.
    pub idle_agents: usize,
    /// Stalled agents.
    pub stalled_agents: usize,
    /// Agent breakdown by program.
    pub by_program: HashMap<String, ProgramSlotSummary>,
    /// Work queue summary.
    pub work_queue: WorkQueueSummary,
}

/// Per-program agent slot summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramSlotSummary {
    /// Total slots for this program.
    pub count: usize,
    /// Active slots.
    pub active: usize,
    /// Idle slots.
    pub idle: usize,
}

/// Work queue summary within fleet status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkQueueSummary {
    /// Total items in queue.
    pub total_items: usize,
    /// Ready items.
    pub ready: usize,
    /// Blocked items.
    pub blocked: usize,
    /// In-progress items.
    pub in_progress: usize,
    /// Completed items.
    pub completed: usize,
}

/// Response data for fleet scale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetScaleData {
    /// Program that was scaled.
    pub program: String,
    /// Previous count.
    pub previous_count: u32,
    /// New count.
    pub new_count: u32,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Pane IDs of newly spawned agents.
    #[serde(default)]
    pub spawned_pane_ids: Vec<u64>,
    /// Pane IDs of terminated agents.
    #[serde(default)]
    pub terminated_pane_ids: Vec<u64>,
}

/// Response data for fleet rebalance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRebalanceData {
    /// Strategy used.
    pub strategy: RebalanceStrategy,
    /// Number of work items reassigned.
    pub items_reassigned: usize,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Per-item reassignment details.
    pub reassignments: Vec<RebalanceAction>,
}

/// A single rebalance reassignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebalanceAction {
    /// Work item ID.
    pub item_id: String,
    /// Previous agent.
    pub from_agent: Option<String>,
    /// New agent.
    pub to_agent: String,
    /// Reason for reassignment.
    pub reason: String,
}

/// Response data for fleet agents list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetAgentsData {
    /// Agent slot details.
    pub agents: Vec<AgentSlotInfo>,
}

/// Info about a single agent slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSlotInfo {
    /// Agent slot ID.
    pub slot_id: String,
    /// Pane ID.
    pub pane_id: u64,
    /// Agent program type.
    pub program: String,
    /// Current state.
    pub state: String,
    /// Currently assigned work item ID.
    pub assigned_work: Option<String>,
    /// Uptime in seconds.
    pub uptime_secs: u64,
    /// Metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

// =============================================================================
// Profile commands (NTM domain O: Project & Template Management)
// =============================================================================

/// Session profile management commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ProfileCommand {
    /// List available profiles.
    List(ProfileListRequest),
    /// Show details of a profile.
    Show(ProfileShowRequest),
    /// Apply a profile to spawn or configure panes.
    Apply(ProfileApplyRequest),
    /// Validate a profile definition.
    Validate(ProfileValidateRequest),
}

impl ProfileCommand {
    fn action_name(&self) -> &'static str {
        match self {
            Self::List(_) => "list",
            Self::Show(_) => "show",
            Self::Apply(_) => "apply",
            Self::Validate(_) => "validate",
        }
    }

    fn is_mutation(&self) -> bool {
        matches!(self, Self::Apply(_))
    }

    fn ntm_equivalence(&self) -> NtmEquivalence {
        let (cmds, classification) = match self {
            Self::List(_) => (
                vec!["ntm profiles list", "ntm session-templates list"],
                ConvergenceClassification::Upgrade,
            ),
            Self::Show(_) => (
                vec!["ntm profiles show", "ntm session-templates show"],
                ConvergenceClassification::Upgrade,
            ),
            Self::Apply(_) => (
                vec![
                    "ntm spawn",
                    "ntm session-templates apply",
                    "ntm personas apply",
                ],
                ConvergenceClassification::Upgrade,
            ),
            Self::Validate(_) => (vec![], ConvergenceClassification::Novel),
        };
        NtmEquivalence {
            ntm_commands: cmds.into_iter().map(String::from).collect(),
            census_domain: "O. Project & Template Management".to_string(),
            classification,
        }
    }
}

/// Request to list profiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileListRequest {
    /// Filter by role.
    #[serde(default)]
    pub role_filter: Option<String>,
    /// Filter by tag.
    #[serde(default)]
    pub tag_filter: Option<String>,
}

/// Request to show a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileShowRequest {
    /// Profile name.
    pub name: String,
}

/// Request to apply a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileApplyRequest {
    /// Profile name to apply.
    pub name: String,
    /// Number of panes to spawn with this profile.
    #[serde(default = "default_count_one")]
    pub count: u32,
    /// Override environment variables.
    #[serde(default)]
    pub env_overrides: HashMap<String, String>,
    /// If true, preview only.
    #[serde(default)]
    pub dry_run: bool,
}

/// Request to validate a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileValidateRequest {
    /// Profile name to validate.
    pub name: String,
}

/// Response data for profile list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileListData {
    /// Available profiles.
    pub profiles: Vec<ProfileSummary>,
}

/// Summary of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSummary {
    /// Profile name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Role classification.
    pub role: String,
    /// Tags.
    pub tags: Vec<String>,
}

/// Response data for profile show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileShowData {
    /// Profile name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Role.
    pub role: String,
    /// Spawn command.
    pub spawn_command: Option<String>,
    /// Environment variables.
    pub environment: HashMap<String, String>,
    /// Working directory.
    pub working_directory: Option<String>,
    /// Layout template name.
    pub layout_template: Option<String>,
    /// Bootstrap commands.
    pub bootstrap_commands: Vec<String>,
    /// Tags.
    pub tags: Vec<String>,
}

/// Response data for profile apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileApplyData {
    /// Profile applied.
    pub profile_name: String,
    /// Panes spawned.
    pub panes_spawned: Vec<u64>,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// Response data for profile validate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileValidateData {
    /// Profile name.
    pub name: String,
    /// Whether the profile is valid.
    pub valid: bool,
    /// Validation issues (empty if valid).
    pub issues: Vec<String>,
}

// =============================================================================
// Surface registry — enumerate all expanded API surfaces
// =============================================================================

/// Expanded API surface identifiers for the NTM-gap command families.
///
/// Complements `robot_api_contracts::ApiSurface` with the new command families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NtmApiSurface {
    CheckpointSave,
    CheckpointList,
    CheckpointShow,
    CheckpointDelete,
    CheckpointRollback,
    ContextStatus,
    ContextRotate,
    ContextHistory,
    WorkClaim,
    WorkRelease,
    WorkComplete,
    WorkList,
    WorkReady,
    WorkAssign,
    FleetStatus,
    FleetScale,
    FleetRebalance,
    FleetAgents,
    ProfileList,
    ProfileShow,
    ProfileApply,
    ProfileValidate,
}

impl NtmApiSurface {
    /// All defined NTM expansion surfaces.
    pub const ALL: &'static [NtmApiSurface] = &[
        Self::CheckpointSave,
        Self::CheckpointList,
        Self::CheckpointShow,
        Self::CheckpointDelete,
        Self::CheckpointRollback,
        Self::ContextStatus,
        Self::ContextRotate,
        Self::ContextHistory,
        Self::WorkClaim,
        Self::WorkRelease,
        Self::WorkComplete,
        Self::WorkList,
        Self::WorkReady,
        Self::WorkAssign,
        Self::FleetStatus,
        Self::FleetScale,
        Self::FleetRebalance,
        Self::FleetAgents,
        Self::ProfileList,
        Self::ProfileShow,
        Self::ProfileApply,
        Self::ProfileValidate,
    ];

    /// Command path as used in `ft robot <family> <action>`.
    #[must_use]
    pub fn command_path(&self) -> &'static str {
        match self {
            Self::CheckpointSave => "checkpoint save",
            Self::CheckpointList => "checkpoint list",
            Self::CheckpointShow => "checkpoint show",
            Self::CheckpointDelete => "checkpoint delete",
            Self::CheckpointRollback => "checkpoint rollback",
            Self::ContextStatus => "context status",
            Self::ContextRotate => "context rotate",
            Self::ContextHistory => "context history",
            Self::WorkClaim => "work claim",
            Self::WorkRelease => "work release",
            Self::WorkComplete => "work complete",
            Self::WorkList => "work list",
            Self::WorkReady => "work ready",
            Self::WorkAssign => "work assign",
            Self::FleetStatus => "fleet status",
            Self::FleetScale => "fleet scale",
            Self::FleetRebalance => "fleet rebalance",
            Self::FleetAgents => "fleet agents",
            Self::ProfileList => "profile list",
            Self::ProfileShow => "profile show",
            Self::ProfileApply => "profile apply",
            Self::ProfileValidate => "profile validate",
        }
    }

    /// Whether this surface is a mutation (vs read-only).
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            Self::CheckpointSave
                | Self::CheckpointDelete
                | Self::CheckpointRollback
                | Self::ContextRotate
                | Self::WorkClaim
                | Self::WorkRelease
                | Self::WorkComplete
                | Self::WorkAssign
                | Self::FleetScale
                | Self::FleetRebalance
                | Self::ProfileApply
        )
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn default_list_limit() -> usize {
    50
}

fn default_count_one() -> u32 {
    1
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Serde roundtrip: top-level command ──────────────────────────

    #[test]
    fn checkpoint_save_command_roundtrip() {
        let cmd = RobotNtmCommand::Checkpoint(CheckpointCommand::Save(CheckpointSaveRequest {
            label: Some("pre-refactor".into()),
            include_scrollback: true,
            pane_ids: vec![1, 2, 3],
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert_eq!(cmd.family_name(), "checkpoint");
        assert_eq!(cmd.action_name(), "save");
        assert!(cmd.is_mutation());
    }

    #[test]
    fn checkpoint_list_command_roundtrip() {
        let cmd = RobotNtmCommand::Checkpoint(CheckpointCommand::List(CheckpointListRequest {
            limit: 10,
            offset: 0,
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(!cmd.is_mutation());
    }

    #[test]
    fn checkpoint_rollback_dry_run_roundtrip() {
        let cmd =
            RobotNtmCommand::Checkpoint(CheckpointCommand::Rollback(CheckpointRollbackRequest {
                checkpoint_id: "ckpt-abc123".into(),
                dry_run: true,
            }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(cmd.is_mutation());
    }

    #[test]
    fn context_status_command_roundtrip() {
        let cmd = RobotNtmCommand::Context(ContextCommand::Status(ContextStatusRequest {
            pane_id: Some(5),
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert_eq!(cmd.family_name(), "context");
        assert!(!cmd.is_mutation());
    }

    #[test]
    fn context_rotate_command_roundtrip() {
        let cmd = RobotNtmCommand::Context(ContextCommand::Rotate(ContextRotateRequest {
            pane_id: 3,
            strategy: RotationStrategy::Aggressive,
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(cmd.is_mutation());
    }

    #[test]
    fn work_claim_command_roundtrip() {
        let cmd = RobotNtmCommand::Work(WorkCommand::Claim(WorkClaimRequest {
            item_id: "ft-abc".into(),
            agent_id: "PinkForge".into(),
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert_eq!(cmd.family_name(), "work");
        assert_eq!(cmd.action_name(), "claim");
        assert!(cmd.is_mutation());
    }

    #[test]
    fn work_list_command_roundtrip() {
        let cmd = RobotNtmCommand::Work(WorkCommand::List(WorkListRequest {
            status_filter: Some("ready".into()),
            agent_filter: None,
            label_filter: Some("backend".into()),
            limit: 20,
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(!cmd.is_mutation());
    }

    #[test]
    fn work_complete_command_roundtrip() {
        let cmd = RobotNtmCommand::Work(WorkCommand::Complete(WorkCompleteRequest {
            item_id: "ft-xyz".into(),
            summary: Some("Implemented in abc123".into()),
            evidence: vec!["abc123".into(), "artifacts/test-log.txt".into()],
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(cmd.is_mutation());
    }

    #[test]
    fn fleet_status_command_roundtrip() {
        let cmd =
            RobotNtmCommand::Fleet(FleetCommand::Status(FleetStatusRequest { detailed: true }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert_eq!(cmd.family_name(), "fleet");
        assert!(!cmd.is_mutation());
    }

    #[test]
    fn fleet_scale_command_roundtrip() {
        let cmd = RobotNtmCommand::Fleet(FleetCommand::Scale(FleetScaleRequest {
            program: "claude_code".into(),
            target_count: 4,
            dry_run: true,
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(cmd.is_mutation());
    }

    #[test]
    fn fleet_rebalance_command_roundtrip() {
        let cmd = RobotNtmCommand::Fleet(FleetCommand::Rebalance(FleetRebalanceRequest {
            strategy: RebalanceStrategy::CapabilityBased,
            dry_run: false,
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
    }

    #[test]
    fn profile_list_command_roundtrip() {
        let cmd = RobotNtmCommand::Profile(ProfileCommand::List(ProfileListRequest {
            role_filter: Some("agent_worker".into()),
            tag_filter: None,
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert_eq!(cmd.family_name(), "profile");
        assert!(!cmd.is_mutation());
    }

    #[test]
    fn profile_apply_command_roundtrip() {
        let mut overrides = HashMap::new();
        overrides.insert("FT_LOG".into(), "debug".into());
        let cmd = RobotNtmCommand::Profile(ProfileCommand::Apply(ProfileApplyRequest {
            name: "agent-worker".into(),
            count: 3,
            env_overrides: overrides,
            dry_run: true,
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(cmd.is_mutation());
    }

    #[test]
    fn profile_validate_command_roundtrip() {
        let cmd = RobotNtmCommand::Profile(ProfileCommand::Validate(ProfileValidateRequest {
            name: "dev-shell".into(),
        }));
        let json = serde_json::to_string(&cmd).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, rt);
        assert!(!cmd.is_mutation());
    }

    // ─── Response data serde roundtrips ──────────────────────────────

    #[test]
    fn checkpoint_save_data_roundtrip() {
        let data = CheckpointSaveData {
            checkpoint_id: "ckpt-001".into(),
            label: Some("pre-deploy".into()),
            pane_count: 5,
            bytes_persisted: 102400,
            scrollback_included: true,
            created_at: 1700000000000,
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: CheckpointSaveData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn checkpoint_list_data_roundtrip() {
        let data = CheckpointListData {
            checkpoints: vec![CheckpointSummary {
                checkpoint_id: "ckpt-001".into(),
                label: None,
                pane_count: 3,
                size_bytes: 8192,
                created_at: 1700000000000,
            }],
            total: 1,
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: CheckpointListData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn checkpoint_show_data_roundtrip() {
        let data = CheckpointShowData {
            checkpoint_id: "ckpt-001".into(),
            label: Some("snapshot".into()),
            panes: vec![CheckpointPaneSnapshot {
                pane_id: 1,
                title: "agent-1".into(),
                working_dir: Some("/home/user/project".into()),
                has_scrollback: true,
                scrollback_lines: 500,
            }],
            size_bytes: 16384,
            content_hash: "abc123def456".into(),
            created_at: 1700000000000,
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: CheckpointShowData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn checkpoint_rollback_data_roundtrip() {
        let data = CheckpointRollbackData {
            checkpoint_id: "ckpt-001".into(),
            panes_restored: 4,
            panes_skipped: 1,
            dry_run: false,
            warnings: vec!["pane 5 no longer exists".into()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: CheckpointRollbackData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn context_status_data_roundtrip() {
        let data = ContextStatusData {
            panes: vec![PaneContextStatus {
                pane_id: 1,
                pressure_tier: "green".into(),
                utilization: 0.35,
                tokens_consumed: 35000,
                token_budget: 100000,
                compaction_count: 0,
                ms_since_last_compaction: None,
            }],
            fleet_pressure: FleetContextPressure {
                total_panes: 4,
                green_count: 3,
                yellow_count: 1,
                red_count: 0,
                black_count: 0,
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: ContextStatusData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn context_rotate_data_roundtrip() {
        let data = ContextRotateData {
            pane_id: 3,
            accepted: true,
            reason: None,
            strategy: RotationStrategy::Gentle,
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: ContextRotateData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn context_history_data_roundtrip() {
        let data = ContextHistoryData {
            pane_id: 1,
            events: vec![CompactionEvent {
                timestamp_ms: 1700000000000,
                utilization_before: 0.92,
                utilization_after: 0.45,
                tokens_freed: 47000,
                trigger: "pressure_threshold".into(),
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: ContextHistoryData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn work_claim_data_roundtrip() {
        let data = WorkClaimData {
            item_id: "ft-abc".into(),
            agent_id: "PinkForge".into(),
            title: "Implement feature X".into(),
            priority: 1,
            claimed_at: 1700000000000,
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: WorkClaimData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn work_complete_data_roundtrip() {
        let data = WorkCompleteData {
            item_id: "ft-xyz".into(),
            unblocked: vec!["ft-abc".into(), "ft-def".into()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: WorkCompleteData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn work_list_data_roundtrip() {
        let data = WorkListData {
            items: vec![WorkItemSummary {
                id: "ft-001".into(),
                title: "Fix bug".into(),
                priority: 0,
                status: "ready".into(),
                assigned_to: None,
                labels: vec!["backend".into()],
                blocked_by_count: 0,
                unblocks_count: 2,
            }],
            total: 1,
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: WorkListData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn fleet_status_data_roundtrip() {
        let mut by_program = HashMap::new();
        by_program.insert(
            "claude_code".into(),
            ProgramSlotSummary {
                count: 4,
                active: 3,
                idle: 1,
            },
        );
        let data = FleetStatusData {
            total_agents: 4,
            active_agents: 3,
            idle_agents: 1,
            stalled_agents: 0,
            by_program,
            work_queue: WorkQueueSummary {
                total_items: 10,
                ready: 3,
                blocked: 5,
                in_progress: 2,
                completed: 0,
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: FleetStatusData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn fleet_scale_data_roundtrip() {
        let data = FleetScaleData {
            program: "codex".into(),
            previous_count: 2,
            new_count: 4,
            dry_run: false,
            spawned_pane_ids: vec![10, 11],
            terminated_pane_ids: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: FleetScaleData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn fleet_rebalance_data_roundtrip() {
        let data = FleetRebalanceData {
            strategy: RebalanceStrategy::LoadBased,
            items_reassigned: 2,
            dry_run: true,
            reassignments: vec![RebalanceAction {
                item_id: "ft-001".into(),
                from_agent: Some("agent-1".into()),
                to_agent: "agent-3".into(),
                reason: "agent-1 overloaded".into(),
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: FleetRebalanceData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn profile_list_data_roundtrip() {
        let data = ProfileListData {
            profiles: vec![ProfileSummary {
                name: "agent-worker".into(),
                description: Some("Standard AI agent pane".into()),
                role: "agent_worker".into(),
                tags: vec!["default".into()],
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: ProfileListData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn profile_show_data_roundtrip() {
        let mut env = HashMap::new();
        env.insert("FT_LOG".into(), "info".into());
        let data = ProfileShowData {
            name: "agent-worker".into(),
            description: Some("Standard AI agent".into()),
            role: "agent_worker".into(),
            spawn_command: Some("claude --dangerously-skip-permissions".into()),
            environment: env,
            working_directory: Some("/home/user/project".into()),
            layout_template: None,
            bootstrap_commands: vec!["git pull".into()],
            tags: vec!["default".into()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: ProfileShowData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn profile_apply_data_roundtrip() {
        let data = ProfileApplyData {
            profile_name: "agent-worker".into(),
            panes_spawned: vec![10, 11, 12],
            dry_run: false,
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: ProfileApplyData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    #[test]
    fn profile_validate_data_roundtrip() {
        let data = ProfileValidateData {
            name: "broken-profile".into(),
            valid: false,
            issues: vec!["spawn_command references nonexistent binary".into()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let rt: ProfileValidateData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, rt);
    }

    // ─── NTM equivalence mapping ─────────────────────────────────────

    #[test]
    fn checkpoint_save_ntm_equivalence() {
        let cmd = RobotNtmCommand::Checkpoint(CheckpointCommand::Save(CheckpointSaveRequest {
            label: None,
            include_scrollback: false,
            pane_ids: vec![],
        }));
        let eq = cmd.ntm_equivalence();
        assert_eq!(eq.census_domain, "F. Session Persistence & Recovery");
        assert_eq!(eq.classification, ConvergenceClassification::Upgrade);
        assert!(eq.ntm_commands.contains(&"ntm checkpoint save".to_string()));
    }

    #[test]
    fn work_claim_ntm_equivalence() {
        let cmd = RobotNtmCommand::Work(WorkCommand::Claim(WorkClaimRequest {
            item_id: "ft-1".into(),
            agent_id: "a".into(),
        }));
        let eq = cmd.ntm_equivalence();
        assert_eq!(eq.census_domain, "J. Workflow & Task Orchestration");
        assert_eq!(eq.classification, ConvergenceClassification::Upgrade);
    }

    #[test]
    fn fleet_rebalance_is_novel() {
        let cmd = RobotNtmCommand::Fleet(FleetCommand::Rebalance(FleetRebalanceRequest {
            strategy: RebalanceStrategy::default(),
            dry_run: false,
        }));
        let eq = cmd.ntm_equivalence();
        assert_eq!(eq.classification, ConvergenceClassification::Novel);
    }

    #[test]
    fn profile_validate_is_novel() {
        let cmd = RobotNtmCommand::Profile(ProfileCommand::Validate(ProfileValidateRequest {
            name: "test".into(),
        }));
        let eq = cmd.ntm_equivalence();
        assert_eq!(eq.classification, ConvergenceClassification::Novel);
        assert!(eq.ntm_commands.is_empty());
    }

    #[test]
    fn context_history_is_novel() {
        let cmd = RobotNtmCommand::Context(ContextCommand::History(ContextHistoryRequest {
            pane_id: 1,
            limit: 10,
        }));
        let eq = cmd.ntm_equivalence();
        assert_eq!(eq.classification, ConvergenceClassification::Novel);
    }

    // ─── Surface registry ────────────────────────────────────────────

    #[test]
    fn all_surfaces_have_unique_command_paths() {
        let paths: Vec<&str> = NtmApiSurface::ALL
            .iter()
            .map(|s| s.command_path())
            .collect();
        let unique: std::collections::HashSet<&str> = paths.iter().copied().collect();
        assert_eq!(
            paths.len(),
            unique.len(),
            "duplicate command paths detected"
        );
    }

    #[test]
    fn surface_count_matches_all_array() {
        assert_eq!(NtmApiSurface::ALL.len(), 22);
    }

    #[test]
    fn mutation_surfaces_are_correct() {
        let mutations: Vec<&str> = NtmApiSurface::ALL
            .iter()
            .filter(|s| s.is_mutation())
            .map(|s| s.command_path())
            .collect();
        // checkpoint save/delete/rollback + context rotate + work claim/release/complete/assign
        // + fleet scale/rebalance + profile apply = 11
        assert_eq!(
            mutations.len(),
            11,
            "mutation surface count mismatch: {mutations:?}"
        );
    }

    #[test]
    fn read_only_surfaces_are_correct() {
        let reads: Vec<&str> = NtmApiSurface::ALL
            .iter()
            .filter(|s| !s.is_mutation())
            .map(|s| s.command_path())
            .collect();
        // checkpoint list/show + context status/history + work list/ready
        // + fleet status/agents + profile list/show/validate = 11
        assert_eq!(
            reads.len(),
            11,
            "read-only surface count mismatch: {reads:?}"
        );
    }

    // ─── Convergence classification ──────────────────────────────────

    #[test]
    fn convergence_classification_serde_roundtrip() {
        for class in [
            ConvergenceClassification::DirectReplacement,
            ConvergenceClassification::Upgrade,
            ConvergenceClassification::Novel,
            ConvergenceClassification::Partial,
        ] {
            let json = serde_json::to_string(&class).unwrap();
            let rt: ConvergenceClassification = serde_json::from_str(&json).unwrap();
            assert_eq!(class, rt);
        }
    }

    #[test]
    fn convergence_classification_labels() {
        assert_eq!(
            ConvergenceClassification::DirectReplacement.label(),
            "direct-replacement"
        );
        assert_eq!(ConvergenceClassification::Upgrade.label(), "upgrade");
        assert_eq!(ConvergenceClassification::Novel.label(), "novel");
        assert_eq!(ConvergenceClassification::Partial.label(), "partial");
    }

    // ─── Default values ──────────────────────────────────────────────

    #[test]
    fn checkpoint_list_default_limit() {
        let json = r#"{"action":"list","limit":50,"offset":0}"#;
        let cmd: CheckpointCommand = serde_json::from_str(json).unwrap();
        if let CheckpointCommand::List(req) = cmd {
            assert_eq!(req.limit, 50);
        } else {
            panic!("expected List");
        }
    }

    #[test]
    fn rotation_strategy_default_is_agent_default() {
        assert_eq!(RotationStrategy::default(), RotationStrategy::AgentDefault);
    }

    #[test]
    fn rebalance_strategy_default_is_load_based() {
        assert_eq!(RebalanceStrategy::default(), RebalanceStrategy::LoadBased);
    }

    #[test]
    fn profile_apply_default_count_is_one() {
        let json =
            r#"{"action":"apply","name":"test","count":1,"env_overrides":{},"dry_run":false}"#;
        let cmd: ProfileCommand = serde_json::from_str(json).unwrap();
        if let ProfileCommand::Apply(req) = cmd {
            assert_eq!(req.count, 1);
        } else {
            panic!("expected Apply");
        }
    }
}
