#![allow(clippy::cast_precision_loss)]
#![allow(clippy::module_name_repetitions)]

//! Snapshot divergence bisect substrate.
//!
//! This module answers the operator question "what is the first snapshot where
//! the invariant stopped holding?" for total-order checkpoint streams and
//! partial-order snapshot DAGs. It stays independent from storage and replay IO:
//! callers pass already-loaded snapshot evidence, predicates, and receive a
//! compact report with replay/diff commands and search-decision logs.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Evidence for one pane at a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshotEvidence {
    /// Pane id.
    pub pane_id: u64,
    /// Redacted pane text or tail text.
    pub text: String,
}

/// Event evidence at a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSnapshotEvidence {
    /// Event kind or rule id.
    pub kind: String,
    /// Count observed up to this snapshot.
    pub count: usize,
}

/// Workflow evidence at a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSnapshotEvidence {
    /// Workflow execution id.
    pub workflow_id: String,
    /// Normalized workflow state.
    pub state: String,
}

/// Policy decision evidence at a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionSnapshotEvidence {
    /// Stable policy decision id or rule key.
    pub decision_id: String,
    /// Normalized decision, for example allow, deny, or require_approval.
    pub decision: String,
}

/// Storage health severity used by divergence predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageHealthStatus {
    /// Healthy.
    Healthy,
    /// Degraded but usable.
    Degraded,
    /// Corrupt or unavailable.
    Corrupt,
}

/// Storage health evidence at a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageHealthSnapshotEvidence {
    /// Health status.
    pub status: StorageHealthStatus,
    /// Redacted detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One snapshot/checkpoint worth of divergence evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergenceSnapshot {
    /// Stable snapshot id.
    pub snapshot_id: String,
    /// Monotonic order key for total-order bisection.
    pub order_key: u64,
    /// Parent ids for partial-order search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_ids: Vec<String>,
    /// Pane text evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneSnapshotEvidence>,
    /// Event-stream evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventSnapshotEvidence>,
    /// Workflow evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<WorkflowSnapshotEvidence>,
    /// Policy decision evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_decisions: Vec<PolicyDecisionSnapshotEvidence>,
    /// Storage health evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_health: Option<StorageHealthSnapshotEvidence>,
}

impl DivergenceSnapshot {
    /// Create a minimal snapshot with an id and order key.
    #[must_use]
    pub fn new(snapshot_id: impl Into<String>, order_key: u64) -> Self {
        Self {
            snapshot_id: snapshot_id.into(),
            order_key,
            parent_ids: Vec::new(),
            panes: Vec::new(),
            events: Vec::new(),
            workflows: Vec::new(),
            policy_decisions: Vec::new(),
            storage_health: None,
        }
    }

    /// Attach parent ids for DAG search.
    #[must_use]
    pub fn with_parents(mut self, parent_ids: Vec<String>) -> Self {
        self.parent_ids = parent_ids;
        self
    }

    /// Attach pane text evidence.
    #[must_use]
    pub fn with_pane_text(mut self, pane_id: u64, text: impl Into<String>) -> Self {
        self.panes.push(PaneSnapshotEvidence {
            pane_id,
            text: text.into(),
        });
        self
    }

    /// Attach event count evidence.
    #[must_use]
    pub fn with_event_count(mut self, kind: impl Into<String>, count: usize) -> Self {
        self.events.push(EventSnapshotEvidence {
            kind: kind.into(),
            count,
        });
        self
    }

    /// Attach workflow state evidence.
    #[must_use]
    pub fn with_workflow_state(
        mut self,
        workflow_id: impl Into<String>,
        state: impl Into<String>,
    ) -> Self {
        self.workflows.push(WorkflowSnapshotEvidence {
            workflow_id: workflow_id.into(),
            state: state.into(),
        });
        self
    }

