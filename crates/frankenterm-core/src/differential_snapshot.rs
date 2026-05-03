//! Differential (incremental) snapshots for continuous background saving.
//!
//! Instead of capturing the entire mux state every time, this module tracks
//! which panes have changed ("dirty set") and records only the diffs.
//! Snapshots form a chain: `base → diff₁ → diff₂ → …` that can be
//! replayed to reconstruct state at any diff point.
//!
//! # Architecture
//!
//! ```text
//! DirtyTracker  ←──  pane events (output, resize, title, close, create)
//!       │
//!       ▼
//! DiffSnapshotEngine::capture_diff()
//!       │
//!       ├── only captures dirty panes
//!       ├── emits Vec<SnapshotDiff>
//!       └── clears dirty set
//!       │
//!       ▼
//! DiffChain  (base + ordered diffs)
//!       │
//!       ├── restore()  → full state at any diff point
//!       └── compact()  → merge chain into new base
//! ```
//!
//! See bead `wa-3kxe.3` for the full design.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

// =============================================================================
// Telemetry
// =============================================================================

/// Operational telemetry counters for the differential snapshot engine.
///
/// Uses plain `u64` because `DiffSnapshotEngine` methods take `&mut self`.
#[derive(Debug, Clone, Default)]
pub struct DiffSnapshotTelemetry {
    /// Successful capture_diff() calls that produced a diff.
    diffs_captured: u64,
    /// capture_diff() calls skipped because tracker was clean.
    clean_skips: u64,
    /// Auto-compactions triggered by exceeding max_chain_len.
    auto_compactions: u64,
    /// Explicit compact() calls.
    manual_compactions: u64,
    /// Total individual diff entries across all captured snapshots.
    total_diff_entries: u64,
    /// Layout change diffs captured.
    layout_diffs: u64,
}

impl DiffSnapshotTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> DiffSnapshotTelemetrySnapshot {
        DiffSnapshotTelemetrySnapshot {
            diffs_captured: self.diffs_captured,
            clean_skips: self.clean_skips,
            auto_compactions: self.auto_compactions,
            manual_compactions: self.manual_compactions,
            total_diff_entries: self.total_diff_entries,
            layout_diffs: self.layout_diffs,
        }
    }
}

/// Serializable snapshot of differential snapshot engine telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSnapshotTelemetrySnapshot {
    /// Successful capture_diff() calls that produced a diff.
    pub diffs_captured: u64,
    /// capture_diff() calls skipped because tracker was clean.
    pub clean_skips: u64,
    /// Auto-compactions triggered by exceeding max_chain_len.
    pub auto_compactions: u64,
    /// Explicit compact() calls.
    pub manual_compactions: u64,
    /// Total individual diff entries across all captured snapshots.
    pub total_diff_entries: u64,
    /// Layout change diffs captured.
    pub layout_diffs: u64,
}

use crate::session_pane_state::PaneStateSnapshot;
use crate::session_topology::TopologySnapshot;

// =============================================================================
// Dirty tracking
// =============================================================================

/// What aspect of a pane changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirtyField {
    /// New scrollback output received.
    Scrollback,
    /// Terminal metadata changed (title, cursor, cwd, size).
    Metadata,
    /// Pane was newly created.
    Created,
    /// Pane was closed / destroyed.
    Closed,
}

/// Tracks which panes have been modified since the last differential snapshot.
///
/// Thread-safe via interior mutability is not needed here — the tracker is
/// owned by the `DiffSnapshotEngine` which serializes access.
#[derive(Debug, Clone)]
pub struct DirtyTracker {
    /// Pane ID → set of dirty fields.
    dirty: HashMap<u64, HashSet<DirtyField>>,
    /// Pane IDs that were created since last snapshot.
    created: HashSet<u64>,
    /// Pane IDs that were closed since last snapshot.
    closed: HashSet<u64>,
    /// Whether the layout topology changed.
    layout_dirty: bool,
}

impl DirtyTracker {
    /// Create a new empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dirty: HashMap::new(),
            created: HashSet::new(),
            closed: HashSet::new(),
            layout_dirty: false,
        }
    }

    /// Mark a specific field of a pane as dirty.
    pub fn mark_dirty(&mut self, pane_id: u64, field: DirtyField) {
        self.dirty.entry(pane_id).or_default().insert(field);

        match field {
            DirtyField::Created => {
                self.created.insert(pane_id);
                self.layout_dirty = true;
            }
            DirtyField::Closed => {
                self.closed.insert(pane_id);
                self.layout_dirty = true;
            }
            _ => {}
        }
    }

    /// Mark a pane as having new scrollback output.
    pub fn mark_output(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Scrollback);
    }

    /// Mark a pane's metadata as changed.
    pub fn mark_metadata(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Metadata);
    }

    /// Mark a pane as newly created.
    pub fn mark_created(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Created);
    }

    /// Mark a pane as closed.
    pub fn mark_closed(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Closed);
    }

    /// Mark the layout topology as changed.
    pub fn mark_layout_dirty(&mut self) {
        self.layout_dirty = true;
    }

    /// Returns the set of dirty pane IDs.
    #[must_use]
    pub fn dirty_pane_ids(&self) -> HashSet<u64> {
        self.dirty.keys().copied().collect()
    }

    /// Returns the dirty fields for a specific pane.
    #[must_use]
    pub fn dirty_fields(&self, pane_id: u64) -> Option<&HashSet<DirtyField>> {
        self.dirty.get(&pane_id)
    }

    /// Check if the layout is dirty.
    #[must_use]
    pub fn is_layout_dirty(&self) -> bool {
        self.layout_dirty
    }

    /// Returns true if there are no dirty panes or layout changes.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.dirty.is_empty() && !self.layout_dirty
    }

    /// Returns the total number of dirty panes.
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// Clear all dirty state (called after a successful diff snapshot).
    pub fn clear(&mut self) {
        self.dirty.clear();
        self.created.clear();
        self.closed.clear();
        self.layout_dirty = false;
    }
}

impl Default for DirtyTracker {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Diff records
// =============================================================================

/// A single diff record describing what changed in one snapshot delta.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum SnapshotDiff {
    /// Pane scrollback content changed (new lines appended).
    PaneScrollbackChanged {
        pane_id: u64,
        /// Updated scrollback reference (latest seq, line count).
        new_scrollback_ref: Option<crate::session_pane_state::ScrollbackRef>,
    },
    /// Pane metadata changed (title, cursor, cwd, size, agent state).
    PaneMetadataChanged {
        pane_id: u64,
        /// The full new pane state snapshot (replaces prior state).
        new_state: PaneStateSnapshot,
    },
    /// A new pane was created.
    PaneCreated {
        pane_id: u64,
        /// Full initial state of the new pane.
        snapshot: PaneStateSnapshot,
    },
    /// A pane was closed / destroyed.
    PaneClosed { pane_id: u64 },
    /// The layout topology changed.
    LayoutChanged {
        /// Full new topology (we don't diff topology — it's small).
        new_topology: TopologySnapshot,
    },
}

impl SnapshotDiff {
    /// Returns the pane ID affected by this diff, if applicable.
    #[must_use]
    pub fn pane_id(&self) -> Option<u64> {
        match self {
            Self::PaneScrollbackChanged { pane_id, .. }
            | Self::PaneMetadataChanged { pane_id, .. }
            | Self::PaneCreated { pane_id, .. }
            | Self::PaneClosed { pane_id } => Some(*pane_id),
            Self::LayoutChanged { .. } => None,
        }
    }
}

// =============================================================================
// Diff snapshot (a single delta)
// =============================================================================

/// A single differential snapshot — the set of changes from the previous state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffSnapshot {
    /// Monotonically increasing sequence number within the chain.
    pub seq: u64,
    /// When this diff was captured (epoch ms).
    pub captured_at: u64,
    /// The individual diff records.
    pub diffs: Vec<SnapshotDiff>,
}

// =============================================================================
// Base snapshot (full state at a point in time)
// =============================================================================

/// A full base snapshot from which diffs are applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaseSnapshot {
    /// When this base was captured (epoch ms).
    pub captured_at: u64,
    /// Layout topology at capture time.
    pub topology: TopologySnapshot,
    /// Per-pane states keyed by pane ID.
    pub pane_states: HashMap<u64, PaneStateSnapshot>,
}

impl BaseSnapshot {
    /// Create a base snapshot from a topology and pane state list.
    #[must_use]
    pub fn new(
        captured_at: u64,
        topology: TopologySnapshot,
        pane_states: Vec<PaneStateSnapshot>,
    ) -> Self {
        let pane_map = pane_states.into_iter().map(|ps| (ps.pane_id, ps)).collect();
        Self {
            captured_at,
            topology,
            pane_states: pane_map,
        }
    }

    /// Apply a single diff snapshot to produce a new state.
    ///
    /// This mutates `self` in place for efficiency.
    pub fn apply_diff(&mut self, diff: &DiffSnapshot) {
        self.captured_at = diff.captured_at;

        for record in &diff.diffs {
            match record {
                SnapshotDiff::PaneScrollbackChanged {
                    pane_id,
                    new_scrollback_ref,
                } => {
                    if let Some(state) = self.pane_states.get_mut(pane_id) {
                        state.scrollback_ref.clone_from(new_scrollback_ref);
                    }
                }
                SnapshotDiff::PaneMetadataChanged { pane_id, new_state } => {
                    self.pane_states.insert(*pane_id, new_state.clone());
                }
                SnapshotDiff::PaneCreated { pane_id, snapshot } => {
                    self.pane_states.insert(*pane_id, snapshot.clone());
                }
                SnapshotDiff::PaneClosed { pane_id } => {
                    self.pane_states.remove(pane_id);
                }
                SnapshotDiff::LayoutChanged { new_topology } => {
                    self.topology = new_topology.clone();
                }
            }
        }
    }
}

// =============================================================================
// Diff chain
// =============================================================================

