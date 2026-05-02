//! Pane groups + background-job queue contract layer
//! ([BR-TERM-EMULATOR-UPLIFT-2.8] / `ft-2okh0.8`).
//!
//! Two cooperating substrates the bead asks for:
//!
//! 1. **Pane groups** (zellij-style) — client-aware pane
//!    grouping with toggle/add semantics. Operators issue
//!    one op (close-all / send-to-all / kill-all) that fans
//!    out across every pane in the group.
//! 2. **Background-job queue** — `asupersync` task pool with
//!    priority levels for long-running work (DisplayPaneError,
//!    AnimatePluginLoading, WebRequest, RunCommand,
//!    ReportSessionInfo) so the render thread never blocks.
//!
//! Both ship as **pure-logic state machines** in this module;
//! the actual asupersync task spawning + UI integration is
//! the integration follow-on.
//!
//! ## What this module ships
//!
//! - [`GroupId`] / [`PaneId`] — typed handles.
//! - [`PaneGroupRegistry`] — the registry of groups and
//!   their pane membership. Pure state machine — no async,
//!   no I/O.
//! - [`GroupOp`] — closed list of operations: `Create`,
//!   `Add`, `Remove`, `Toggle`, `CloseAll`, `KillAll`,
//!   `SendTextToAll`, `FocusCycle`, `Rename`, `Destroy`.
//! - [`apply_group_op`] — bit-for-bit-faithful state-machine
//!   reducer.
//! - [`BackgroundJob`] / [`JobPriority`] — closed list of
//!   priority levels matching the bead's table.
//! - [`BackgroundJobQueue`] — priority-ordered queue with
//!   `enqueue`, `take_next`, `cancel`, `len_at_priority`.
//! - [`PaneGroupHealth`] / [`BackgroundJobHealth`] — `ft
//!   doctor` snapshots mirroring this session's `*Health`
//!   shape.
//! - JSONL row contract for structured logging at
//!   `tests/pane_groups/logs/<scenario>.jsonl` (the bead's
//!   "Structured logging" requirement).
//!
//! ## What this module is NOT
//!
//! - Not the asupersync task pool. The bead's action 8.2
//!   uses `runtime_async::spawn`; this module ships the job
//!   record + queue contract the executor consumes.
//! - Not the robot-mode CLI handler. Bead action 8.3 lives
//!   in `RobotCommands::Panes` dispatch; this module ships
//!   the operation taxonomy the handler dispatches into.
//! - Not the plugin-hook seam. Bead action 8.4 cross-links
//!   the WASM plugin substrate; this module ships the
//!   `GroupChangeEvent` shape the seam emits.
//! - Not session-restore wiring. Bead action 8.5 — the
//!   `PaneGroupRegistry` is `serde`-roundtrip clean; the
//!   restore path consumes the JSON.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use serde::{Deserialize, Serialize};

// ============================================================================
// Typed handles
// ============================================================================

/// Group id — bounded `u8` for state-space tractability in
/// tests; production wraps a `String` via the registry's
/// name → id index.
pub type GroupId = u8;

/// Pane id — bounded `u8` for tests.
pub type PaneId = u8;

// ============================================================================
// Pane group registry
// ============================================================================

/// One pane group: a name + a set of pane ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneGroup {
    pub id: GroupId,
    pub name: String,
    pub members: BTreeSet<PaneId>,
}

/// Registry tracking every active group + a name→id index
/// for fast lookup. Pure state machine — no async, no I/O.
///
/// **Integrity invariant**: fields are `pub(crate)` so all
/// mutation must go through [`apply_group_op`]. Downstream
/// crates use the read-only accessor methods
/// ([`Self::groups`], [`Self::ops_applied_total`], etc.) —
/// they cannot desync `name_index` from `groups`, zero
/// the telemetry counter, or clear the audit-trail
/// `events` vec. Same-crate access (this module's
/// `apply_group_op` + tests) bypasses the visibility
/// barrier per Rust's standard `pub(crate)` semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneGroupRegistry {
    pub(crate) groups: BTreeMap<GroupId, PaneGroup>,
    /// Maps lowercase group name → group id. Operators
    /// reference groups by name in the CLI; the registry
    /// resolves to id internally.
    pub(crate) name_index: BTreeMap<String, GroupId>,
    /// Total ops applied — counter for telemetry.
    pub(crate) ops_applied_total: u64,
    /// Trace of emitted change events for the plugin-hook
    /// seam (bead action 8.4). Bounded by
    /// `MAX_RETAINED_EVENTS` to prevent unbounded growth.
    pub(crate) events: Vec<GroupChangeEvent>,
}

/// Cap on retained events in the registry. The integration
/// drains older events into the asupersync log path; this
/// cap is a safety floor for tests.
pub const MAX_RETAINED_EVENTS: usize = 1024;