    /// Attach policy decision evidence.
    #[must_use]
    pub fn with_policy_decision(
        mut self,
        decision_id: impl Into<String>,
        decision: impl Into<String>,
    ) -> Self {
        self.policy_decisions.push(PolicyDecisionSnapshotEvidence {
            decision_id: decision_id.into(),
            decision: decision.into(),
        });
        self
    }

    /// Attach storage health evidence.
    #[must_use]
    pub fn with_storage_health(
        mut self,
        status: StorageHealthStatus,
        detail: Option<String>,
    ) -> Self {
        self.storage_health = Some(StorageHealthSnapshotEvidence { status, detail });
        self
    }
}

/// Invariant predicate evaluated against snapshot evidence. A snapshot is good
/// when the predicate evaluates to pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SnapshotInvariantPredicate {
    /// Pane text must contain a string.
    PaneTextContains { pane_id: u64, needle: String },
    /// Pane text must not contain a string.
    PaneTextNotContains { pane_id: u64, needle: String },
    /// Event count must be at least the threshold.
    EventCountAtLeast {
        event_kind: String,
        min_count: usize,
    },
    /// Workflow must be in a specific state.
    WorkflowStateIs {
        workflow_id: String,
        expected_state: String,
    },
    /// Policy decision must match.
    PolicyDecisionIs {
        decision_id: String,
        expected_decision: String,
    },
    /// Storage health must be no worse than a status threshold.
    StorageHealthAtMost { max_status: StorageHealthStatus },
    /// Every child predicate must pass.
    All {
        predicates: Vec<SnapshotInvariantPredicate>,
    },
    /// At least one child predicate must pass.
    Any {
        predicates: Vec<SnapshotInvariantPredicate>,
    },
}

/// Predicate evaluation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateState {
    /// Invariant holds.
    Pass,
    /// Invariant is violated.
    Fail,
    /// Evidence is missing or incomplete.
    Unknown,
}

/// Predicate evaluation with compact evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateEvaluation {
    /// Evaluation state.
    pub state: PredicateState,
    /// Short evidence string for reports.
    pub evidence: String,
}

impl PredicateEvaluation {
    #[must_use]
    fn pass(evidence: impl Into<String>) -> Self {
        Self {
            state: PredicateState::Pass,
            evidence: evidence.into(),
        }
    }

    #[must_use]
    fn fail(evidence: impl Into<String>) -> Self {
        Self {
            state: PredicateState::Fail,
            evidence: evidence.into(),
        }
    }

    #[must_use]
    fn unknown(evidence: impl Into<String>) -> Self {
        Self {
            state: PredicateState::Unknown,
            evidence: evidence.into(),
        }
    }

    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.state == PredicateState::Pass
    }

    #[must_use]
    pub fn is_fail(&self) -> bool {
        self.state == PredicateState::Fail
    }
}