/// An ordered chain of diffs from a base snapshot.
///
/// Supports restoring state at any point in the chain, and compacting
/// the chain into a new base snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffChain {
    /// The base snapshot (full state).
    pub base: BaseSnapshot,
    /// Ordered diff snapshots (oldest first).
    pub diffs: Vec<DiffSnapshot>,
    /// Next sequence number for the next diff.
    next_seq: u64,
}

impl DiffChain {
    /// Create a new chain from a base snapshot.
    #[must_use]
    pub fn new(base: BaseSnapshot) -> Self {
        Self {
            base,
            diffs: Vec::new(),
            next_seq: 1,
        }
    }

    /// Append a new diff to the chain.
    pub fn push_diff(&mut self, mut diff: DiffSnapshot) {
        diff.seq = self.next_seq;
        self.next_seq += 1;
        self.diffs.push(diff);
    }

    /// Restore the full state at the latest diff (or base if no diffs).
    #[must_use]
    pub fn restore_latest(&self) -> BaseSnapshot {
        let mut state = self.base.clone();
        for diff in &self.diffs {
            state.apply_diff(diff);
        }
        state
    }

    /// Restore the full state at a specific sequence number.
    ///
    /// Returns `None` if the sequence number is not in the chain.
    #[must_use]
    pub fn restore_at(&self, seq: u64) -> Option<BaseSnapshot> {
        if seq == 0 {
            return Some(self.base.clone());
        }

        let mut state = self.base.clone();
        for diff in &self.diffs {
            if diff.seq > seq {
                break;
            }
            state.apply_diff(diff);
        }

        // Check if we actually found the requested seq
        if self.diffs.iter().any(|d| d.seq == seq) {
            Some(state)
        } else {
            None
        }
    }

    /// Compact the chain: merge all diffs into a new base snapshot.
    ///
    /// After compaction, the chain has no diffs and the base reflects
    /// the latest state. Returns the number of diffs that were merged.
    pub fn compact(&mut self) -> usize {
        if self.diffs.is_empty() {
            return 0;
        }
        let count = self.diffs.len();
        self.base = self.restore_latest();
        self.diffs.clear();
        // Don't reset next_seq — sequence numbers are monotonic
        count
    }

    /// Number of diffs in the chain.
    #[must_use]
    pub fn chain_len(&self) -> usize {
        self.diffs.len()
    }
}

// =============================================================================
// Snapshot divergence bisect
// =============================================================================

/// Storage-health summary used by snapshot divergence predicates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotStorageHealth {
    /// True when the storage surface was healthy at this snapshot.
    pub healthy: bool,
    /// Optional human-readable health detail.
    pub detail: Option<String>,
}

impl Default for SnapshotStorageHealth {
    fn default() -> Self {
        Self {
            healthy: true,
            detail: None,
        }
    }
}

/// A compact, surface-complete observation for one saved snapshot/checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDivergenceObservation {
    /// Stable snapshot/checkpoint identifier.
    pub snapshot_id: String,
    /// Total-order position when the snapshot chain is linear.
    pub ordinal: u64,
    /// Parent snapshot IDs for partial-order / causal-DAG search.
    pub parents: Vec<String>,
    /// False when the checkpoint row or artifact is known but unavailable.
    pub available: bool,
    /// Pane text sampled from the snapshot.
    pub pane_text: HashMap<u64, String>,
    /// Event types or rule IDs observed at this snapshot.
    pub events: Vec<String>,
    /// Workflow name to durable status at this snapshot.
    pub workflow_states: HashMap<String, String>,
    /// Policy decision ID to decision value at this snapshot.
    pub policy_decisions: HashMap<String, String>,
    /// Storage health for this snapshot.
    pub storage_health: SnapshotStorageHealth,
}

impl SnapshotDivergenceObservation {
    /// Create an available observation with empty surfaces.
    #[must_use]
    pub fn new(snapshot_id: impl Into<String>, ordinal: u64) -> Self {
        Self {
            snapshot_id: snapshot_id.into(),
            ordinal,
            parents: Vec::new(),
            available: true,
            pane_text: HashMap::new(),
            events: Vec::new(),
            workflow_states: HashMap::new(),
            policy_decisions: HashMap::new(),
            storage_health: SnapshotStorageHealth::default(),
        }
    }

    /// Create an unavailable placeholder so the search can report gaps.
    #[must_use]
    pub fn missing(snapshot_id: impl Into<String>, ordinal: u64) -> Self {
        Self {
            available: false,
            ..Self::new(snapshot_id, ordinal)
        }
    }
}

/// Invariant predicates supported by snapshot divergence search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotInvariant {
    /// Pane text must contain `needle`.
    PaneTextContains { pane_id: u64, needle: String },
    /// At least one event/rule ID must match `event_type`.
    EventSeen { event_type: String },
    /// Workflow must have the expected durable status.
    WorkflowStatus {
        workflow_name: String,
        status: String,
    },
    /// Policy decision must match the expected value.
    PolicyDecision {
        decision_id: String,
        expected: String,
    },
    /// Storage must report healthy.
    StorageHealthy,
    /// All child predicates must pass.
    All { predicates: Vec<SnapshotInvariant> },
    /// At least one child predicate must pass.
    Any { predicates: Vec<SnapshotInvariant> },
}

/// Predicate evaluation evidence for one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInvariantEvaluation {
    /// True when the invariant holds.
    pub passed: bool,
    /// Compact evidence for reports and tests.
    pub evidence: String,
}

impl SnapshotInvariant {
    /// Evaluate this invariant against one snapshot observation.
    #[must_use]
    pub fn evaluate(
        &self,
        snapshot: &SnapshotDivergenceObservation,
    ) -> SnapshotInvariantEvaluation {
        match self {
            Self::PaneTextContains { pane_id, needle } => {
                let haystack = snapshot.pane_text.get(pane_id);
                let passed = haystack.is_some_and(|text| text.contains(needle));
                SnapshotInvariantEvaluation {
                    passed,
                    evidence: format!("pane_text[{pane_id}] contains {needle:?}: {passed}"),
                }
            }
            Self::EventSeen { event_type } => {
                let passed = snapshot.events.iter().any(|event| event == event_type);
                SnapshotInvariantEvaluation {
                    passed,
                    evidence: format!("event {event_type:?} seen: {passed}"),
                }
            }
            Self::WorkflowStatus {
                workflow_name,
                status,
            } => {
                let actual = snapshot.workflow_states.get(workflow_name);
                let passed = actual.is_some_and(|actual| actual == status);
                SnapshotInvariantEvaluation {
                    passed,
                    evidence: format!(
                        "workflow {workflow_name:?} status expected {status:?}, actual {actual:?}"
                    ),
                }
            }
            Self::PolicyDecision {
                decision_id,
                expected,
            } => {
                let actual = snapshot.policy_decisions.get(decision_id);
                let passed = actual.is_some_and(|actual| actual == expected);
                SnapshotInvariantEvaluation {
                    passed,
                    evidence: format!(
                        "policy {decision_id:?} expected {expected:?}, actual {actual:?}"
                    ),
                }
            }
            Self::StorageHealthy => SnapshotInvariantEvaluation {
                passed: snapshot.storage_health.healthy,
                evidence: snapshot.storage_health.detail.clone().unwrap_or_else(|| {
                    format!("storage healthy: {}", snapshot.storage_health.healthy)
                }),
            },
            Self::All { predicates } => {
                let evaluations: Vec<_> = predicates
                    .iter()
                    .map(|predicate| predicate.evaluate(snapshot))
                    .collect();
                SnapshotInvariantEvaluation {
                    passed: evaluations.iter().all(|evaluation| evaluation.passed),
                    evidence: evaluations
                        .iter()
                        .map(|evaluation| evaluation.evidence.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                }
            }
            Self::Any { predicates } => {
                let evaluations: Vec<_> = predicates
                    .iter()
                    .map(|predicate| predicate.evaluate(snapshot))
                    .collect();
                SnapshotInvariantEvaluation {
                    passed: evaluations.iter().any(|evaluation| evaluation.passed),
                    evidence: evaluations
                        .iter()
                        .map(|evaluation| evaluation.evidence.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                }
            }
        }
    }
}

/// Search strategy for snapshot divergence analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotDivergenceSearchMode {
    /// Snapshot ordinals form a complete total order suitable for bisection.
    TotalOrder,
    /// Snapshot parents form a causal DAG / partial order.
    PartialOrder,
}

/// One auditable search decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDivergenceDecision {
    /// Snapshot inspected or skipped.
    pub snapshot_id: String,
    /// Snapshot ordinal.
    pub ordinal: u64,
    /// Decision phase, e.g. `bisect_probe`, `dag_probe`, or `skipped`.
    pub phase: String,
    /// Search result for this decision.
    pub result: String,
    /// Compact evidence or reason.
    pub evidence: String,
}

/// Divergence search outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotDivergenceOutcome {
    /// No available snapshot violated the invariant.
    NoDivergence { last_checked: Vec<String> },
    /// The first bad transition is fully proven.
    FirstBadTransition {
        good_before: Vec<String>,
        first_bad: Vec<String>,
        evidence: Vec<String>,
    },
    /// Missing data prevents proving a single first-bad transition.
    SuspectInterval {
        lower_bound_good: Vec<String>,
        upper_bound_bad: Vec<String>,
        reason: String,
    },
}

/// Full divergence report, including skipped snapshots and probe decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDivergenceReport {
    /// Search strategy used.
    pub mode: SnapshotDivergenceSearchMode,
    /// Final outcome.
    pub outcome: SnapshotDivergenceOutcome,
    /// Auditable decision log.
    pub decisions: Vec<SnapshotDivergenceDecision>,
    /// Known snapshots skipped because their artifact or row was unavailable.
    pub skipped_snapshots: Vec<String>,
}

/// Locate the first snapshot/checkpoint divergence for an invariant.
#[must_use]
pub fn search_snapshot_divergence(
    snapshots: &[SnapshotDivergenceObservation],
    invariant: &SnapshotInvariant,
    mode: SnapshotDivergenceSearchMode,
) -> SnapshotDivergenceReport {
    match mode {
        SnapshotDivergenceSearchMode::TotalOrder => {
            search_total_order_divergence(snapshots, invariant)
        }
        SnapshotDivergenceSearchMode::PartialOrder => {
            search_partial_order_divergence(snapshots, invariant)
        }
    }
}