impl PaneGroupRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only access to the groups map for downstream
    /// callers (the GUI consumes this for rendering group
    /// indicators).
    #[must_use]
    pub fn groups(&self) -> &BTreeMap<GroupId, PaneGroup> {
        &self.groups
    }

    /// Read-only ops-applied counter for the doctor.
    #[must_use]
    pub fn ops_applied_total(&self) -> u64 {
        self.ops_applied_total
    }

    /// Read-only events log for plugin-hook drainers.
    #[must_use]
    pub fn events(&self) -> &[GroupChangeEvent] {
        &self.events
    }

    /// Look up a group id by name (case-insensitive).
    #[must_use]
    pub fn id_of(&self, name: &str) -> Option<GroupId> {
        self.name_index.get(&name.to_ascii_lowercase()).copied()
    }

    /// All pane ids across all groups (deduped). The robot-
    /// mode `panes group-op kill-all` consumes this for an
    /// "every pane in a group" operation.
    #[must_use]
    pub fn all_grouped_panes(&self) -> BTreeSet<PaneId> {
        let mut out = BTreeSet::new();
        for group in self.groups.values() {
            out.extend(group.members.iter().copied());
        }
        out
    }
}

// ============================================================================
// Group ops
// ============================================================================

/// Closed list of operations on the registry. Adding an op
/// extends this enum; the harness's
/// `every_op_kind_has_a_classification` test pins coverage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GroupOp {
    /// Create a fresh group with a name. Fails if name
    /// already taken.
    Create { id: GroupId, name: String },
    /// Add a pane to a group.
    Add { group: GroupId, pane: PaneId },
    /// Remove a pane.
    Remove { group: GroupId, pane: PaneId },
    /// Toggle membership — add if absent, remove if
    /// present. Mirrors zellij's toggle semantics.
    Toggle { group: GroupId, pane: PaneId },
    /// Close every pane in the group (panes still exist,
    /// just deregistered from rendering).
    CloseAll { group: GroupId },
    /// Forcefully kill every pane in the group + remove the
    /// group itself.
    KillAll { group: GroupId },
    /// Send a text payload to every pane in the group.
    /// Telemetry records `panes_targeted` from the result.
    SendTextToAll { group: GroupId, bytes_len: u32 },
    /// Cycle keyboard focus through the group's panes
    /// (next-pane wraps).
    FocusCycle { group: GroupId },
    /// Rename a group.
    Rename { group: GroupId, new_name: String },
    /// Destroy the group (drops membership; panes survive).
    Destroy { group: GroupId },
}

/// Outcome of applying one op.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GroupOpOutcome {
    /// Op succeeded; ops_applied_total bumped.
    Applied { panes_affected: u32 },
    /// Op was a no-op — same effective state.
    NoOp,
    /// Op was rejected. The reason carries the failure
    /// class so the operator + telemetry can audit.
    Denied { reason: GroupOpDenialReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupOpDenialReason {
    NameAlreadyTaken,
    UnknownGroup,
    DuplicateGroupId,
    EmptyName,
}

/// Emitted when registry state changes — plugin hooks (bead
/// action 8.4) subscribe to these.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GroupChangeEvent {
    GroupCreated {
        group: GroupId,
        name: String,
    },
    PaneAdded {
        group: GroupId,
        pane: PaneId,
    },
    PaneRemoved {
        group: GroupId,
        pane: PaneId,
    },
    GroupBulkOp {
        group: GroupId,
        op_slug: String,
        panes_affected: u32,
    },
    GroupRenamed {
        group: GroupId,
        new_name: String,
    },
    GroupDestroyed {
        group: GroupId,
    },
}