impl SnapshotInvariantPredicate {
    /// Evaluate this predicate for a single snapshot.
    #[must_use]
    pub fn evaluate(&self, snapshot: &DivergenceSnapshot) -> PredicateEvaluation {
        match self {
            Self::PaneTextContains { pane_id, needle } => match pane_text(snapshot, *pane_id) {
                Some(text) if text.contains(needle) => {
                    PredicateEvaluation::pass(format!("pane {pane_id} contains `{needle}`"))
                }
                Some(_) => PredicateEvaluation::fail(format!("pane {pane_id} lacks `{needle}`")),
                None => PredicateEvaluation::unknown(format!("pane {pane_id} missing")),
            },
            Self::PaneTextNotContains { pane_id, needle } => match pane_text(snapshot, *pane_id) {
                Some(text) if text.contains(needle) => {
                    PredicateEvaluation::fail(format!("pane {pane_id} contains `{needle}`"))
                }
                Some(_) => PredicateEvaluation::pass(format!("pane {pane_id} excludes `{needle}`")),
                None => PredicateEvaluation::unknown(format!("pane {pane_id} missing")),
            },
            Self::EventCountAtLeast {
                event_kind,
                min_count,
            } => match event_count(snapshot, event_kind) {
                Some(count) if count >= *min_count => PredicateEvaluation::pass(format!(
                    "event `{event_kind}` count {count} >= {min_count}"
                )),
                Some(count) => PredicateEvaluation::fail(format!(
                    "event `{event_kind}` count {count} < {min_count}"
                )),
                None => PredicateEvaluation::unknown(format!("event `{event_kind}` missing")),
            },
            Self::WorkflowStateIs {
                workflow_id,
                expected_state,
            } => match workflow_state(snapshot, workflow_id) {
                Some(state) if state == expected_state => PredicateEvaluation::pass(format!(
                    "workflow `{workflow_id}` state `{expected_state}`"
                )),
                Some(state) => PredicateEvaluation::fail(format!(
                    "workflow `{workflow_id}` state `{state}`, expected `{expected_state}`"
                )),
                None => PredicateEvaluation::unknown(format!("workflow `{workflow_id}` missing")),
            },
            Self::PolicyDecisionIs {
                decision_id,
                expected_decision,
            } => match policy_decision(snapshot, decision_id) {
                Some(decision) if decision == expected_decision => PredicateEvaluation::pass(
                    format!("policy `{decision_id}` decision `{expected_decision}`"),
                ),
                Some(decision) => PredicateEvaluation::fail(format!(
                    "policy `{decision_id}` decision `{decision}`, expected `{expected_decision}`"
                )),
                None => PredicateEvaluation::unknown(format!("policy `{decision_id}` missing")),
            },
            Self::StorageHealthAtMost { max_status } => match &snapshot.storage_health {
                Some(health) if health.status <= *max_status => {
                    PredicateEvaluation::pass(format!("storage health {:?}", health.status))
                }
                Some(health) => PredicateEvaluation::fail(format!(
                    "storage health {:?} worse than {:?}",
                    health.status, max_status
                )),
                None => PredicateEvaluation::unknown("storage health missing"),
            },
            Self::All { predicates } => evaluate_all(predicates, snapshot),
            Self::Any { predicates } => evaluate_any(predicates, snapshot),
        }
    }
}

/// Search strategy used for a divergence report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceSearchStrategy {
    /// Binary search over a total order.
    TotalOrderBisect,
    /// DAG search over parent links.
    PartialOrderDag,
}

/// One logged search decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergenceSearchLog {
    /// Strategy used for this step.
    pub strategy: DivergenceSearchStrategy,
    /// Snapshot evaluated.
    pub snapshot_id: String,
    /// Position in the sorted total order or DAG order.
    pub index: usize,
    /// Predicate state.
    pub state: PredicateState,
    /// Evidence string from predicate evaluation.
    pub evidence: String,
    /// Operator-readable decision reason.
    pub decision: String,
}

/// Divergence result kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DivergenceOutcome {
    /// No bad snapshot was proven.
    NoDivergence { last_checked_snapshot: String },
    /// First bad transition was found.
    FirstBadTransition {
        before_snapshot: Option<String>,
        after_snapshot: String,
    },
    /// Missing or unknown evidence leaves a bounded suspect interval.
    SuspectInterval {
        start_snapshot: Option<String>,
        end_snapshot: Option<String>,
        reason: String,
    },
}

/// Compact divergence report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDivergenceReport {
    /// Strategy used.
    pub strategy: DivergenceSearchStrategy,
    /// Predicate evaluated.
    pub predicate: SnapshotInvariantPredicate,
    /// Search outcome.
    pub outcome: DivergenceOutcome,
    /// Search logs, including skipped/unknown snapshots.
    pub logs: Vec<DivergenceSearchLog>,
    /// Snapshot ids skipped because evidence was unknown or parent data was missing.
    pub skipped_snapshot_ids: Vec<String>,
    /// Suggested command to inspect or replay the transition.
    pub replay_command: String,
}

/// Pure snapshot divergence bisector.
#[derive(Debug, Clone, Default)]
pub struct SnapshotDivergenceBisector;