fn search_total_order_divergence(
    snapshots: &[SnapshotDivergenceObservation],
    invariant: &SnapshotInvariant,
) -> SnapshotDivergenceReport {
    let mut ordered: Vec<&SnapshotDivergenceObservation> = snapshots.iter().collect();
    ordered.sort_by(|left, right| {
        left.ordinal
            .cmp(&right.ordinal)
            .then_with(|| left.snapshot_id.cmp(&right.snapshot_id))
    });

    let mut decisions = Vec::new();
    let mut skipped_snapshots = Vec::new();
    let mut available_indexes = Vec::new();
    for (index, snapshot) in ordered.iter().enumerate() {
        if snapshot.available {
            available_indexes.push(index);
        } else {
            skipped_snapshots.push(snapshot.snapshot_id.clone());
            decisions.push(SnapshotDivergenceDecision {
                snapshot_id: snapshot.snapshot_id.clone(),
                ordinal: snapshot.ordinal,
                phase: "skipped".to_string(),
                result: "missing_snapshot".to_string(),
                evidence: "snapshot artifact unavailable".to_string(),
            });
        }
    }

    if available_indexes.is_empty() {
        return SnapshotDivergenceReport {
            mode: SnapshotDivergenceSearchMode::TotalOrder,
            outcome: SnapshotDivergenceOutcome::SuspectInterval {
                lower_bound_good: Vec::new(),
                upper_bound_bad: Vec::new(),
                reason: "no available snapshots to evaluate".to_string(),
            },
            decisions,
            skipped_snapshots,
        };
    }

    let mut low = 0usize;
    let mut high = available_indexes.len();
    while low < high {
        let mid = low + ((high - low) / 2);
        let snapshot = ordered[available_indexes[mid]];
        let evaluation = invariant.evaluate(snapshot);
        decisions.push(SnapshotDivergenceDecision {
            snapshot_id: snapshot.snapshot_id.clone(),
            ordinal: snapshot.ordinal,
            phase: "bisect_probe".to_string(),
            result: if evaluation.passed { "pass" } else { "fail" }.to_string(),
            evidence: evaluation.evidence,
        });
        if evaluation.passed {
            low = mid + 1;
        } else {
            high = mid;
        }
    }

    if low == available_indexes.len() {
        let last = ordered[*available_indexes.last().expect("available index exists")];
        return SnapshotDivergenceReport {
            mode: SnapshotDivergenceSearchMode::TotalOrder,
            outcome: SnapshotDivergenceOutcome::NoDivergence {
                last_checked: vec![last.snapshot_id.clone()],
            },
            decisions,
            skipped_snapshots,
        };
    }

    let bad_index = available_indexes[low];
    let bad = ordered[bad_index];
    let bad_evaluation = invariant.evaluate(bad);
    let lower_index = low.checked_sub(1).map(|idx| available_indexes[idx]);
    let missing_between: Vec<String> = ordered[lower_index.map_or(0, |idx| idx + 1)..bad_index]
        .iter()
        .filter(|snapshot| !snapshot.available)
        .map(|snapshot| snapshot.snapshot_id.clone())
        .collect();

    let outcome = if missing_between.is_empty() {
        SnapshotDivergenceOutcome::FirstBadTransition {
            good_before: lower_index
                .map(|idx| vec![ordered[idx].snapshot_id.clone()])
                .unwrap_or_default(),
            first_bad: vec![bad.snapshot_id.clone()],
            evidence: vec![bad_evaluation.evidence],
        }
    } else {
        SnapshotDivergenceOutcome::SuspectInterval {
            lower_bound_good: lower_index
                .map(|idx| vec![ordered[idx].snapshot_id.clone()])
                .unwrap_or_default(),
            upper_bound_bad: vec![bad.snapshot_id.clone()],
            reason: format!(
                "missing snapshots prevent proving the exact first bad transition: {}",
                missing_between.join(", ")
            ),
        }
    };

    SnapshotDivergenceReport {
        mode: SnapshotDivergenceSearchMode::TotalOrder,
        outcome,
        decisions,
        skipped_snapshots,
    }
}

fn search_partial_order_divergence(
    snapshots: &[SnapshotDivergenceObservation],
    invariant: &SnapshotInvariant,
) -> SnapshotDivergenceReport {
    let by_id: HashMap<&str, &SnapshotDivergenceObservation> = snapshots
        .iter()
        .map(|snapshot| (snapshot.snapshot_id.as_str(), snapshot))
        .collect();
    let mut ordered: Vec<&SnapshotDivergenceObservation> = snapshots.iter().collect();
    ordered.sort_by(|left, right| {
        left.ordinal
            .cmp(&right.ordinal)
            .then_with(|| left.snapshot_id.cmp(&right.snapshot_id))
    });

    let mut decisions = Vec::new();
    let mut skipped_snapshots = Vec::new();
    let mut bad_ids = HashSet::new();
    let mut good_ids = HashSet::new();
    let mut evidence_by_id = HashMap::new();

    for snapshot in ordered {
        if !snapshot.available {
            skipped_snapshots.push(snapshot.snapshot_id.clone());
            decisions.push(SnapshotDivergenceDecision {
                snapshot_id: snapshot.snapshot_id.clone(),
                ordinal: snapshot.ordinal,
                phase: "skipped".to_string(),
                result: "missing_snapshot".to_string(),
                evidence: "snapshot artifact unavailable".to_string(),
            });
            continue;
        }

        let evaluation = invariant.evaluate(snapshot);
        let result = if evaluation.passed { "pass" } else { "fail" };
        decisions.push(SnapshotDivergenceDecision {
            snapshot_id: snapshot.snapshot_id.clone(),
            ordinal: snapshot.ordinal,
            phase: "dag_probe".to_string(),
            result: result.to_string(),
            evidence: evaluation.evidence.clone(),
        });
        evidence_by_id.insert(snapshot.snapshot_id.clone(), evaluation.evidence);
        if evaluation.passed {
            good_ids.insert(snapshot.snapshot_id.clone());
        } else {
            bad_ids.insert(snapshot.snapshot_id.clone());
        }
    }

    if bad_ids.is_empty() {
        let mut last_checked: Vec<String> = good_ids.into_iter().collect();
        last_checked.sort();
        return SnapshotDivergenceReport {
            mode: SnapshotDivergenceSearchMode::PartialOrder,
            outcome: SnapshotDivergenceOutcome::NoDivergence { last_checked },
            decisions,
            skipped_snapshots,
        };
    }

    let mut first_bad: Vec<String> = bad_ids
        .iter()
        .filter(|id| !has_bad_ancestor(id, &bad_ids, &by_id, &mut HashSet::new()))
        .cloned()
        .collect();
    first_bad.sort_by(|left, right| {
        let left_ord = by_id
            .get(left.as_str())
            .map_or(u64::MAX, |snapshot| snapshot.ordinal);
        let right_ord = by_id
            .get(right.as_str())
            .map_or(u64::MAX, |snapshot| snapshot.ordinal);
        left_ord.cmp(&right_ord).then_with(|| left.cmp(right))
    });

    let mut good_before = HashSet::new();
    let mut missing_before = HashSet::new();
    for id in &first_bad {
        collect_parent_status(
            id,
            &by_id,
            &good_ids,
            &mut good_before,
            &mut missing_before,
            &mut HashSet::new(),
            true,
        );
    }

    let outcome = if missing_before.is_empty() {
        let mut good_before: Vec<_> = good_before.into_iter().collect();
        good_before.sort();
        SnapshotDivergenceOutcome::FirstBadTransition {
            good_before,
            evidence: first_bad
                .iter()
                .filter_map(|id| evidence_by_id.get(id).cloned())
                .collect(),
            first_bad,
        }
    } else {
        let mut lower_bound_good: Vec<_> = good_before.into_iter().collect();
        lower_bound_good.sort();
        let mut missing: Vec<_> = missing_before.into_iter().collect();
        missing.sort();
        SnapshotDivergenceOutcome::SuspectInterval {
            lower_bound_good,
            upper_bound_bad: first_bad,
            reason: format!(
                "partial-order parents are missing, smallest suspect frontier includes: {}",
                missing.join(", ")
            ),
        }
    };

    SnapshotDivergenceReport {
        mode: SnapshotDivergenceSearchMode::PartialOrder,
        outcome,
        decisions,
        skipped_snapshots,
    }
}

fn has_bad_ancestor(
    id: &str,
    bad_ids: &HashSet<String>,
    by_id: &HashMap<&str, &SnapshotDivergenceObservation>,
    visited: &mut HashSet<String>,
) -> bool {
    if !visited.insert(id.to_string()) {
        return false;
    }
    let Some(snapshot) = by_id.get(id) else {
        return false;
    };
    snapshot
        .parents
        .iter()
        .any(|parent| bad_ids.contains(parent) || has_bad_ancestor(parent, bad_ids, by_id, visited))
}

fn collect_parent_status(
    id: &str,
    by_id: &HashMap<&str, &SnapshotDivergenceObservation>,
    good_ids: &HashSet<String>,
    good_before: &mut HashSet<String>,
    missing_before: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    record_good_parent: bool,
) {
    if !visited.insert(id.to_string()) {
        return;
    }
    let Some(snapshot) = by_id.get(id) else {
        missing_before.insert(id.to_string());
        return;
    };
    for parent in &snapshot.parents {
        match by_id.get(parent.as_str()) {
            Some(parent_snapshot) if parent_snapshot.available && good_ids.contains(parent) => {
                if record_good_parent {
                    good_before.insert(parent.clone());
                }
                collect_parent_status(
                    parent,
                    by_id,
                    good_ids,
                    good_before,
                    missing_before,
                    visited,
                    false,
                );
            }
            Some(parent_snapshot) if !parent_snapshot.available => {
                missing_before.insert(parent_snapshot.snapshot_id.clone());
            }
            Some(_) => {}
            None => {
                missing_before.insert(parent.clone());
            }
        }
    }
}