/// Apply a single op against the registry. Pure state
/// machine — same inputs always produce the same outcome
/// + same registry mutation.
pub fn apply_group_op(registry: &mut PaneGroupRegistry, op: &GroupOp) -> GroupOpOutcome {
    match op {
        GroupOp::Create { id, name } => {
            if name.is_empty() {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::EmptyName,
                };
            }
            let normalized = name.to_ascii_lowercase();
            if registry.name_index.contains_key(&normalized) {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::NameAlreadyTaken,
                };
            }
            if registry.groups.contains_key(id) {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::DuplicateGroupId,
                };
            }
            registry.groups.insert(
                *id,
                PaneGroup {
                    id: *id,
                    name: name.clone(),
                    members: BTreeSet::new(),
                },
            );
            registry.name_index.insert(normalized, *id);
            push_event(
                registry,
                GroupChangeEvent::GroupCreated {
                    group: *id,
                    name: name.clone(),
                },
            );
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            GroupOpOutcome::Applied { panes_affected: 0 }
        }
        GroupOp::Add { group, pane } => {
            let Some(g) = registry.groups.get_mut(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            if g.members.insert(*pane) {
                push_event(
                    registry,
                    GroupChangeEvent::PaneAdded {
                        group: *group,
                        pane: *pane,
                    },
                );
                registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
                GroupOpOutcome::Applied { panes_affected: 1 }
            } else {
                GroupOpOutcome::NoOp
            }
        }
        GroupOp::Remove { group, pane } => {
            let Some(g) = registry.groups.get_mut(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            if g.members.remove(pane) {
                push_event(
                    registry,
                    GroupChangeEvent::PaneRemoved {
                        group: *group,
                        pane: *pane,
                    },
                );
                registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
                GroupOpOutcome::Applied { panes_affected: 1 }
            } else {
                GroupOpOutcome::NoOp
            }
        }
        GroupOp::Toggle { group, pane } => {
            let Some(g) = registry.groups.get_mut(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            if g.members.remove(pane) {
                push_event(
                    registry,
                    GroupChangeEvent::PaneRemoved {
                        group: *group,
                        pane: *pane,
                    },
                );
            } else {
                g.members.insert(*pane);
                push_event(
                    registry,
                    GroupChangeEvent::PaneAdded {
                        group: *group,
                        pane: *pane,
                    },
                );
            }
            GroupOpOutcome::Applied { panes_affected: 1 }
        }
        GroupOp::CloseAll { group } => {
            let Some(g) = registry.groups.get(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            let count = g.members.len() as u32;
            push_event(
                registry,
                GroupChangeEvent::GroupBulkOp {
                    group: *group,
                    op_slug: "close_all".to_string(),
                    panes_affected: count,
                },
            );
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            GroupOpOutcome::Applied {
                panes_affected: count,
            }
        }
        GroupOp::KillAll { group } => {
            let Some(g) = registry.groups.remove(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            let count = g.members.len() as u32;
            registry.name_index.remove(&g.name.to_ascii_lowercase());
            push_event(
                registry,
                GroupChangeEvent::GroupBulkOp {
                    group: *group,
                    op_slug: "kill_all".to_string(),
                    panes_affected: count,
                },
            );
            push_event(registry, GroupChangeEvent::GroupDestroyed { group: *group });
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            GroupOpOutcome::Applied {
                panes_affected: count,
            }
        }
        GroupOp::SendTextToAll { group, .. } => {
            let Some(g) = registry.groups.get(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            let count = g.members.len() as u32;
            push_event(
                registry,
                GroupChangeEvent::GroupBulkOp {
                    group: *group,
                    op_slug: "send_text_to_all".to_string(),
                    panes_affected: count,
                },
            );
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            GroupOpOutcome::Applied {
                panes_affected: count,
            }
        }
        GroupOp::FocusCycle { group } => {
            let Some(g) = registry.groups.get(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            let count = g.members.len() as u32;
            // Focus-cycle is a non-mutating op for the
            // registry (focus state lives in the GUI); we
            // still emit a bulk-op event for telemetry.
            push_event(
                registry,
                GroupChangeEvent::GroupBulkOp {
                    group: *group,
                    op_slug: "focus_cycle".to_string(),
                    panes_affected: count,
                },
            );
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            GroupOpOutcome::Applied {
                panes_affected: count,
            }
        }
        GroupOp::Rename { group, new_name } => {
            if new_name.is_empty() {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::EmptyName,
                };
            }
            let new_normalized = new_name.to_ascii_lowercase();
            // Allow self-rename (case-only change). The
            // index check rejects only when the name is
            // taken by a *different* group.
            if let Some(holder) = registry.name_index.get(&new_normalized) {
                if holder != group {
                    return GroupOpOutcome::Denied {
                        reason: GroupOpDenialReason::NameAlreadyTaken,
                    };
                }
            }
            let Some(g) = registry.groups.get_mut(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            let old_normalized = g.name.to_ascii_lowercase();
            g.name = new_name.clone();
            registry.name_index.remove(&old_normalized);
            registry.name_index.insert(new_normalized, *group);
            push_event(
                registry,
                GroupChangeEvent::GroupRenamed {
                    group: *group,
                    new_name: new_name.clone(),
                },
            );
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            GroupOpOutcome::Applied { panes_affected: 0 }
        }
        GroupOp::Destroy { group } => {
            let Some(g) = registry.groups.remove(group) else {
                return GroupOpOutcome::Denied {
                    reason: GroupOpDenialReason::UnknownGroup,
                };
            };
            // Report orphaned-pane count so the
            // integration's UI can re-render affected
            // panes without their group decoration. Match
            // KillAll's reporting shape.
            let orphaned_count = g.members.len() as u32;
            registry.name_index.remove(&g.name.to_ascii_lowercase());
            push_event(registry, GroupChangeEvent::GroupDestroyed { group: *group });
            registry.ops_applied_total = registry.ops_applied_total.saturating_add(1);
            GroupOpOutcome::Applied {
                panes_affected: orphaned_count,
            }
        }
    }
}

fn push_event(registry: &mut PaneGroupRegistry, event: GroupChangeEvent) {
    if registry.events.len() >= MAX_RETAINED_EVENTS {
        registry.events.remove(0);
    }
    registry.events.push(event);
}

// ============================================================================
// Background-job queue
// ============================================================================

/// Priority levels per the bead's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPriority {
    /// TX engine prepare/commit/compensate (cross-link
    /// `tx_execution.rs`).
    Critical,
    /// User-initiated (workflow run, send to all).
    High,
    /// Background polling, plugin tick.
    Normal,
    /// Periodic cleanup, telemetry export.
    Low,
}

impl JobPriority {
    /// Sort key — Critical = 3, Low = 0. Higher value
    /// dequeues first.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Critical => 3,
            Self::High => 2,
            Self::Normal => 1,
            Self::Low => 0,
        }
    }
}

impl PartialOrd for JobPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JobPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// One queued job. Carries a `kind` slug naming the work
/// kind (DisplayPaneError, AnimatePluginLoading, WebRequest,
/// RunCommand, ReportSessionInfo, etc.) and the priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundJob {
    pub job_id: u64,
    pub kind: String,
    pub priority: JobPriority,
    pub scheduled_at_ms: u64,
}

/// Internal queue entry — wraps `BackgroundJob` with its
/// insertion order so equal-priority jobs dequeue FIFO.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedJob {
    job: BackgroundJob,
    insertion_order: u64,
}