impl SnapshotDivergenceBisector {
    /// Create a new bisector.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Binary-search a total-order snapshot stream for the first failing
    /// snapshot. If unknown evidence blocks the proof, returns the smallest
    /// suspect interval proven by the available evaluations.
    #[must_use]
    pub fn bisect_total_order(
        &self,
        snapshots: &[DivergenceSnapshot],
        predicate: SnapshotInvariantPredicate,
    ) -> SnapshotDivergenceReport {
        let mut ordered = snapshots.to_vec();
        ordered.sort_by_key(|snapshot| (snapshot.order_key, snapshot.snapshot_id.clone()));
        let mut logs = Vec::new();
        let mut skipped = Vec::new();

        if ordered.is_empty() {
            return report(
                DivergenceSearchStrategy::TotalOrderBisect,
                predicate,
                DivergenceOutcome::SuspectInterval {
                    start_snapshot: None,
                    end_snapshot: None,
                    reason: "no snapshots provided".to_string(),
                },
                logs,
                skipped,
            );
        }

        let first = evaluate_logged(
            DivergenceSearchStrategy::TotalOrderBisect,
            &predicate,
            &ordered,
            0,
            &mut logs,
            "establish lower bound",
        );
        if first.is_fail() {
            return report(
                DivergenceSearchStrategy::TotalOrderBisect,
                predicate,
                DivergenceOutcome::FirstBadTransition {
                    before_snapshot: None,
                    after_snapshot: ordered[0].snapshot_id.clone(),
                },
                logs,
                skipped,
            );
        }
        if first.state == PredicateState::Unknown {
            skipped.push(ordered[0].snapshot_id.clone());
        }

        let last_idx = ordered.len() - 1;
        let last = evaluate_logged(
            DivergenceSearchStrategy::TotalOrderBisect,
            &predicate,
            &ordered,
            last_idx,
            &mut logs,
            "establish upper bound",
        );
        if last.is_pass() {
            return report(
                DivergenceSearchStrategy::TotalOrderBisect,
                predicate,
                DivergenceOutcome::NoDivergence {
                    last_checked_snapshot: ordered[last_idx].snapshot_id.clone(),
                },
                logs,
                skipped,
            );
        }
        if last.state == PredicateState::Unknown {
            skipped.push(ordered[last_idx].snapshot_id.clone());
        }

        let mut lo = 0usize;
        let mut hi = last_idx;
        let mut blocked_unknown: Option<usize> = None;
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            let eval = evaluate_logged(
                DivergenceSearchStrategy::TotalOrderBisect,
                &predicate,
                &ordered,
                mid,
                &mut logs,
                "bisect midpoint",
            );
            match eval.state {
                PredicateState::Pass => lo = mid,
                PredicateState::Fail => hi = mid,
                PredicateState::Unknown => {
                    skipped.push(ordered[mid].snapshot_id.clone());
                    blocked_unknown = Some(mid);
                    break;
                }
            }
        }

        if let Some(mid) = blocked_unknown {
            return report(
                DivergenceSearchStrategy::TotalOrderBisect,
                predicate,
                DivergenceOutcome::SuspectInterval {
                    start_snapshot: Some(ordered[lo].snapshot_id.clone()),
                    end_snapshot: Some(ordered[hi].snapshot_id.clone()),
                    reason: format!(
                        "snapshot {} had unknown predicate evidence",
                        ordered[mid].snapshot_id
                    ),
                },
                logs,
                skipped,
            );
        }