// =============================================================================
// Diff snapshot engine
// =============================================================================

/// Engine that captures differential snapshots using a dirty tracker.
///
/// Usage:
/// 1. As pane events occur, call `tracker_mut().mark_*()` methods.
/// 2. Periodically call `capture_diff()` to produce a diff snapshot.
/// 3. The engine maintains the diff chain internally.
/// 4. Call `compact()` when the chain gets too long.
#[derive(Debug)]
pub struct DiffSnapshotEngine {
    /// Dirty tracker for change detection.
    tracker: DirtyTracker,
    /// The diff chain (base + diffs).
    chain: Option<DiffChain>,
    /// Maximum chain length before auto-compaction.
    max_chain_len: usize,
    /// Operational telemetry counters.
    telemetry: DiffSnapshotTelemetry,
}

impl DiffSnapshotEngine {
    /// Create a new diff snapshot engine.
    ///
    /// `max_chain_len` controls when auto-compaction triggers (0 = never).
    #[must_use]
    pub fn new(max_chain_len: usize) -> Self {
        Self {
            tracker: DirtyTracker::new(),
            chain: None,
            max_chain_len,
            telemetry: DiffSnapshotTelemetry::new(),
        }
    }

    /// Access the dirty tracker for marking changes.
    pub fn tracker_mut(&mut self) -> &mut DirtyTracker {
        &mut self.tracker
    }

    /// Access the dirty tracker (read-only).
    #[must_use]
    pub fn tracker(&self) -> &DirtyTracker {
        &self.tracker
    }

    /// Initialize with a full base snapshot.
    ///
    /// This must be called before `capture_diff()`. Typically called once
    /// with the initial full snapshot from `SnapshotEngine`.
    pub fn initialize(&mut self, base: BaseSnapshot) {
        self.chain = Some(DiffChain::new(base));
        self.tracker.clear();
    }