impl PartialOrd for QueuedJob {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedJob {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher priority first; same priority → lower
        // insertion_order first (FIFO). BinaryHeap is a
        // max-heap, so we order accordingly.
        self.job
            .priority
            .cmp(&other.job.priority)
            .then_with(|| other.insertion_order.cmp(&self.insertion_order))
    }
}

/// Priority-ordered job queue. Uses a `BinaryHeap` keyed on
/// (priority, -insertion_order) so dequeue is O(log n) and
/// stable per priority.
///
/// **Integrity invariant**: counters are `pub(crate)` so
/// downstream callers cannot zero them silently. Use
/// [`Self::enqueued_total`], [`Self::dequeued_total`],
/// [`Self::cancelled_total`] read-only accessors to inspect
/// from outside the crate.
#[derive(Debug, Default)]
pub struct BackgroundJobQueue {
    heap: BinaryHeap<QueuedJob>,
    next_insertion: u64,
    /// Counters per priority for the doctor surface.
    pub(crate) enqueued_total: BTreeMap<String, u64>,
    pub(crate) dequeued_total: BTreeMap<String, u64>,
    pub(crate) cancelled_total: BTreeMap<String, u64>,
}

impl BackgroundJobQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a job. Returns the job_id.
    pub fn enqueue(&mut self, job: BackgroundJob) -> u64 {
        let slug = priority_slug(job.priority).to_string();
        *self.enqueued_total.entry(slug).or_insert(0) += 1;
        let job_id = job.job_id;
        let entry = QueuedJob {
            job,
            insertion_order: self.next_insertion,
        };
        self.next_insertion = self.next_insertion.saturating_add(1);
        self.heap.push(entry);
        job_id
    }

    /// Dequeue the highest-priority job. FIFO within the
    /// same priority level.
    pub fn take_next(&mut self) -> Option<BackgroundJob> {
        let next = self.heap.pop()?;
        let slug = priority_slug(next.job.priority).to_string();
        *self.dequeued_total.entry(slug).or_insert(0) += 1;
        Some(next.job)
    }

    /// Cancel a job by id. Returns true iff the queue
    /// contained it.
    pub fn cancel(&mut self, job_id: u64) -> bool {
        let mut remaining: Vec<QueuedJob> = Vec::with_capacity(self.heap.len());
        let mut found = false;
        while let Some(entry) = self.heap.pop() {
            if !found && entry.job.job_id == job_id {
                let slug = priority_slug(entry.job.priority).to_string();
                *self.cancelled_total.entry(slug).or_insert(0) += 1;
                found = true;
                continue;
            }
            remaining.push(entry);
        }
        for entry in remaining {
            self.heap.push(entry);
        }
        found
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Count of jobs at the given priority level.
    #[must_use]
    pub fn len_at_priority(&self, priority: JobPriority) -> usize {
        self.heap
            .iter()
            .filter(|q| q.job.priority == priority)
            .count()
    }

    /// Read-only accessor for the per-priority enqueued
    /// counter map.
    #[must_use]
    pub fn enqueued_total(&self) -> &BTreeMap<String, u64> {
        &self.enqueued_total
    }

    /// Read-only accessor for the per-priority dequeued
    /// counter map.
    #[must_use]
    pub fn dequeued_total(&self) -> &BTreeMap<String, u64> {
        &self.dequeued_total
    }

    /// Read-only accessor for the per-priority cancelled
    /// counter map.
    #[must_use]
    pub fn cancelled_total(&self) -> &BTreeMap<String, u64> {
        &self.cancelled_total
    }
}