        report(
            DivergenceSearchStrategy::TotalOrderBisect,
            predicate,
            DivergenceOutcome::FirstBadTransition {
                before_snapshot: Some(ordered[lo].snapshot_id.clone()),
                after_snapshot: ordered[hi].snapshot_id.clone(),
            },
            logs,
            skipped,
        )
    }

    /// Search a partial-order snapshot DAG for a failing node whose parent was
    /// still passing. Missing parent or unknown evidence yields a bounded
    /// suspect interval instead of a fabricated first-bad transition.
    #[must_use]
    pub fn search_partial_order(
        &self,
        snapshots: &[DivergenceSnapshot],
        predicate: SnapshotInvariantPredicate,
    ) -> SnapshotDivergenceReport {
        let mut ordered = snapshots.to_vec();
        ordered.sort_by_key(|snapshot| (snapshot.order_key, snapshot.snapshot_id.clone()));
        let index_by_id: BTreeMap<String, usize> = ordered
            .iter()
            .enumerate()
            .map(|(idx, snapshot)| (snapshot.snapshot_id.clone(), idx))
            .collect();
        let mut states = BTreeMap::new();
        let mut logs = Vec::new();
        let mut skipped = Vec::new();

        if ordered.is_empty() {
            return report(
                DivergenceSearchStrategy::PartialOrderDag,
                predicate,
                DivergenceOutcome::SuspectInterval {
                    start_snapshot: None,
                    end_snapshot: None,
                    reason: "no snapshots provided".to_string(),
                },
                logs,
                skipped,
            );
        }

        for idx in 0..ordered.len() {
            let snapshot = &ordered[idx];
            let eval = evaluate_logged(
                DivergenceSearchStrategy::PartialOrderDag,
                &predicate,
                &ordered,
                idx,
                &mut logs,
                "visit DAG node",
            );
            states.insert(snapshot.snapshot_id.clone(), eval.state);

            if eval.state == PredicateState::Unknown {
                skipped.push(snapshot.snapshot_id.clone());
                continue;
            }
            if eval.state == PredicateState::Pass {
                continue;
            }

            match parent_boundary(snapshot, &states, &index_by_id, &ordered, &mut skipped) {
                ParentBoundary::Known { before_snapshot } => {
                    return report(
                        DivergenceSearchStrategy::PartialOrderDag,
                        predicate,
                        DivergenceOutcome::FirstBadTransition {
                            before_snapshot,
                            after_snapshot: snapshot.snapshot_id.clone(),
                        },
                        logs,
                        skipped,
                    );
                }
                ParentBoundary::Unknown { reason, start } => {
                    return report(
                        DivergenceSearchStrategy::PartialOrderDag,
                        predicate,
                        DivergenceOutcome::SuspectInterval {
                            start_snapshot: start,
                            end_snapshot: Some(snapshot.snapshot_id.clone()),
                            reason,
                        },
                        logs,
                        skipped,
                    );
                }
                ParentBoundary::NoPassingParent => {}
            }
        }

        report(
            DivergenceSearchStrategy::PartialOrderDag,
            predicate,
            DivergenceOutcome::NoDivergence {
                last_checked_snapshot: ordered
                    .last()
                    .map(|snapshot| snapshot.snapshot_id.clone())
                    .unwrap_or_default(),
            },
            logs,
            skipped,
        )
    }
}

enum ParentBoundary {
    Known {
        before_snapshot: Option<String>,
    },
    Unknown {
        reason: String,
        start: Option<String>,
    },
    NoPassingParent,
}

fn parent_boundary(
    snapshot: &DivergenceSnapshot,
    states: &BTreeMap<String, PredicateState>,
    index_by_id: &BTreeMap<String, usize>,
    ordered: &[DivergenceSnapshot],
    skipped: &mut Vec<String>,
) -> ParentBoundary {
    if snapshot.parent_ids.is_empty() {
        return ParentBoundary::Known {
            before_snapshot: None,
        };
    }

    let mut failing_parents = BTreeSet::new();
    for parent_id in &snapshot.parent_ids {
        let Some(parent_idx) = index_by_id.get(parent_id) else {
            skipped.push(parent_id.clone());
            return ParentBoundary::Unknown {
                reason: format!("parent snapshot `{parent_id}` missing"),
                start: None,
            };
        };
        match states.get(parent_id).copied() {
            Some(PredicateState::Pass) => {
                return ParentBoundary::Known {
                    before_snapshot: Some(parent_id.clone()),
                };
            }
            Some(PredicateState::Unknown) | None => {
                skipped.push(parent_id.clone());
                return ParentBoundary::Unknown {
                    reason: format!("parent snapshot `{parent_id}` has unknown predicate state"),
                    start: Some(parent_id.clone()),
                };
            }
            Some(PredicateState::Fail) => {
                failing_parents.insert(ordered[*parent_idx].snapshot_id.clone());
            }
        }
    }

    if let Some(parent_id) = failing_parents.iter().next() {
        ParentBoundary::Unknown {
            reason: "all known parents already fail; earliest transition lies upstream".to_string(),
            start: Some(parent_id.clone()),
        }
    } else {
        ParentBoundary::NoPassingParent
    }
}