    /// Returns true if the engine has been initialized with a base snapshot.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.chain.is_some()
    }

    /// Capture a differential snapshot of only the dirty panes.
    ///
    /// `current_panes` provides the current state of panes (only dirty ones
    /// are read). `current_topology` is used if the layout changed.
    ///
    /// Returns `None` if nothing changed.
    pub fn capture_diff(
        &mut self,
        current_panes: &HashMap<u64, PaneStateSnapshot>,
        current_topology: Option<&TopologySnapshot>,
        now_ms: u64,
    ) -> Option<DiffSnapshot> {
        let chain = self.chain.as_mut()?;

        if self.tracker.is_clean() {
            self.telemetry.clean_skips += 1;
            return None;
        }

        let mut diffs = Vec::new();

        // Process closed panes first (before they disappear from current_panes)
        for &pane_id in &self.tracker.closed {
            diffs.push(SnapshotDiff::PaneClosed { pane_id });
        }

        // Process created panes
        for &pane_id in &self.tracker.created {
            if let Some(snapshot) = current_panes.get(&pane_id) {
                diffs.push(SnapshotDiff::PaneCreated {
                    pane_id,
                    snapshot: snapshot.clone(),
                });
            }
        }

        // Process dirty panes (excluding created/closed which are already handled)
        for (&pane_id, fields) in &self.tracker.dirty {
            if self.tracker.created.contains(&pane_id) || self.tracker.closed.contains(&pane_id) {
                continue;
            }

            if let Some(current) = current_panes.get(&pane_id) {
                if fields.contains(&DirtyField::Metadata) {
                    diffs.push(SnapshotDiff::PaneMetadataChanged {
                        pane_id,
                        new_state: current.clone(),
                    });
                }

                if fields.contains(&DirtyField::Scrollback) {
                    diffs.push(SnapshotDiff::PaneScrollbackChanged {
                        pane_id,
                        new_scrollback_ref: current.scrollback_ref.clone(),
                    });
                }
            }
        }

        // Process layout changes
        if self.tracker.is_layout_dirty() {
            if let Some(topo) = current_topology {
                diffs.push(SnapshotDiff::LayoutChanged {
                    new_topology: topo.clone(),
                });
                self.telemetry.layout_diffs += 1;
            }
        }

        if diffs.is_empty() {
            self.tracker.clear();
            return None;
        }

        self.telemetry.diffs_captured += 1;
        self.telemetry.total_diff_entries += diffs.len() as u64;

        let diff = DiffSnapshot {
            seq: 0, // will be set by push_diff
            captured_at: now_ms,
            diffs,
        };

        chain.push_diff(diff.clone());
        self.tracker.clear();

        // Auto-compact if chain is too long
        if self.max_chain_len > 0 && chain.chain_len() > self.max_chain_len {
            chain.compact();
            self.telemetry.auto_compactions += 1;
        }

        Some(diff)
    }

    /// Restore the latest state from the diff chain.
    #[must_use]
    pub fn restore_latest(&self) -> Option<BaseSnapshot> {
        self.chain.as_ref().map(DiffChain::restore_latest)
    }

    /// Manually trigger compaction of the diff chain.
    ///
    /// Returns the number of diffs merged, or `None` if not initialized.
    pub fn compact(&mut self) -> Option<usize> {
        let result = self.chain.as_mut().map(DiffChain::compact);
        if result.is_some() {
            self.telemetry.manual_compactions += 1;
        }
        result
    }

    /// Access telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &DiffSnapshotTelemetry {
        &self.telemetry
    }

    /// Returns the current chain length (number of diffs since last base).
    #[must_use]
    pub fn chain_len(&self) -> usize {
        self.chain.as_ref().map_or(0, DiffChain::chain_len)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_pane_state::{ScrollbackRef, TerminalState};
    use crate::session_topology::{
        PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
    };

    // ---- Helpers ----

    fn make_terminal(rows: u16, cols: u16) -> TerminalState {
        TerminalState {
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            is_alt_screen: false,
            title: "test".to_string(),
        }
    }

    fn make_pane_state(pane_id: u64, rows: u16, cols: u16) -> PaneStateSnapshot {
        PaneStateSnapshot::new(pane_id, 1000, make_terminal(rows, cols))
            .with_cwd(format!("/home/user/pane-{pane_id}"))
    }

    fn make_topology(pane_ids: &[u64]) -> TopologySnapshot {
        let tabs: Vec<TabSnapshot> = pane_ids
            .iter()
            .map(|&id| TabSnapshot {
                tab_id: id,
                title: Some(format!("tab-{id}")),
                pane_tree: PaneNode::Leaf {
                    pane_id: id,
                    rows: 24,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: false,
                },
                active_pane_id: Some(id),
            })
            .collect();

        TopologySnapshot {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: Some("test-window".to_string()),
                position: None,
                size: None,
                tabs,
                active_tab_index: Some(0),
            }],
        }
    }

    fn make_base(pane_ids: &[u64]) -> BaseSnapshot {
        let pane_states: Vec<PaneStateSnapshot> = pane_ids
            .iter()
            .map(|&id| make_pane_state(id, 24, 80))
            .collect();
        BaseSnapshot::new(1000, make_topology(pane_ids), pane_states)
    }

    fn observed_snapshot(id: &str, ordinal: u64, ok: bool) -> SnapshotDivergenceObservation {
        let mut snapshot = SnapshotDivergenceObservation::new(id, ordinal);
        snapshot.pane_text.insert(
            7,
            if ok {
                "agent recovered and prompt is ready".to_string()
            } else {
                "agent hit fatal divergence marker".to_string()
            },
        );
        snapshot.events.push(if ok {
            "workflow.recovered".to_string()
        } else {
            "workflow.diverged".to_string()
        });
        snapshot.workflow_states.insert(
            "recover-pane".to_string(),
            if ok { "completed" } else { "failed" }.to_string(),
        );
        snapshot.policy_decisions.insert(
            "send-input".to_string(),
            if ok { "allow" } else { "deny" }.to_string(),
        );
        snapshot.storage_health = SnapshotStorageHealth {
            healthy: ok,
            detail: Some(if ok {
                "storage healthy".to_string()
            } else {
                "storage health probe failed".to_string()
            }),
        };
        snapshot
    }

    fn healthy_recovery_invariant() -> SnapshotInvariant {
        SnapshotInvariant::All {
            predicates: vec![
                SnapshotInvariant::PaneTextContains {
                    pane_id: 7,
                    needle: "ready".to_string(),
                },
                SnapshotInvariant::EventSeen {
                    event_type: "workflow.recovered".to_string(),
                },
                SnapshotInvariant::WorkflowStatus {
                    workflow_name: "recover-pane".to_string(),
                    status: "completed".to_string(),
                },
                SnapshotInvariant::PolicyDecision {
                    decision_id: "send-input".to_string(),
                    expected: "allow".to_string(),
                },
                SnapshotInvariant::StorageHealthy,
            ],
        }
    }

    // ---- DirtyTracker tests ----

    #[test]
    fn divergence_predicate_evaluates_all_supported_surfaces() {
        let good = observed_snapshot("s1", 1, true);
        let bad = observed_snapshot("s2", 2, false);
        let invariant = healthy_recovery_invariant();

        let good_evaluation = invariant.evaluate(&good);
        assert!(good_evaluation.passed);
        assert!(good_evaluation.evidence.contains("pane_text[7]"));
        assert!(good_evaluation.evidence.contains("workflow"));
        assert!(good_evaluation.evidence.contains("policy"));
        assert!(good_evaluation.evidence.contains("storage healthy"));

        let bad_evaluation = invariant.evaluate(&bad);
        assert!(!bad_evaluation.passed);
        assert!(
            bad_evaluation
                .evidence
                .contains("storage health probe failed")
        );
    }

    #[test]
    fn divergence_total_order_bisect_finds_first_bad_transition() {
        let snapshots = vec![
            observed_snapshot("s1", 1, true),
            observed_snapshot("s2", 2, true),
            observed_snapshot("s3", 3, false),
            observed_snapshot("s4", 4, false),
        ];

        let report = search_snapshot_divergence(
            &snapshots,
            &healthy_recovery_invariant(),
            SnapshotDivergenceSearchMode::TotalOrder,
        );

        assert_eq!(
            report.outcome,
            SnapshotDivergenceOutcome::FirstBadTransition {
                good_before: vec!["s2".to_string()],
                first_bad: vec!["s3".to_string()],
                evidence: vec![
                    healthy_recovery_invariant()
                        .evaluate(&snapshots[2])
                        .evidence
                ],
            }
        );
        assert!(
            report
                .decisions
                .iter()
                .any(|decision| decision.phase == "bisect_probe")
        );
        assert!(report.skipped_snapshots.is_empty());
    }

    #[test]
    fn divergence_total_order_missing_snapshot_returns_suspect_interval() {
        let snapshots = vec![
            observed_snapshot("s1", 1, true),
            SnapshotDivergenceObservation::missing("s2", 2),
            observed_snapshot("s3", 3, false),
        ];

        let report = search_snapshot_divergence(
            &snapshots,
            &healthy_recovery_invariant(),
            SnapshotDivergenceSearchMode::TotalOrder,
        );

        assert_eq!(report.skipped_snapshots, vec!["s2".to_string()]);
        match report.outcome {
            SnapshotDivergenceOutcome::SuspectInterval {
                lower_bound_good,
                upper_bound_bad,
                reason,
            } => {
                assert_eq!(lower_bound_good, vec!["s1".to_string()]);
                assert_eq!(upper_bound_bad, vec!["s3".to_string()]);
                assert!(reason.contains("s2"));
            }
            other => panic!("expected suspect interval for incomplete chain, got {other:?}"),
        }
    }

    #[test]
    fn divergence_partial_order_finds_minimal_bad_frontier() {
        let root = observed_snapshot("root", 1, true);
        let mut left = observed_snapshot("left", 2, true);
        left.parents = vec!["root".to_string()];
        let mut right = observed_snapshot("right", 3, true);
        right.parents = vec!["root".to_string()];
        let mut bad_left = observed_snapshot("bad-left", 4, false);
        bad_left.parents = vec!["left".to_string()];
        let mut bad_right = observed_snapshot("bad-right", 5, false);
        bad_right.parents = vec!["right".to_string()];
        let mut child_of_bad = observed_snapshot("child-of-bad", 6, false);
        child_of_bad.parents = vec!["bad-left".to_string()];

        let report = search_snapshot_divergence(
            &[root, left, right, bad_left, bad_right, child_of_bad],
            &healthy_recovery_invariant(),
            SnapshotDivergenceSearchMode::PartialOrder,
        );

        assert_eq!(
            report.outcome,
            SnapshotDivergenceOutcome::FirstBadTransition {
                good_before: vec!["left".to_string(), "right".to_string()],
                first_bad: vec!["bad-left".to_string(), "bad-right".to_string()],
                evidence: vec![
                    healthy_recovery_invariant()
                        .evaluate(&observed_snapshot("bad-left", 4, false))
                        .evidence,
                    healthy_recovery_invariant()
                        .evaluate(&observed_snapshot("bad-right", 5, false))
                        .evidence,
                ],
            }
        );
        assert!(
            report
                .decisions
                .iter()
                .any(|decision| decision.phase == "dag_probe" && decision.result == "fail")
        );
    }

    #[test]
    fn divergence_partial_order_missing_parent_returns_suspect_frontier() {
        let root = observed_snapshot("root", 1, true);
        let missing = SnapshotDivergenceObservation::missing("missing-parent", 2);
        let mut bad = observed_snapshot("bad", 3, false);
        bad.parents = vec!["missing-parent".to_string()];

        let report = search_snapshot_divergence(
            &[root, missing, bad],
            &healthy_recovery_invariant(),
            SnapshotDivergenceSearchMode::PartialOrder,
        );

        assert_eq!(report.skipped_snapshots, vec!["missing-parent".to_string()]);
        match report.outcome {
            SnapshotDivergenceOutcome::SuspectInterval {
                upper_bound_bad,
                reason,
                ..
            } => {
                assert_eq!(upper_bound_bad, vec!["bad".to_string()]);
                assert!(reason.contains("missing-parent"));
            }
            other => panic!("expected suspect frontier for missing parent, got {other:?}"),
        }
    }

    #[test]
    fn tracker_starts_clean() {
        let tracker = DirtyTracker::new();
        assert!(tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 0);
    }

    #[test]
    fn tracker_marks_output_dirty() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(1);
        assert!(!tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 1);
        assert!(tracker.dirty_pane_ids().contains(&1));
        assert!(
            tracker
                .dirty_fields(1)
                .unwrap()
                .contains(&DirtyField::Scrollback)
        );
    }

    #[test]
    fn tracker_marks_metadata_dirty() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_metadata(2);
        assert!(
            tracker
                .dirty_fields(2)
                .unwrap()
                .contains(&DirtyField::Metadata)
        );
    }

    #[test]
    fn tracker_marks_created() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_created(3);
        assert!(
            tracker
                .dirty_fields(3)
                .unwrap()
                .contains(&DirtyField::Created)
        );
        assert!(tracker.is_layout_dirty());
    }

    #[test]
    fn tracker_marks_closed() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_closed(4);
        assert!(
            tracker
                .dirty_fields(4)
                .unwrap()
                .contains(&DirtyField::Closed)
        );
        assert!(tracker.is_layout_dirty());
    }

    #[test]
    fn tracker_multiple_fields_per_pane() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(1);
        tracker.mark_metadata(1);
        assert_eq!(tracker.dirty_count(), 1);
        let fields = tracker.dirty_fields(1).unwrap();
        assert!(fields.contains(&DirtyField::Scrollback));
        assert!(fields.contains(&DirtyField::Metadata));
    }

    #[test]
    fn tracker_clear_resets_all() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(1);
        tracker.mark_created(2);
        tracker.mark_layout_dirty();
        assert!(!tracker.is_clean());

        tracker.clear();
        assert!(tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 0);
        assert!(!tracker.is_layout_dirty());
    }

    // ---- BaseSnapshot tests ----

    #[test]
    fn base_snapshot_from_pane_list() {
        let base = make_base(&[1, 2, 3]);
        assert_eq!(base.pane_states.len(), 3);
        assert!(base.pane_states.contains_key(&1));
        assert!(base.pane_states.contains_key(&2));
        assert!(base.pane_states.contains_key(&3));
    }

    #[test]
    fn apply_diff_pane_created() {
        let mut base = make_base(&[1, 2]);
        let new_pane = make_pane_state(3, 30, 120);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: new_pane.clone(),
            }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.pane_states.len(), 3);
        assert_eq!(base.pane_states[&3].pane_id, 3);
        assert_eq!(base.captured_at, 2000);
    }

    #[test]
    fn apply_diff_pane_closed() {
        let mut base = make_base(&[1, 2, 3]);
        assert_eq!(base.pane_states.len(), 3);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 2 }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.pane_states.len(), 2);
        assert!(!base.pane_states.contains_key(&2));
    }

    #[test]
    fn apply_diff_metadata_changed() {
        let mut base = make_base(&[1, 2]);
        let mut updated = make_pane_state(1, 30, 120);
        updated.cwd = Some("/new/path".to_string());

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneMetadataChanged {
                pane_id: 1,
                new_state: updated,
            }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.pane_states[&1].cwd, Some("/new/path".to_string()));
        assert_eq!(base.pane_states[&1].terminal.rows, 30);
    }

    #[test]
    fn apply_diff_scrollback_changed() {
        let mut base = make_base(&[1]);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneScrollbackChanged {
                pane_id: 1,
                new_scrollback_ref: Some(ScrollbackRef {
                    output_segments_seq: 42,
                    total_lines_captured: 500,
                    last_capture_at: 1999,
                }),
            }],
        };

        base.apply_diff(&diff);
        let sb = base.pane_states[&1].scrollback_ref.as_ref().unwrap();
        assert_eq!(sb.output_segments_seq, 42);
        assert_eq!(sb.total_lines_captured, 500);
    }

    #[test]
    fn apply_diff_layout_changed() {
        let mut base = make_base(&[1, 2]);
        let new_topo = make_topology(&[1, 2, 3]);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::LayoutChanged {
                new_topology: new_topo.clone(),
            }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.topology.windows[0].tabs.len(), 3);
    }

    // ---- DiffChain tests ----

    #[test]
    fn chain_restore_latest_no_diffs() {
        let base = make_base(&[1, 2]);
        let chain = DiffChain::new(base.clone());
        let restored = chain.restore_latest();
        assert_eq!(restored.pane_states.len(), 2);
        assert_eq!(restored.captured_at, base.captured_at);
    }

    #[test]
    fn chain_restore_latest_with_diffs() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base);

        // Add pane 3
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: make_pane_state(3, 24, 80),
            }],
        });

        // Close pane 1
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        let restored = chain.restore_latest();
        assert_eq!(restored.pane_states.len(), 2);
        assert!(restored.pane_states.contains_key(&2));
        assert!(restored.pane_states.contains_key(&3));
        assert!(!restored.pane_states.contains_key(&1));
        assert_eq!(restored.captured_at, 3000);
    }

    #[test]
    fn chain_restore_at_specific_seq() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: make_pane_state(3, 24, 80),
            }],
        });

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        // At seq 0 (base)
        let at_base = chain.restore_at(0).unwrap();
        assert_eq!(at_base.pane_states.len(), 2);

        // At seq 1 (after adding pane 3)
        let at_1 = chain.restore_at(1).unwrap();
        assert_eq!(at_1.pane_states.len(), 3);

        // At seq 2 (after closing pane 1)
        let at_2 = chain.restore_at(2).unwrap();
        assert_eq!(at_2.pane_states.len(), 2);

        // Invalid seq
        assert!(chain.restore_at(99).is_none());
    }

    #[test]
    fn chain_compact_merges_diffs() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: make_pane_state(3, 24, 80),
            }],
        });

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        assert_eq!(chain.chain_len(), 2);

        let merged = chain.compact();
        assert_eq!(merged, 2);
        assert_eq!(chain.chain_len(), 0);

        // Compacted base should have panes 2 and 3
        assert_eq!(chain.base.pane_states.len(), 2);
        assert!(chain.base.pane_states.contains_key(&2));
        assert!(chain.base.pane_states.contains_key(&3));
    }

    #[test]
    fn chain_compact_empty_is_noop() {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);
        let merged = chain.compact();
        assert_eq!(merged, 0);
    }

    #[test]
    fn chain_sequence_numbers_monotonic() {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);

        for i in 0..5 {
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 1000 + i * 1000,
                diffs: vec![],
            });
        }

        let seqs: Vec<u64> = chain.diffs.iter().map(|d| d.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn chain_seq_survives_compaction() {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![],
        });
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![],
        });

        chain.compact();

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 4000,
            diffs: vec![],
        });

        // After compaction at seq=2, new diff should get seq=3
        assert_eq!(chain.diffs[0].seq, 3);
    }

    // ---- SnapshotDiff tests ----

    #[test]
    fn diff_pane_id_returns_correct_value() {
        let closed = SnapshotDiff::PaneClosed { pane_id: 42 };
        assert_eq!(closed.pane_id(), Some(42));

        let layout = SnapshotDiff::LayoutChanged {
            new_topology: make_topology(&[1]),
        };
        assert_eq!(layout.pane_id(), None);
    }

    #[test]
    fn diff_snapshot_serialization_roundtrip() {
        let diff = DiffSnapshot {
            seq: 5,
            captured_at: 5000,
            diffs: vec![
                SnapshotDiff::PaneCreated {
                    pane_id: 10,
                    snapshot: make_pane_state(10, 24, 80),
                },
                SnapshotDiff::PaneClosed { pane_id: 1 },
            ],
        };

        let json = serde_json::to_string(&diff).unwrap();
        let restored: DiffSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(diff, restored);
    }

    // ---- DiffSnapshotEngine tests ----

    #[test]
    fn engine_not_initialized_returns_none() {
        let mut engine = DiffSnapshotEngine::new(10);
        assert!(!engine.is_initialized());
        assert!(engine.restore_latest().is_none());
        assert!(engine.capture_diff(&HashMap::new(), None, 1000).is_none());
    }

    #[test]
    fn engine_capture_diff_only_dirty_panes() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2, 3, 4, 5]);
        engine.initialize(base);

        // Mark panes 2 and 4 as dirty
        engine.tracker_mut().mark_metadata(2);
        engine.tracker_mut().mark_output(4);

        let mut current = HashMap::new();
        for id in [1, 2, 3, 4, 5] {
            current.insert(id, make_pane_state(id, 24, 80));
        }
        // Modify pane 2 metadata
        current.get_mut(&2).unwrap().cwd = Some("/changed".to_string());

        let diff = engine.capture_diff(&current, None, 2000);
        assert!(diff.is_some());
        let diff = diff.unwrap();

        // Should only have diffs for panes 2 and 4
        assert_eq!(diff.diffs.len(), 2);
        let pane_ids: HashSet<u64> = diff.diffs.iter().filter_map(|d| d.pane_id()).collect();
        assert!(pane_ids.contains(&2));
        assert!(pane_ids.contains(&4));
        assert!(!pane_ids.contains(&1));
        assert!(!pane_ids.contains(&3));
    }

    #[test]
    fn engine_capture_diff_clean_returns_none() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        // No dirty panes
        let diff = engine.capture_diff(&HashMap::new(), None, 2000);
        assert!(diff.is_none());
    }

    #[test]
    fn engine_capture_diff_created_pane() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        engine.tracker_mut().mark_created(3);

        let mut current = HashMap::new();
        current.insert(3, make_pane_state(3, 30, 120));

        let diff = engine.capture_diff(&current, None, 2000).unwrap();
        assert!(
            diff.diffs
                .iter()
                .any(|d| matches!(d, SnapshotDiff::PaneCreated { pane_id: 3, .. }))
        );

        // Restore should have pane 3
        let restored = engine.restore_latest().unwrap();
        assert!(restored.pane_states.contains_key(&3));
    }

    #[test]
    fn engine_capture_diff_closed_pane() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2, 3]);
        engine.initialize(base);

        engine.tracker_mut().mark_closed(2);

        let diff = engine.capture_diff(&HashMap::new(), None, 2000).unwrap();
        assert!(
            diff.diffs
                .iter()
                .any(|d| matches!(d, SnapshotDiff::PaneClosed { pane_id: 2 }))
        );

        let restored = engine.restore_latest().unwrap();
        assert!(!restored.pane_states.contains_key(&2));
        assert_eq!(restored.pane_states.len(), 2);
    }

    #[test]
    fn engine_auto_compaction() {
        let mut engine = DiffSnapshotEngine::new(3); // compact after 3 diffs
        let base = make_base(&[1]);
        engine.initialize(base);

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));

        for i in 0..4 {
            engine.tracker_mut().mark_metadata(1);
            current.get_mut(&1).unwrap().cwd = Some(format!("/path/{i}"));
            engine.capture_diff(&current, None, 2000 + i * 1000);
        }

        // After 4 captures with max_chain_len=3, should have auto-compacted
        // Chain len should be 0 (compacted) or 1 (one after compaction)
        assert!(engine.chain_len() <= 1);
    }

    #[test]
    fn engine_clears_tracker_after_capture() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        engine.tracker_mut().mark_output(1);
        assert!(!engine.tracker().is_clean());

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));
        engine.capture_diff(&current, None, 2000);

        assert!(engine.tracker().is_clean());
    }

    #[test]
    fn engine_layout_change_captured() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        engine.tracker_mut().mark_layout_dirty();

        let new_topo = make_topology(&[1, 2, 3]);
        let diff = engine
            .capture_diff(&HashMap::new(), Some(&new_topo), 2000)
            .unwrap();

        assert!(
            diff.diffs
                .iter()
                .any(|d| matches!(d, SnapshotDiff::LayoutChanged { .. }))
        );

        let restored = engine.restore_latest().unwrap();
        assert_eq!(restored.topology.windows[0].tabs.len(), 3);
    }

    #[test]
    fn engine_manual_compact() {
        let mut engine = DiffSnapshotEngine::new(0); // no auto-compact
        let base = make_base(&[1]);
        engine.initialize(base);

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));

        for i in 0..5 {
            engine.tracker_mut().mark_metadata(1);
            current.get_mut(&1).unwrap().cwd = Some(format!("/path/{i}"));
            engine.capture_diff(&current, None, 2000 + i * 1000);
        }

        assert_eq!(engine.chain_len(), 5);

        let merged = engine.compact().unwrap();
        assert_eq!(merged, 5);
        assert_eq!(engine.chain_len(), 0);
    }

    #[test]
    fn engine_pane_created_after_base_then_closed() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1]);
        engine.initialize(base);

        // Create pane 5
        engine.tracker_mut().mark_created(5);
        let mut current = HashMap::new();
        current.insert(5, make_pane_state(5, 24, 80));
        engine.capture_diff(&current, None, 2000);

        let restored = engine.restore_latest().unwrap();
        assert!(restored.pane_states.contains_key(&5));
        assert_eq!(restored.pane_states.len(), 2);

        // Close pane 5 before next snapshot
        engine.tracker_mut().mark_closed(5);
        engine.capture_diff(&HashMap::new(), None, 3000);

        let restored = engine.restore_latest().unwrap();
        assert!(!restored.pane_states.contains_key(&5));
        assert_eq!(restored.pane_states.len(), 1);
    }

    #[test]
    fn engine_restore_after_compaction_matches_before() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2, 3]);
        engine.initialize(base);

        let mut current: HashMap<u64, PaneStateSnapshot> = HashMap::new();
        for id in [1, 2, 3] {
            current.insert(id, make_pane_state(id, 24, 80));
        }

        // Make several changes
        engine.tracker_mut().mark_metadata(1);
        current.get_mut(&1).unwrap().cwd = Some("/new/path/1".to_string());
        engine.capture_diff(&current, None, 2000);

        engine.tracker_mut().mark_created(4);
        current.insert(4, make_pane_state(4, 30, 120));
        engine.capture_diff(&current, None, 3000);

        engine.tracker_mut().mark_closed(2);
        engine.capture_diff(&current, None, 4000);

        // Snapshot state before compaction
        let before = engine.restore_latest().unwrap();

        // Compact
        engine.compact();

        // State after compaction should be identical
        let after = engine.restore_latest().unwrap();
        assert_eq!(before.pane_states.len(), after.pane_states.len());
        for (id, state) in &before.pane_states {
            assert_eq!(state, after.pane_states.get(id).unwrap());
        }
    }

    // -----------------------------------------------------------------------
    // Batch -- RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn tracker_default_is_clean() {
        let tracker = DirtyTracker::default();
        assert!(tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 0);
        assert!(!tracker.is_layout_dirty());
    }

    #[test]
    fn tracker_dirty_fields_returns_none_for_unknown_pane() {
        let tracker = DirtyTracker::new();
        assert!(tracker.dirty_fields(999).is_none());
    }

    #[test]
    fn tracker_layout_dirty_alone_makes_not_clean() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_layout_dirty();
        assert!(!tracker.is_clean());
        // No dirty panes, but layout is dirty
        assert_eq!(tracker.dirty_count(), 0);
        assert!(tracker.is_layout_dirty());
    }

    #[test]
    fn tracker_scrollback_and_metadata_do_not_set_layout_dirty() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(10);
        tracker.mark_metadata(20);
        assert!(!tracker.is_layout_dirty());
        assert_eq!(tracker.dirty_count(), 2);
    }

    #[test]
    fn tracker_clone_is_independent() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(1);
        tracker.mark_created(2);

        let mut cloned = tracker.clone();
        cloned.clear();

        // Original unchanged
        assert!(!tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 2);
        // Clone is clean
        assert!(cloned.is_clean());
    }

    #[test]
    fn tracker_mark_dirty_all_four_fields_same_pane() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_dirty(1, DirtyField::Scrollback);
        tracker.mark_dirty(1, DirtyField::Metadata);
        tracker.mark_dirty(1, DirtyField::Created);
        tracker.mark_dirty(1, DirtyField::Closed);

        let fields = tracker.dirty_fields(1).unwrap();
        assert_eq!(fields.len(), 4);
        assert!(fields.contains(&DirtyField::Scrollback));
        assert!(fields.contains(&DirtyField::Metadata));
        assert!(fields.contains(&DirtyField::Created));
        assert!(fields.contains(&DirtyField::Closed));
        assert!(tracker.is_layout_dirty());
    }

    #[test]
    fn dirty_field_serde_roundtrip_all_variants() {
        let variants = [
            DirtyField::Scrollback,
            DirtyField::Metadata,
            DirtyField::Created,
            DirtyField::Closed,
        ];
        for field in &variants {
            let json = serde_json::to_string(field).unwrap();
            let restored: DirtyField = serde_json::from_str(&json).unwrap();
            assert_eq!(*field, restored);
        }
    }

    #[test]
    fn dirty_field_serde_uses_snake_case() {
        let json = serde_json::to_string(&DirtyField::Scrollback).unwrap();
        assert_eq!(json, "\"scrollback\"");
        let json = serde_json::to_string(&DirtyField::Metadata).unwrap();
        assert_eq!(json, "\"metadata\"");
        let json = serde_json::to_string(&DirtyField::Created).unwrap();
        assert_eq!(json, "\"created\"");
        let json = serde_json::to_string(&DirtyField::Closed).unwrap();
        assert_eq!(json, "\"closed\"");
    }

    #[test]
    fn dirty_field_clone_copy_eq_hash() {
        let a = DirtyField::Scrollback;
        let b = a; // Copy
        let c = a; // Copy (also Clone)
        assert_eq!(a, b);
        assert_eq!(a, c);

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn snapshot_diff_pane_id_scrollback_changed() {
        let diff = SnapshotDiff::PaneScrollbackChanged {
            pane_id: 77,
            new_scrollback_ref: None,
        };
        assert_eq!(diff.pane_id(), Some(77));
    }

    #[test]
    fn snapshot_diff_pane_id_metadata_changed() {
        let diff = SnapshotDiff::PaneMetadataChanged {
            pane_id: 88,
            new_state: make_pane_state(88, 24, 80),
        };
        assert_eq!(diff.pane_id(), Some(88));
    }

    #[test]
    fn snapshot_diff_pane_id_pane_created() {
        let diff = SnapshotDiff::PaneCreated {
            pane_id: 99,
            snapshot: make_pane_state(99, 24, 80),
        };
        assert_eq!(diff.pane_id(), Some(99));
    }

    #[test]
    fn snapshot_diff_serde_roundtrip_each_variant() {
        // PaneScrollbackChanged
        let d1 = SnapshotDiff::PaneScrollbackChanged {
            pane_id: 1,
            new_scrollback_ref: Some(ScrollbackRef {
                output_segments_seq: 10,
                total_lines_captured: 200,
                last_capture_at: 5000,
            }),
        };
        let j1 = serde_json::to_string(&d1).unwrap();
        let r1: SnapshotDiff = serde_json::from_str(&j1).unwrap();
        assert_eq!(d1, r1);

        // PaneMetadataChanged
        let d2 = SnapshotDiff::PaneMetadataChanged {
            pane_id: 2,
            new_state: make_pane_state(2, 30, 120),
        };
        let j2 = serde_json::to_string(&d2).unwrap();
        let r2: SnapshotDiff = serde_json::from_str(&j2).unwrap();
        assert_eq!(d2, r2);

        // PaneCreated
        let d3 = SnapshotDiff::PaneCreated {
            pane_id: 3,
            snapshot: make_pane_state(3, 24, 80),
        };
        let j3 = serde_json::to_string(&d3).unwrap();
        let r3: SnapshotDiff = serde_json::from_str(&j3).unwrap();
        assert_eq!(d3, r3);

        // PaneClosed
        let d4 = SnapshotDiff::PaneClosed { pane_id: 4 };
        let j4 = serde_json::to_string(&d4).unwrap();
        let r4: SnapshotDiff = serde_json::from_str(&j4).unwrap();
        assert_eq!(d4, r4);

        // LayoutChanged
        let d5 = SnapshotDiff::LayoutChanged {
            new_topology: make_topology(&[1, 2]),
        };
        let j5 = serde_json::to_string(&d5).unwrap();
        let r5: SnapshotDiff = serde_json::from_str(&j5).unwrap();
        assert_eq!(d5, r5);
    }

    #[test]
    fn base_snapshot_serde_roundtrip() {
        let base = make_base(&[1, 2, 3]);
        let json = serde_json::to_string(&base).unwrap();
        let restored: BaseSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(base, restored);
    }

    #[test]
    fn base_snapshot_new_empty_panes() {
        let base = BaseSnapshot::new(500, make_topology(&[]), vec![]);
        assert_eq!(base.pane_states.len(), 0);
        assert_eq!(base.captured_at, 500);
        assert!(base.topology.windows[0].tabs.is_empty());
    }

    #[test]
    fn apply_diff_scrollback_nonexistent_pane_is_noop() {
        let mut base = make_base(&[1]);
        let original = base.clone();

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneScrollbackChanged {
                pane_id: 999, // does not exist
                new_scrollback_ref: Some(ScrollbackRef {
                    output_segments_seq: 50,
                    total_lines_captured: 100,
                    last_capture_at: 2000,
                }),
            }],
        };

        base.apply_diff(&diff);
        // Pane map unchanged
        assert_eq!(base.pane_states.len(), original.pane_states.len());
        assert_eq!(
            base.pane_states[&1].scrollback_ref,
            original.pane_states[&1].scrollback_ref
        );
        // But captured_at is updated
        assert_eq!(base.captured_at, 2000);
    }

    #[test]
    fn apply_diff_scrollback_set_to_none() {
        let mut base = make_base(&[1]);
        // First set a scrollback ref
        base.pane_states.get_mut(&1).unwrap().scrollback_ref = Some(ScrollbackRef {
            output_segments_seq: 10,
            total_lines_captured: 100,
            last_capture_at: 1500,
        });

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneScrollbackChanged {
                pane_id: 1,
                new_scrollback_ref: None,
            }],
        };

        base.apply_diff(&diff);
        assert!(base.pane_states[&1].scrollback_ref.is_none());
    }

    #[test]
    fn apply_diff_multiple_records_in_single_snapshot() {
        let mut base = make_base(&[1, 2, 3]);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![
                SnapshotDiff::PaneClosed { pane_id: 1 },
                SnapshotDiff::PaneCreated {
                    pane_id: 10,
                    snapshot: make_pane_state(10, 40, 160),
                },
                SnapshotDiff::PaneMetadataChanged {
                    pane_id: 2,
                    new_state: {
                        let mut ps = make_pane_state(2, 50, 200);
                        ps.cwd = Some("/updated".to_string());
                        ps
                    },
                },
                SnapshotDiff::LayoutChanged {
                    new_topology: make_topology(&[2, 3, 10]),
                },
            ],
        };

        base.apply_diff(&diff);
        assert!(!base.pane_states.contains_key(&1));
        assert!(base.pane_states.contains_key(&10));
        assert_eq!(base.pane_states[&2].cwd, Some("/updated".to_string()));
        assert_eq!(base.topology.windows[0].tabs.len(), 3);
        assert_eq!(base.captured_at, 2000);
    }

    #[test]
    fn diff_chain_serde_roundtrip() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base);
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        let json = serde_json::to_string(&chain).unwrap();
        let restored: DiffChain = serde_json::from_str(&json).unwrap();

        // Verify restored chain yields same state
        let orig_state = chain.restore_latest();
        let rest_state = restored.restore_latest();
        assert_eq!(orig_state, rest_state);
    }

    #[test]
    fn diff_chain_chain_len_tracks_pushes() {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);
        assert_eq!(chain.chain_len(), 0);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![],
        });
        assert_eq!(chain.chain_len(), 1);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![],
        });
        assert_eq!(chain.chain_len(), 2);
    }

    #[test]
    fn engine_chain_len_zero_when_not_initialized() {
        let engine = DiffSnapshotEngine::new(10);
        assert_eq!(engine.chain_len(), 0);
    }

    #[test]
    fn engine_compact_returns_none_when_not_initialized() {
        let mut engine = DiffSnapshotEngine::new(10);
        assert!(engine.compact().is_none());
    }

    #[test]
    fn engine_layout_dirty_but_no_topology_provided() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1]));

        engine.tracker_mut().mark_layout_dirty();

        // Layout is dirty, but we pass None for topology
        let diff = engine.capture_diff(&HashMap::new(), None, 2000);
        // No diffs produced because no topology was supplied and no panes dirty
        assert!(diff.is_none());
        // Tracker should be cleared
        assert!(engine.tracker().is_clean());
    }

    #[test]
    fn engine_scrollback_only_dirty_emits_scrollback_diff() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1, 2]));

        // Mark pane 1 scrollback only (not metadata)
        engine.tracker_mut().mark_output(1);

        let mut current = HashMap::new();
        let mut ps = make_pane_state(1, 24, 80);
        ps.scrollback_ref = Some(ScrollbackRef {
            output_segments_seq: 99,
            total_lines_captured: 1000,
            last_capture_at: 2000,
        });
        current.insert(1, ps);

        let diff = engine.capture_diff(&current, None, 2000).unwrap();
        assert_eq!(diff.diffs.len(), 1);
        match &diff.diffs[0] {
            SnapshotDiff::PaneScrollbackChanged {
                pane_id,
                new_scrollback_ref,
            } => {
                assert_eq!(*pane_id, 1);
                let sb = new_scrollback_ref.as_ref().unwrap();
                assert_eq!(sb.output_segments_seq, 99);
                assert_eq!(sb.total_lines_captured, 1000);
            }
            other => panic!("expected PaneScrollbackChanged, got {:?}", other),
        }
    }

    #[test]
    fn engine_reinitialize_replaces_chain() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1, 2, 3]));

        // Make some changes
        engine.tracker_mut().mark_metadata(1);
        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));
        engine.capture_diff(&current, None, 2000);
        assert_eq!(engine.chain_len(), 1);

        // Re-initialize with different base
        engine.initialize(make_base(&[10, 20]));
        assert_eq!(engine.chain_len(), 0);
        let restored = engine.restore_latest().unwrap();
        assert_eq!(restored.pane_states.len(), 2);
        assert!(restored.pane_states.contains_key(&10));
        assert!(restored.pane_states.contains_key(&20));
        assert!(!restored.pane_states.contains_key(&1));
    }

    #[test]
    fn engine_dirty_pane_not_in_current_panes_is_skipped() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1, 2]));

        // Mark pane 1 as dirty but don't include it in current_panes
        engine.tracker_mut().mark_metadata(1);

        let current = HashMap::new(); // empty!
        let diff = engine.capture_diff(&current, None, 2000);
        // No diffs because the dirty pane is not found in current_panes
        assert!(diff.is_none());
        assert!(engine.tracker().is_clean());
    }

    #[test]
    fn engine_metadata_takes_priority_over_scrollback() {
        // When a pane has both metadata and scrollback dirty, both diffs are emitted
        // (metadata first, then scrollback) since capture_diff uses independent checks.
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1]));

        engine.tracker_mut().mark_output(1);
        engine.tracker_mut().mark_metadata(1);

        let mut current = HashMap::new();
        let mut ps = make_pane_state(1, 30, 120);
        ps.cwd = Some("/both-dirty".to_string());
        current.insert(1, ps);

        let diff = engine.capture_diff(&current, None, 2000).unwrap();
        // Both dirty fields are emitted: metadata first, then scrollback.
        assert_eq!(diff.diffs.len(), 2);
        assert!(matches!(
            &diff.diffs[0],
            SnapshotDiff::PaneMetadataChanged { pane_id: 1, .. }
        ));
        assert!(matches!(
            &diff.diffs[1],
            SnapshotDiff::PaneScrollbackChanged { pane_id: 1, .. }
        ));
    }

    #[test]
    fn chain_restore_at_base_preserves_original_state() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base.clone());

        // Add diffs that modify state
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        // restore_at(0) should yield original base
        let at_base = chain.restore_at(0).unwrap();
        assert_eq!(at_base.pane_states.len(), 2);
        assert_eq!(at_base.captured_at, base.captured_at);
        assert!(at_base.pane_states.contains_key(&1));
    }

    // ── Telemetry counter tests ──────────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let engine = DiffSnapshotEngine::new(10);
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.diffs_captured, 0);
        assert_eq!(snap.clean_skips, 0);
        assert_eq!(snap.auto_compactions, 0);
        assert_eq!(snap.manual_compactions, 0);
        assert_eq!(snap.total_diff_entries, 0);
        assert_eq!(snap.layout_diffs, 0);
    }

    #[test]
    fn telemetry_clean_skip_counted() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1, 2]));
        // No dirty panes → clean skip
        let diff = engine.capture_diff(&HashMap::new(), None, 2000);
        assert!(diff.is_none());
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.clean_skips, 1);
        assert_eq!(snap.diffs_captured, 0);
    }

    #[test]
    fn telemetry_diff_captured_counted() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1, 2]));
        engine.tracker_mut().mark_metadata(1);
        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));
        current.insert(2, make_pane_state(2, 24, 80));
        let diff = engine.capture_diff(&current, None, 2000);
        assert!(diff.is_some());
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.diffs_captured, 1);
        assert!(snap.total_diff_entries >= 1);
    }

    #[test]
    fn telemetry_manual_compaction_counted() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1]));
        // Create a diff so chain exists
        engine.tracker_mut().mark_metadata(1);
        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));
        let _ = engine.capture_diff(&current, None, 2000);
        engine.compact();
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.manual_compactions, 1);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&[1]));
        engine.tracker_mut().mark_metadata(1);
        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));
        let _ = engine.capture_diff(&current, None, 2000);
        let snap = engine.telemetry().snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: DiffSnapshotTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- proptest ----

    #[cfg(test)]
    mod prop {
        use super::*;
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum PaneAction {
            Create(u64),
            Close(u64),
            ModifyMetadata(u64),
            ModifyScrollback(u64),
        }

        fn arb_action(max_pane_id: u64) -> impl Strategy<Value = PaneAction> {
            let id_range = 1..=max_pane_id;
            prop_oneof![
                id_range.clone().prop_map(PaneAction::Create),
                id_range.clone().prop_map(PaneAction::Close),
                id_range.clone().prop_map(PaneAction::ModifyMetadata),
                id_range.prop_map(PaneAction::ModifyScrollback),
            ]
        }

        fn arb_action_sequence(
            len: usize,
            max_pane_id: u64,
        ) -> impl Strategy<Value = Vec<PaneAction>> {
            proptest::collection::vec(arb_action(max_pane_id), 1..=len)
        }

        proptest! {
            /// After any sequence of actions + snapshot + restore, the restored
            /// state matches the live "current" state.
            #[test]
            fn restore_matches_live_state(
                actions in arb_action_sequence(20, 10)
            ) {
                let initial_ids: Vec<u64> = vec![1, 2, 3];
                let mut engine = DiffSnapshotEngine::new(0);
                engine.initialize(make_base(&initial_ids));

                let mut live_panes: HashMap<u64, PaneStateSnapshot> = initial_ids
                    .iter()
                    .map(|&id| (id, make_pane_state(id, 24, 80)))
                    .collect();

                let mut time = 2000u64;

                for action in &actions {
                    match action {
                        PaneAction::Create(id) => {
                            if !live_panes.contains_key(id) {
                                let ps = make_pane_state(*id, 24, 80);
                                live_panes.insert(*id, ps);
                                engine.tracker_mut().mark_created(*id);
                            }
                        }
                        PaneAction::Close(id) => {
                            if live_panes.contains_key(id) {
                                live_panes.remove(id);
                                engine.tracker_mut().mark_closed(*id);
                            }
                        }
                        PaneAction::ModifyMetadata(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.cwd = Some(format!("/modified/{time}"));
                                engine.tracker_mut().mark_metadata(*id);
                            }
                        }
                        PaneAction::ModifyScrollback(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.scrollback_ref = Some(ScrollbackRef {
                                    output_segments_seq: time as i64,
                                    total_lines_captured: time,
                                    last_capture_at: time,
                                });
                                engine.tracker_mut().mark_output(*id);
                            }
                        }
                    }

                    time += 1000;
                    engine.capture_diff(&live_panes, None, time);
                }

                let restored = engine.restore_latest().unwrap();
                // Same set of pane IDs
                let live_ids: HashSet<u64> = live_panes.keys().copied().collect();
                let restored_ids: HashSet<u64> = restored.pane_states.keys().copied().collect();
                prop_assert_eq!(live_ids, restored_ids);

                // Each pane state matches
                for (id, live_state) in &live_panes {
                    let restored_state = restored.pane_states.get(id).unwrap();
                    prop_assert_eq!(live_state.cwd.as_deref(), restored_state.cwd.as_deref());
                    prop_assert_eq!(&live_state.scrollback_ref, &restored_state.scrollback_ref);
                }
            }

            /// Compaction preserves final state.
            #[test]
            fn compaction_preserves_state(
                actions in arb_action_sequence(15, 8)
            ) {
                let initial_ids: Vec<u64> = vec![1, 2, 3];
                let mut engine = DiffSnapshotEngine::new(0);
                engine.initialize(make_base(&initial_ids));

                let mut live_panes: HashMap<u64, PaneStateSnapshot> = initial_ids
                    .iter()
                    .map(|&id| (id, make_pane_state(id, 24, 80)))
                    .collect();

                let mut time = 2000u64;

                for action in &actions {
                    match action {
                        PaneAction::Create(id) => {
                            if !live_panes.contains_key(id) {
                                live_panes.insert(*id, make_pane_state(*id, 24, 80));
                                engine.tracker_mut().mark_created(*id);
                            }
                        }
                        PaneAction::Close(id) => {
                            if live_panes.contains_key(id) {
                                live_panes.remove(id);
                                engine.tracker_mut().mark_closed(*id);
                            }
                        }
                        PaneAction::ModifyMetadata(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.cwd = Some(format!("/m/{time}"));
                                engine.tracker_mut().mark_metadata(*id);
                            }
                        }
                        PaneAction::ModifyScrollback(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.scrollback_ref = Some(ScrollbackRef {
                                    output_segments_seq: time as i64,
                                    total_lines_captured: time,
                                    last_capture_at: time,
                                });
                                engine.tracker_mut().mark_output(*id);
                            }
                        }
                    }
                    time += 1000;
                    engine.capture_diff(&live_panes, None, time);
                }

                let before = engine.restore_latest().unwrap();
                engine.compact();
                let after = engine.restore_latest().unwrap();

                let before_ids: HashSet<u64> = before.pane_states.keys().copied().collect();
                let after_ids: HashSet<u64> = after.pane_states.keys().copied().collect();
                prop_assert_eq!(before_ids, after_ids);

                for (id, before_state) in &before.pane_states {
                    let after_state = after.pane_states.get(id).unwrap();
                    prop_assert_eq!(before_state, after_state);
                }
            }

            /// Dirty tracker always reports accurate dirty count.
            #[test]
            fn dirty_count_matches_dirty_set(
                actions in arb_action_sequence(30, 20)
            ) {
                let mut tracker = DirtyTracker::new();

                for action in &actions {
                    match action {
                        PaneAction::Create(id) => tracker.mark_created(*id),
                        PaneAction::Close(id) => tracker.mark_closed(*id),
                        PaneAction::ModifyMetadata(id) => tracker.mark_metadata(*id),
                        PaneAction::ModifyScrollback(id) => tracker.mark_output(*id),
                    }
                }

                prop_assert_eq!(tracker.dirty_count(), tracker.dirty_pane_ids().len());
                prop_assert_eq!(tracker.is_clean(), tracker.dirty_count() == 0);
            }
        }
    }
}