const fn priority_slug(p: JobPriority) -> &'static str {
    match p {
        JobPriority::Critical => "critical",
        JobPriority::High => "high",
        JobPriority::Normal => "normal",
        JobPriority::Low => "low",
    }
}

impl Clone for BackgroundJobQueue {
    fn clone(&self) -> Self {
        // BinaryHeap doesn't Clone unless T: Clone (it does
        // here), so this is straightforward.
        Self {
            heap: self.heap.clone(),
            next_insertion: self.next_insertion,
            enqueued_total: self.enqueued_total.clone(),
            dequeued_total: self.dequeued_total.clone(),
            cancelled_total: self.cancelled_total.clone(),
        }
    }
}

// ============================================================================
// JSONL row contract — bead's structured logging requirement
// ============================================================================

/// One JSONL row in `tests/pane_groups/logs/<scenario>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructuredLogRow {
    /// Group op record per the bead's "ts, group_name, op,
    /// pane_count, success" requirement.
    GroupOp {
        ts_ms: u64,
        group_name: String,
        op_slug: String,
        pane_count: u32,
        success: bool,
    },
    /// Background-job lifecycle record per the bead's "ts,
    /// job_kind, priority, scheduled_ts, started_ts,
    /// completed_ts, status" requirement.
    BackgroundJob {
        ts_ms: u64,
        job_kind: String,
        priority: String,
        scheduled_at_ms: u64,
        started_at_ms: Option<u64>,
        completed_at_ms: Option<u64>,
        status: String,
    },
}

#[must_use]
pub fn render_log_jsonl(rows: &[StructuredLogRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let line = serde_json::to_string(r).expect("StructuredLogRow always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_log_jsonl(jsonl: &str) -> Result<Vec<StructuredLogRow>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Health snapshots
// ============================================================================

/// `ft doctor` snapshot for the pane-group registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneGroupHealth {
    pub groups_total: u32,
    pub grouped_panes_total: u32,
    pub ops_applied_total: u64,
    pub ops_denied_total: u64,
    /// Per-denial-reason counter map.
    pub denial_reasons: BTreeMap<String, u64>,
}

impl PaneGroupHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            groups_total: 0,
            grouped_panes_total: 0,
            ops_applied_total: 0,
            ops_denied_total: 0,
            denial_reasons: BTreeMap::new(),
        }
    }

    /// Project a snapshot from a registry + outcome history.
    /// `denials` is a per-reason count from the operator's
    /// observed outcomes.
    #[must_use]
    pub fn from_registry(registry: &PaneGroupRegistry, denials: &BTreeMap<String, u64>) -> Self {
        Self {
            groups_total: registry.groups.len() as u32,
            grouped_panes_total: registry.all_grouped_panes().len() as u32,
            ops_applied_total: registry.ops_applied_total,
            ops_denied_total: denials.values().sum(),
            denial_reasons: denials.clone(),
        }
    }

    /// True iff no denial counters are non-zero. The doctor
    /// surfaces a WARN when any denial counter is hot.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.ops_denied_total == 0
    }
}

/// `ft doctor` snapshot for the background-job queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundJobHealth {
    pub queue_depth: u32,
    pub queue_depth_per_priority: BTreeMap<String, u32>,
    pub enqueued_total: BTreeMap<String, u64>,
    pub dequeued_total: BTreeMap<String, u64>,
    pub cancelled_total: BTreeMap<String, u64>,
}

impl BackgroundJobHealth {
    #[must_use]
    pub fn from_queue(queue: &BackgroundJobQueue) -> Self {
        let mut per_pri: BTreeMap<String, u32> = BTreeMap::new();
        for p in [
            JobPriority::Critical,
            JobPriority::High,
            JobPriority::Normal,
            JobPriority::Low,
        ] {
            per_pri.insert(
                priority_slug(p).to_string(),
                queue.len_at_priority(p) as u32,
            );
        }
        Self {
            queue_depth: queue.len() as u32,
            queue_depth_per_priority: per_pri,
            enqueued_total: queue.enqueued_total.clone(),
            dequeued_total: queue.dequeued_total.clone(),
            cancelled_total: queue.cancelled_total.clone(),
        }
    }