fn evaluate_logged(
    strategy: DivergenceSearchStrategy,
    predicate: &SnapshotInvariantPredicate,
    ordered: &[DivergenceSnapshot],
    idx: usize,
    logs: &mut Vec<DivergenceSearchLog>,
    decision: &str,
) -> PredicateEvaluation {
    let snapshot = &ordered[idx];
    let eval = predicate.evaluate(snapshot);
    logs.push(DivergenceSearchLog {
        strategy,
        snapshot_id: snapshot.snapshot_id.clone(),
        index: idx,
        state: eval.state,
        evidence: eval.evidence.clone(),
        decision: decision.to_string(),
    });
    eval
}

fn report(
    strategy: DivergenceSearchStrategy,
    predicate: SnapshotInvariantPredicate,
    outcome: DivergenceOutcome,
    logs: Vec<DivergenceSearchLog>,
    mut skipped_snapshot_ids: Vec<String>,
) -> SnapshotDivergenceReport {
    skipped_snapshot_ids.sort();
    skipped_snapshot_ids.dedup();
    let replay_command = replay_command_for(&outcome);
    SnapshotDivergenceReport {
        strategy,
        predicate,
        outcome,
        logs,
        skipped_snapshot_ids,
        replay_command,
    }
}

fn replay_command_for(outcome: &DivergenceOutcome) -> String {
    match outcome {
        DivergenceOutcome::NoDivergence {
            last_checked_snapshot,
        } => format!("ft snapshot inspect {last_checked_snapshot} -f json"),
        DivergenceOutcome::FirstBadTransition {
            before_snapshot: Some(before),
            after_snapshot,
        } => format!("ft snapshot diff {before} {after_snapshot} -f json"),
        DivergenceOutcome::FirstBadTransition {
            before_snapshot: None,
            after_snapshot,
        } => format!("ft snapshot inspect {after_snapshot} -f json"),
        DivergenceOutcome::SuspectInterval {
            start_snapshot: Some(start),
            end_snapshot: Some(end),
            ..
        } => format!("ft snapshot diff {start} {end} -f json"),
        DivergenceOutcome::SuspectInterval {
            start_snapshot: None,
            end_snapshot: Some(end),
            ..
        } => format!("ft snapshot inspect {end} -f json"),
        DivergenceOutcome::SuspectInterval { .. } => "ft snapshot list -f json".to_string(),
    }
}

fn evaluate_all(
    predicates: &[SnapshotInvariantPredicate],
    snapshot: &DivergenceSnapshot,
) -> PredicateEvaluation {
    if predicates.is_empty() {
        return PredicateEvaluation::pass("empty all predicate");
    }

    let mut unknown = Vec::new();
    for predicate in predicates {
        let eval = predicate.evaluate(snapshot);
        match eval.state {
            PredicateState::Pass => {}
            PredicateState::Fail => return eval,
            PredicateState::Unknown => unknown.push(eval.evidence),
        }
    }

    if unknown.is_empty() {
        PredicateEvaluation::pass("all predicates passed")
    } else {
        PredicateEvaluation::unknown(unknown.join("; "))
    }
}

fn evaluate_any(
    predicates: &[SnapshotInvariantPredicate],
    snapshot: &DivergenceSnapshot,
) -> PredicateEvaluation {
    if predicates.is_empty() {
        return PredicateEvaluation::unknown("empty any predicate");
    }

    let mut failures = Vec::new();
    let mut unknown = Vec::new();
    for predicate in predicates {
        let eval = predicate.evaluate(snapshot);
        match eval.state {
            PredicateState::Pass => return eval,
            PredicateState::Fail => failures.push(eval.evidence),
            PredicateState::Unknown => unknown.push(eval.evidence),
        }
    }

    if !unknown.is_empty() {
        PredicateEvaluation::unknown(unknown.join("; "))
    } else {
        PredicateEvaluation::fail(failures.join("; "))
    }
}

fn pane_text(snapshot: &DivergenceSnapshot, pane_id: u64) -> Option<&str> {
    snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .map(|pane| pane.text.as_str())
}

fn event_count(snapshot: &DivergenceSnapshot, event_kind: &str) -> Option<usize> {
    snapshot
        .events
        .iter()
        .find(|event| event.kind == event_kind)
        .map(|event| event.count)
}

fn workflow_state<'a>(snapshot: &'a DivergenceSnapshot, workflow_id: &str) -> Option<&'a str> {
    snapshot
        .workflows
        .iter()
        .find(|workflow| workflow.workflow_id == workflow_id)
        .map(|workflow| workflow.state.as_str())
}