    /// Total Critical-priority jobs queued — the doctor
    /// fires a WARN when this is non-zero (Critical jobs
    /// shouldn't backlog).
    #[must_use]
    pub fn critical_queued(&self) -> u32 {
        self.queue_depth_per_priority
            .get("critical")
            .copied()
            .unwrap_or(0)
    }

    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.critical_queued() == 0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> PaneGroupRegistry {
        let mut r = PaneGroupRegistry::new();
        let _ = apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 1,
                name: "cc-agents".to_string(),
            },
        );
        r
    }

    // ------------------------------------------------------------------------
    // Group ops
    // ------------------------------------------------------------------------

    #[test]
    fn create_group_inserts_into_index() {
        let r = fresh();
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.id_of("cc-agents"), Some(1));
        assert_eq!(r.id_of("CC-AGENTS"), Some(1)); // case-insensitive
    }

    #[test]
    fn create_group_with_duplicate_name_denied() {
        let mut r = fresh();
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 2,
                name: "cc-agents".to_string(),
            },
        );
        assert_eq!(
            outcome,
            GroupOpOutcome::Denied {
                reason: GroupOpDenialReason::NameAlreadyTaken
            }
        );
    }

    #[test]
    fn create_group_with_duplicate_id_denied() {
        let mut r = fresh();
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 1,
                name: "different".to_string(),
            },
        );
        assert_eq!(
            outcome,
            GroupOpOutcome::Denied {
                reason: GroupOpDenialReason::DuplicateGroupId
            }
        );
    }

    #[test]
    fn create_group_with_empty_name_denied() {
        let mut r = PaneGroupRegistry::new();
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 1,
                name: "".to_string(),
            },
        );
        assert_eq!(
            outcome,
            GroupOpOutcome::Denied {
                reason: GroupOpDenialReason::EmptyName
            }
        );
    }

    #[test]
    fn add_pane_to_group() {
        let mut r = fresh();
        let outcome = apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 1 });
        assert!(r.groups.get(&1).unwrap().members.contains(&7));
    }

    #[test]
    fn add_duplicate_pane_is_noop() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        let outcome = apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        assert_eq!(outcome, GroupOpOutcome::NoOp);
    }

    #[test]
    fn add_to_unknown_group_denied() {
        let mut r = fresh();
        let outcome = apply_group_op(&mut r, &GroupOp::Add { group: 99, pane: 7 });
        assert!(matches!(
            outcome,
            GroupOpOutcome::Denied {
                reason: GroupOpDenialReason::UnknownGroup
            }
        ));
    }

    #[test]
    fn remove_pane_from_group() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        let outcome = apply_group_op(&mut r, &GroupOp::Remove { group: 1, pane: 7 });
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 1 });
        assert!(r.groups.get(&1).unwrap().members.is_empty());
    }

    #[test]
    fn remove_absent_pane_is_noop() {
        let mut r = fresh();
        let outcome = apply_group_op(&mut r, &GroupOp::Remove { group: 1, pane: 7 });
        assert_eq!(outcome, GroupOpOutcome::NoOp);
    }

    #[test]
    fn toggle_alternates_membership() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Toggle { group: 1, pane: 7 });
        assert!(r.groups.get(&1).unwrap().members.contains(&7));
        apply_group_op(&mut r, &GroupOp::Toggle { group: 1, pane: 7 });
        assert!(r.groups.get(&1).unwrap().members.is_empty());
        apply_group_op(&mut r, &GroupOp::Toggle { group: 1, pane: 7 });
        assert!(r.groups.get(&1).unwrap().members.contains(&7));
    }

    #[test]
    fn close_all_reports_pane_count() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 8 });
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 9 });
        let outcome = apply_group_op(&mut r, &GroupOp::CloseAll { group: 1 });
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 3 });
        // CloseAll doesn't remove the group (panes survive,
        // just deregistered). Verify membership preserved
        // for sender's reference.
        assert_eq!(r.groups.get(&1).unwrap().members.len(), 3);
    }

    #[test]
    fn kill_all_destroys_group() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 8 });
        let outcome = apply_group_op(&mut r, &GroupOp::KillAll { group: 1 });
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 2 });
        assert!(r.groups.get(&1).is_none());
        assert_eq!(r.id_of("cc-agents"), None);
    }

    #[test]
    fn rename_updates_index() {
        let mut r = fresh();
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::Rename {
                group: 1,
                new_name: "ai-agents".to_string(),
            },
        );
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 0 });
        assert_eq!(r.id_of("ai-agents"), Some(1));
        assert_eq!(r.id_of("cc-agents"), None);
    }

    #[test]
    fn rename_to_taken_name_denied() {
        let mut r = fresh();
        apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 2,
                name: "cod-agents".to_string(),
            },
        );
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::Rename {
                group: 2,
                new_name: "cc-agents".to_string(), // taken
            },
        );
        assert_eq!(
            outcome,
            GroupOpOutcome::Denied {
                reason: GroupOpDenialReason::NameAlreadyTaken
            }
        );
    }

    #[test]
    fn destroy_group() {
        let mut r = fresh();
        let outcome = apply_group_op(&mut r, &GroupOp::Destroy { group: 1 });
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 0 });
        assert!(r.groups.is_empty());
    }

    #[test]
    fn destroy_group_with_members_reports_orphan_count() {
        // Regression: previously, Destroy reported
        // panes_affected: 0 even when the group had
        // members — those panes were silently orphaned
        // (no longer in any group). Now matches KillAll's
        // shape: report g.members.len() so the
        // integration's UI can re-render affected panes
        // without their group decoration.
        let mut r = fresh();
        for pane in 0..5 {
            apply_group_op(&mut r, &GroupOp::Add { group: 1, pane });
        }
        let outcome = apply_group_op(&mut r, &GroupOp::Destroy { group: 1 });
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 5 });
        assert!(r.groups.is_empty());
    }

    #[test]
    fn rename_to_own_name_case_change_succeeds() {
        // Regression: previously, renaming a group to its
        // own current name (case-only change) returned
        // Denied{NameAlreadyTaken} because the index
        // contains_key check didn't distinguish
        // self-rename from another-group-takes-name. The
        // operator's intent is "set my display name to
        // X" — case-only changes should succeed.
        let mut r = fresh(); // group 1 named "cc-agents"
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::Rename {
                group: 1,
                new_name: "CC-AGENTS".to_string(),
            },
        );
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 0 });
        // Display name updated to the new case.
        assert_eq!(r.groups.get(&1).unwrap().name, "CC-AGENTS");
        // Lookup still works (case-insensitive).
        assert_eq!(r.id_of("cc-agents"), Some(1));
        assert_eq!(r.id_of("CC-AGENTS"), Some(1));
    }

    #[test]
    fn rename_to_identical_name_succeeds_as_noop_shape() {
        // Renaming to the exact same name (no case change)
        // is a no-op in effect but still succeeds. The
        // event log gets a redundant GroupRenamed entry —
        // pin this behavior so a future "optimize away
        // no-op renames" change is intentional.
        let mut r = fresh();
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::Rename {
                group: 1,
                new_name: "cc-agents".to_string(),
            },
        );
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 0 });
        assert_eq!(r.groups.get(&1).unwrap().name, "cc-agents");
    }

    #[test]
    fn send_text_to_all_targets_count() {
        let mut r = fresh();
        for pane in 0..5 {
            apply_group_op(&mut r, &GroupOp::Add { group: 1, pane });
        }
        let outcome = apply_group_op(
            &mut r,
            &GroupOp::SendTextToAll {
                group: 1,
                bytes_len: 100,
            },
        );
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 5 });
    }

    #[test]
    fn focus_cycle_is_non_mutating_but_recorded() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        let before_members = r.groups.get(&1).unwrap().members.clone();
        apply_group_op(&mut r, &GroupOp::FocusCycle { group: 1 });
        let after_members = &r.groups.get(&1).unwrap().members;
        assert_eq!(*after_members, before_members);
    }

    #[test]
    fn all_grouped_panes_dedups_across_groups() {
        let mut r = PaneGroupRegistry::new();
        apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 1,
                name: "g1".to_string(),
            },
        );
        apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 2,
                name: "g2".to_string(),
            },
        );
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 5 });
        apply_group_op(&mut r, &GroupOp::Add { group: 2, pane: 5 }); // shared
        apply_group_op(&mut r, &GroupOp::Add { group: 2, pane: 6 });
        let all = r.all_grouped_panes();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&5));
        assert!(all.contains(&6));
    }

    #[test]
    fn registry_serde_roundtrip() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 8 });
        let json = serde_json::to_string(&r).unwrap();
        let parsed: PaneGroupRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    // ------------------------------------------------------------------------
    // Background-job queue
    // ------------------------------------------------------------------------

    fn job(id: u64, kind: &str, priority: JobPriority) -> BackgroundJob {
        BackgroundJob {
            job_id: id,
            kind: kind.to_string(),
            priority,
            scheduled_at_ms: 0,
        }
    }

    #[test]
    fn priority_rank_ordering() {
        assert!(JobPriority::Critical > JobPriority::High);
        assert!(JobPriority::High > JobPriority::Normal);
        assert!(JobPriority::Normal > JobPriority::Low);
    }

    #[test]
    fn enqueue_then_dequeue_returns_critical_first() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "low_work", JobPriority::Low));
        q.enqueue(job(2, "normal_work", JobPriority::Normal));
        q.enqueue(job(3, "critical_work", JobPriority::Critical));
        q.enqueue(job(4, "high_work", JobPriority::High));

        let order: Vec<_> = std::iter::from_fn(|| q.take_next())
            .map(|j| j.job_id)
            .collect();
        assert_eq!(order, vec![3, 4, 2, 1]);
    }

    #[test]
    fn fifo_within_same_priority() {
        let mut q = BackgroundJobQueue::new();
        for id in 1..=5 {
            q.enqueue(job(id, "normal", JobPriority::Normal));
        }
        let order: Vec<_> = std::iter::from_fn(|| q.take_next())
            .map(|j| j.job_id)
            .collect();
        assert_eq!(order, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn cancel_removes_specific_job() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "low", JobPriority::Low));
        q.enqueue(job(2, "normal", JobPriority::Normal));
        q.enqueue(job(3, "critical", JobPriority::Critical));

        assert!(q.cancel(2));
        let order: Vec<_> = std::iter::from_fn(|| q.take_next())
            .map(|j| j.job_id)
            .collect();
        assert_eq!(order, vec![3, 1]);
    }

    #[test]
    fn cancel_unknown_returns_false() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "low", JobPriority::Low));
        assert!(!q.cancel(99));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn len_at_priority_counts_correctly() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "n1", JobPriority::Normal));
        q.enqueue(job(2, "n2", JobPriority::Normal));
        q.enqueue(job(3, "h1", JobPriority::High));
        assert_eq!(q.len_at_priority(JobPriority::Normal), 2);
        assert_eq!(q.len_at_priority(JobPriority::High), 1);
        assert_eq!(q.len_at_priority(JobPriority::Low), 0);
    }

    #[test]
    fn take_next_increments_dequeued_counter() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "n", JobPriority::Normal));
        q.take_next();
        assert_eq!(q.dequeued_total.get("normal"), Some(&1));
    }

    #[test]
    fn empty_queue_take_returns_none() {
        let mut q = BackgroundJobQueue::new();
        assert!(q.take_next().is_none());
    }

    // ------------------------------------------------------------------------
    // Health snapshots
    // ------------------------------------------------------------------------

    #[test]
    fn pane_group_health_baseline_safe() {
        let h = PaneGroupHealth::baseline();
        assert!(h.is_safe());
    }

    #[test]
    fn pane_group_health_unsafe_when_denials_present() {
        let mut h = PaneGroupHealth::baseline();
        h.ops_denied_total = 3;
        assert!(!h.is_safe());
    }

    #[test]
    fn pane_group_health_from_registry_counts() {
        let mut r = fresh();
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 7 });
        apply_group_op(&mut r, &GroupOp::Add { group: 1, pane: 8 });
        let mut denials = BTreeMap::new();
        denials.insert("name_already_taken".to_string(), 1);
        let h = PaneGroupHealth::from_registry(&r, &denials);
        assert_eq!(h.groups_total, 1);
        assert_eq!(h.grouped_panes_total, 2);
        assert!(h.ops_applied_total >= 3);
        assert_eq!(h.ops_denied_total, 1);
    }

    #[test]
    fn background_job_health_unsafe_when_critical_queued() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "tx_commit", JobPriority::Critical));
        let h = BackgroundJobHealth::from_queue(&q);
        assert_eq!(h.critical_queued(), 1);
        assert!(!h.is_safe());
    }

    #[test]
    fn background_job_health_safe_with_no_critical() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "poll", JobPriority::Normal));
        let h = BackgroundJobHealth::from_queue(&q);
        assert!(h.is_safe());
    }

    #[test]
    fn background_job_health_from_queue_per_priority_counts() {
        let mut q = BackgroundJobQueue::new();
        q.enqueue(job(1, "n1", JobPriority::Normal));
        q.enqueue(job(2, "n2", JobPriority::Normal));
        q.enqueue(job(3, "h1", JobPriority::High));
        let h = BackgroundJobHealth::from_queue(&q);
        assert_eq!(h.queue_depth, 3);
        assert_eq!(h.queue_depth_per_priority.get("normal"), Some(&2));
        assert_eq!(h.queue_depth_per_priority.get("high"), Some(&1));
    }

    // ------------------------------------------------------------------------
    // Structured logging
    // ------------------------------------------------------------------------

    #[test]
    fn structured_log_jsonl_roundtrip() {
        let rows = vec![
            StructuredLogRow::GroupOp {
                ts_ms: 1_000,
                group_name: "cc-agents".to_string(),
                op_slug: "close_all".to_string(),
                pane_count: 5,
                success: true,
            },
            StructuredLogRow::BackgroundJob {
                ts_ms: 2_000,
                job_kind: "tx_commit".to_string(),
                priority: "critical".to_string(),
                scheduled_at_ms: 1_900,
                started_at_ms: Some(2_000),
                completed_at_ms: Some(2_005),
                status: "completed".to_string(),
            },
        ];
        let jsonl = render_log_jsonl(&rows);
        let parsed = parse_log_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, rows);
    }

    // ------------------------------------------------------------------------
    // Integration shape: bead's headline scenario
    // ------------------------------------------------------------------------

    #[test]
    fn ai_agents_group_kill_all_scenario() {
        // Bead's stated user value: "AI agents can group
        // panes: 'all panes running cc' → one operation
        // kills/closes them all."
        let mut r = PaneGroupRegistry::new();
        apply_group_op(
            &mut r,
            &GroupOp::Create {
                id: 1,
                name: "cc-agents".to_string(),
            },
        );
        for pane in 0..10 {
            apply_group_op(&mut r, &GroupOp::Add { group: 1, pane });
        }
        let outcome = apply_group_op(&mut r, &GroupOp::KillAll { group: 1 });
        assert_eq!(outcome, GroupOpOutcome::Applied { panes_affected: 10 });
        assert!(r.groups.is_empty());
    }
}