fn policy_decision<'a>(snapshot: &'a DivergenceSnapshot, decision_id: &str) -> Option<&'a str> {
    snapshot
        .policy_decisions
        .iter()
        .find(|decision| decision.decision_id == decision_id)
        .map(|decision| decision.decision.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_predicate() -> SnapshotInvariantPredicate {
        SnapshotInvariantPredicate::PaneTextNotContains {
            pane_id: 7,
            needle: "FAULT".to_string(),
        }
    }

    fn snapshot(idx: u64, text: &str) -> DivergenceSnapshot {
        DivergenceSnapshot::new(format!("snap-{idx:02}"), idx).with_pane_text(7, text)
    }

    #[test]
    fn total_order_bisect_finds_known_injected_fault_transition() {
        let snapshots: Vec<DivergenceSnapshot> = (0..16)
            .map(|idx| {
                if idx < 8 {
                    snapshot(idx, "agent steady prompt")
                } else {
                    snapshot(idx, "agent steady prompt\nFAULT injected divergence")
                }
            })
            .collect();
        let report =
            SnapshotDivergenceBisector::new().bisect_total_order(&snapshots, pane_predicate());

        assert_eq!(
            report.outcome,
            DivergenceOutcome::FirstBadTransition {
                before_snapshot: Some("snap-07".to_string()),
                after_snapshot: "snap-08".to_string(),
            }
        );
        assert!(report.logs.len() >= 4, "logs={:?}", report.logs);
        assert_eq!(
            report.replay_command,
            "ft snapshot diff snap-07 snap-08 -f json"
        );
    }

    #[test]
    fn total_order_missing_snapshot_evidence_returns_suspect_interval() {
        let mut snapshots: Vec<DivergenceSnapshot> = (0..8)
            .map(|idx| {
                if idx < 5 {
                    snapshot(idx, "steady")
                } else {
                    snapshot(idx, "FAULT")
                }
            })
            .collect();
        snapshots[4].panes.clear();

        let report =
            SnapshotDivergenceBisector::new().bisect_total_order(&snapshots, pane_predicate());

        assert!(matches!(
            report.outcome,
            DivergenceOutcome::SuspectInterval { .. }
        ));
        assert_eq!(report.skipped_snapshot_ids, vec!["snap-04".to_string()]);
        assert!(
            report
                .logs
                .iter()
                .any(|log| log.state == PredicateState::Unknown)
        );
    }

    #[test]
    fn partial_order_search_finds_bad_branch_after_good_parent() {
        let root = snapshot(0, "steady");
        let good = snapshot(1, "steady").with_parents(vec!["snap-00".to_string()]);
        let bad = snapshot(2, "FAULT").with_parents(vec!["snap-00".to_string()]);
        let report = SnapshotDivergenceBisector::new()
            .search_partial_order(&[root, good, bad], pane_predicate());

        assert_eq!(
            report.outcome,
            DivergenceOutcome::FirstBadTransition {
                before_snapshot: Some("snap-00".to_string()),
                after_snapshot: "snap-02".to_string(),
            }
        );
        assert_eq!(
            report.replay_command,
            "ft snapshot diff snap-00 snap-02 -f json"
        );
    }

    #[test]
    fn partial_order_missing_parent_returns_suspect_interval() {
        let bad = snapshot(2, "FAULT").with_parents(vec!["missing-parent".to_string()]);
        let report =
            SnapshotDivergenceBisector::new().search_partial_order(&[bad], pane_predicate());

        assert!(matches!(
            report.outcome,
            DivergenceOutcome::SuspectInterval {
                end_snapshot: Some(_),
                ..
            }
        ));
        assert_eq!(
            report.skipped_snapshot_ids,
            vec!["missing-parent".to_string()]
        );
    }

    #[test]
    fn predicates_cover_events_workflows_policy_and_storage_health() {
        let snap = DivergenceSnapshot::new("snap", 1)
            .with_event_count("rate_limit", 3)
            .with_workflow_state("wf-1", "running")
            .with_policy_decision("send-1", "deny")
            .with_storage_health(
                StorageHealthStatus::Degraded,
                Some("wal backlog".to_string()),
            );

        assert!(
            SnapshotInvariantPredicate::EventCountAtLeast {
                event_kind: "rate_limit".to_string(),
                min_count: 2,
            }
            .evaluate(&snap)
            .is_pass()
        );
        assert!(
            SnapshotInvariantPredicate::WorkflowStateIs {
                workflow_id: "wf-1".to_string(),
                expected_state: "running".to_string(),
            }
            .evaluate(&snap)
            .is_pass()
        );
        assert!(
            SnapshotInvariantPredicate::PolicyDecisionIs {
                decision_id: "send-1".to_string(),
                expected_decision: "deny".to_string(),
            }
            .evaluate(&snap)
            .is_pass()
        );
        assert!(
            SnapshotInvariantPredicate::StorageHealthAtMost {
                max_status: StorageHealthStatus::Degraded,
            }
            .evaluate(&snap)
            .is_pass()
        );
        assert!(
            SnapshotInvariantPredicate::StorageHealthAtMost {
                max_status: StorageHealthStatus::Healthy,
            }
            .evaluate(&snap)
            .is_fail()
        );
    }

    #[test]
    fn composite_all_and_any_preserve_unknowns() {
        let snap = snapshot(1, "ready");
        let all = SnapshotInvariantPredicate::All {
            predicates: vec![
                SnapshotInvariantPredicate::PaneTextContains {
                    pane_id: 7,
                    needle: "ready".to_string(),
                },
                SnapshotInvariantPredicate::WorkflowStateIs {
                    workflow_id: "wf-missing".to_string(),
                    expected_state: "done".to_string(),
                },
            ],
        };
        let any = SnapshotInvariantPredicate::Any {
            predicates: vec![
                SnapshotInvariantPredicate::WorkflowStateIs {
                    workflow_id: "wf-missing".to_string(),
                    expected_state: "done".to_string(),
                },
                SnapshotInvariantPredicate::PaneTextContains {
                    pane_id: 7,
                    needle: "ready".to_string(),
                },
            ],
        };

        assert_eq!(all.evaluate(&snap).state, PredicateState::Unknown);
        assert_eq!(any.evaluate(&snap).state, PredicateState::Pass);
    }

    #[test]
    fn total_order_reports_no_divergence_when_tail_snapshot_still_passes() {
        let snapshots: Vec<DivergenceSnapshot> =
            (0..5).map(|idx| snapshot(idx, "steady")).collect();
        let report =
            SnapshotDivergenceBisector::new().bisect_total_order(&snapshots, pane_predicate());

        assert_eq!(
            report.outcome,
            DivergenceOutcome::NoDivergence {
                last_checked_snapshot: "snap-04".to_string(),
            }
        );
        assert_eq!(report.replay_command, "ft snapshot inspect snap-04 -f json");
    }
}
