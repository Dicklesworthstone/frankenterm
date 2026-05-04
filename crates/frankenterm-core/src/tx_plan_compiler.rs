//! Transaction plan DAG compiler (ft-1i2ge.8.3).
//!
//! Compiles mission assignment outputs into an executable dependency graph
//! with explicit preconditions and compensating actions.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

// ── Preconditions ───────────────────────────────────────────────────────────

/// A precondition that must hold before a step can execute.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreconditionKind {
    /// Policy preflight must pass for this action.
    PolicyApproved,
    /// File reservation must be acquired.
    ReservationHeld { paths: Vec<String> },
    /// Operator approval required.
    ApprovalRequired { approver: String },
    /// Target pane/agent must be reachable.
    TargetReachable { target_id: String },
    /// Context snapshot must be fresh (within max_age_ms).
    ContextFresh { max_age_ms: u64 },
}

/// A single precondition attached to a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Precondition {
    pub kind: PreconditionKind,
    pub description: String,
    pub required: bool,
}

// ── Compensating actions ────────────────────────────────────────────────────

/// A compensating action that can undo or mitigate a failed step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompensatingAction {
    pub step_id: String,
    pub description: String,
    pub action_type: CompensationKind,
}

/// Kind of compensation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompensationKind {
    /// Undo the action (e.g., revert a file change).
    Rollback,
    /// Notify an operator for manual intervention.
    NotifyOperator,
    /// Retry the step with modified parameters.
    RetryWithBackoff { max_retries: u32 },
    /// Skip and continue (best-effort).
    SkipAndContinue,
    /// Run an alternative action.
    Alternative { alternative_step_id: String },
}

// ── Step and DAG ────────────────────────────────────────────────────────────

/// Risk level for a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepRisk {
    Low,
    Medium,
    High,
    Critical,
}

/// A single step in the transaction plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxStep {
    pub id: String,
    pub bead_id: String,
    pub agent_id: String,
    pub description: String,
    /// Steps that must complete before this step can start.
    pub depends_on: Vec<String>,
    /// Preconditions to validate before execution.
    pub preconditions: Vec<Precondition>,
    /// Compensating actions if this step fails.
    pub compensations: Vec<CompensatingAction>,
    /// Risk level of this step.
    pub risk: StepRisk,
    /// Score from the planner.
    pub score: f64,
}

/// Result of compiling a transaction plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxPlan {
    pub plan_id: String,
    pub plan_hash: u64,
    pub steps: Vec<TxStep>,
    pub execution_order: Vec<String>,
    pub parallel_levels: Vec<Vec<String>>,
    pub risk_summary: TxRiskSummary,
    pub rejected_edges: Vec<RejectedEdge>,
    /// Planner assignments that were dropped before step construction
    /// because their `bead_id` was empty or collided with an earlier
    /// assignment in the same compile call. Preserved as forensic
    /// evidence so an operator inspecting the resulting plan can see
    /// what was lost. br-ft-6gf4j.
    #[serde(default)]
    pub rejected_assignments: Vec<RejectedAssignment>,
}

/// Risk summary for the entire plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRiskSummary {
    pub total_steps: usize,
    pub high_risk_count: usize,
    pub critical_risk_count: usize,
    pub uncompensated_steps: usize,
    pub overall_risk: StepRisk,
}

/// An edge that was considered but rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedEdge {
    pub from_step: String,
    pub to_step: String,
    pub reason: String,
}

/// br-ft-6gf4j: a planner assignment that was dropped before step
/// construction because its `bead_id` was empty or duplicated an
/// earlier assignment's bead_id in the same compile call. Without
/// this rejection the duplicate would collapse into a single TxStep
/// (since step ids are derived as `step-{bead_id}` and the
/// topological-sort adjacency is keyed by step id), corrupting
/// dedup, resume, and parallel-level semantics downstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedAssignment {
    pub bead_id: String,
    pub agent_id: String,
    pub reason: String,
}

impl RejectedAssignment {
    /// Reason text for an empty `bead_id`. Stable string so audit
    /// pipelines can pattern-match.
    pub const REASON_EMPTY_BEAD_ID: &'static str = "bead_id is empty";

    /// Reason prefix for a duplicate `bead_id`. Full reason text
    /// is `"{REASON_DUPLICATE_BEAD_ID_PREFIX}{prior_index}"` so
    /// audit consumers can correlate the duplicate against the
    /// retained entry's index in the input slice.
    pub const REASON_DUPLICATE_BEAD_ID_PREFIX: &'static str =
        "duplicate bead_id (first seen at assignment index ";
}

// ── Compiler input ──────────────────────────────────────────────────────────

/// Input for the plan compiler: an assignment from the planner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerAssignment {
    pub bead_id: String,
    pub agent_id: String,
    pub score: f64,
    pub tags: Vec<String>,
    /// Bead IDs this assignment depends on (from the beads graph).
    pub dependency_bead_ids: Vec<String>,
}

/// Configuration for the plan compiler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilerConfig {
    /// Automatically add PolicyApproved precondition to all steps.
    pub require_policy_preflight: bool,
    /// Automatically add compensation for high-risk steps.
    pub auto_compensate_high_risk: bool,
    /// Default compensation kind for auto-compensated steps.
    pub default_compensation: CompensationKind,
    /// Steps with score below this get ContextFresh precondition.
    pub context_freshness_threshold: f64,
    /// Max age in ms for context freshness check.
    pub context_freshness_max_age_ms: u64,
}

impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            require_policy_preflight: true,
            auto_compensate_high_risk: true,
            default_compensation: CompensationKind::NotifyOperator,
            context_freshness_threshold: 0.5,
            context_freshness_max_age_ms: 60_000,
        }
    }
}

// ── Compiler ────────────────────────────────────────────────────────────────

/// br-ft-6gf4j: structured error class returned by
/// [`compile_tx_plan`] when assignment input fails the
/// pre-compile validation contract. Tx plans are content-
/// addressed and resumed by step id (`step-{bead_id}`); a
/// duplicate or empty bead id collapses distinct logical
/// assignments into the same DAG node, corrupting execution
/// dedup, idempotency-resume, and parallel-level computation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TxPlanCompileError {
    /// A `PlannerAssignment` had an empty `bead_id`.
    EmptyBeadId { assignment_index: usize },
    /// Two or more `PlannerAssignment`s shared the same `bead_id`.
    /// All step ids derive from the bead id (`step-{bead_id}`),
    /// so duplicates collapse into one DAG node.
    DuplicateBeadId {
        bead_id: String,
        first_index: usize,
        duplicate_index: usize,
    },
}

impl std::fmt::Display for TxPlanCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyBeadId { assignment_index } => write!(
                f,
                "tx plan assignment at index {assignment_index} has empty bead_id"
            ),
            Self::DuplicateBeadId {
                bead_id,
                first_index,
                duplicate_index,
            } => write!(
                f,
                "tx plan assignment at index {duplicate_index} duplicates bead_id {bead_id:?} \
                 (first seen at index {first_index})"
            ),
        }
    }
}

impl std::error::Error for TxPlanCompileError {}

/// br-ft-6gf4j strict variant: validates `bead_id`
/// uniqueness + non-emptiness BEFORE compilation; returns
/// [`TxPlanCompileError`] on the first violation. Two
/// assignments with the same `bead_id` would derive the same
/// `step-{bead_id}` id and collapse into one DAG node —
/// corrupting execution dedup, idempotency-resume
/// (`tx_idempotency::pending_step_ids` keys by step id), and
/// parallel-level computation.
///
/// Callers that prefer the lenient default (track rejected
/// assignments in `TxPlan::rejected_assignments`, continue
/// compilation with the deduplicated set) call
/// [`compile_tx_plan`] directly.
pub fn compile_tx_plan_strict(
    plan_id: &str,
    assignments: &[PlannerAssignment],
    config: &CompilerConfig,
) -> Result<TxPlan, TxPlanCompileError> {
    let mut seen_bead_ids: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(assignments.len());
    for (idx, assignment) in assignments.iter().enumerate() {
        if assignment.bead_id.is_empty() {
            return Err(TxPlanCompileError::EmptyBeadId {
                assignment_index: idx,
            });
        }
        if let Some(&first_index) = seen_bead_ids.get(assignment.bead_id.as_str()) {
            return Err(TxPlanCompileError::DuplicateBeadId {
                bead_id: assignment.bead_id.clone(),
                first_index,
                duplicate_index: idx,
            });
        }
        seen_bead_ids.insert(assignment.bead_id.as_str(), idx);
    }
    Ok(compile_tx_plan(plan_id, assignments, config))
}

/// Compile planner assignments into a transaction plan
/// (lenient default).
///
/// br-ft-6gf4j: invalid bead_ids (empty / duplicate) are
/// recorded in `TxPlan::rejected_assignments` and excluded
/// from the compiled DAG. Callers that want fail-fast
/// validation use [`compile_tx_plan_strict`].
pub fn compile_tx_plan(
    plan_id: &str,
    assignments: &[PlannerAssignment],
    config: &CompilerConfig,
) -> TxPlan {
    // br-ft-6gf4j: walk assignments once to detect empty/duplicate
    // bead_ids and partition the input into accepted (kept) vs
    // rejected. The accepted set is what enters step construction
    // AND seeds the dependency lookup — a duplicate's deps must
    // not be merged into the retained entry's depends_on (that
    // would silently expand the kept step's dependency surface).
    let mut accepted_indices: Vec<usize> = Vec::with_capacity(assignments.len());
    let mut rejected_assignments: Vec<RejectedAssignment> = Vec::new();
    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    for (idx, assignment) in assignments.iter().enumerate() {
        let bead_id = assignment.bead_id.as_str();
        if bead_id.is_empty() {
            rejected_assignments.push(RejectedAssignment {
                bead_id: assignment.bead_id.clone(),
                agent_id: assignment.agent_id.clone(),
                reason: RejectedAssignment::REASON_EMPTY_BEAD_ID.to_string(),
            });
            continue;
        }
        if let Some(prior_idx) = first_seen.get(bead_id) {
            rejected_assignments.push(RejectedAssignment {
                bead_id: assignment.bead_id.clone(),
                agent_id: assignment.agent_id.clone(),
                reason: format!(
                    "{}{prior_idx})",
                    RejectedAssignment::REASON_DUPLICATE_BEAD_ID_PREFIX
                ),
            });
            continue;
        }
        first_seen.insert(bead_id, idx);
        accepted_indices.push(idx);
    }

    // Only accepted bead_ids count as in-plan for the dep-resolution
    // step-{bead_id} lookup that follows.
    let assigned_beads: HashSet<&str> = accepted_indices
        .iter()
        .map(|&idx| assignments[idx].bead_id.as_str())
        .collect();
    let mut steps = Vec::new();
    let mut rejected_edges = Vec::new();

    for &idx in &accepted_indices {
        let assignment = &assignments[idx];
        let step_id = format!("step-{}", assignment.bead_id);

        // Build dependencies: only include deps that are also in this plan.
        let mut depends_on = Vec::new();
        for dep_id in &assignment.dependency_bead_ids {
            if assigned_beads.contains(dep_id.as_str()) {
                depends_on.push(format!("step-{}", dep_id));
            } else {
                rejected_edges.push(RejectedEdge {
                    from_step: format!("step-{}", dep_id),
                    to_step: step_id.clone(),
                    reason: format!("Dependency {} not in this plan", dep_id),
                });
            }
        }

        // Determine risk from tags.
        let risk = classify_risk(&assignment.tags, assignment.score);

        // Build preconditions.
        let mut preconditions = Vec::new();
        if config.require_policy_preflight {
            preconditions.push(Precondition {
                kind: PreconditionKind::PolicyApproved,
                description: "Policy preflight must pass".to_string(),
                required: true,
            });
        }
        if assignment.score < config.context_freshness_threshold {
            preconditions.push(Precondition {
                kind: PreconditionKind::ContextFresh {
                    max_age_ms: config.context_freshness_max_age_ms,
                },
                description: format!(
                    "Low-confidence step (score {:.3}): context must be fresh",
                    assignment.score
                ),
                required: true,
            });
        }

        // Build compensations.
        let mut compensations = Vec::new();
        if config.auto_compensate_high_risk
            && (risk == StepRisk::High || risk == StepRisk::Critical)
        {
            compensations.push(CompensatingAction {
                step_id: step_id.clone(),
                description: format!("Auto-compensation for high-risk step {}", step_id),
                action_type: config.default_compensation.clone(),
            });
        }

        steps.push(TxStep {
            id: step_id,
            bead_id: assignment.bead_id.clone(),
            agent_id: assignment.agent_id.clone(),
            description: format!(
                "Execute {} on agent {}",
                assignment.bead_id, assignment.agent_id
            ),
            depends_on,
            preconditions,
            compensations,
            risk,
            score: assignment.score,
        });
    }

    // Compute execution order via topological sort.
    let sort_result = topological_sort_steps(&steps);
    record_cycle_rejections(
        &steps,
        &sort_result.unresolved_step_ids,
        &mut rejected_edges,
    );
    let execution_order = sort_result.execution_order;
    let parallel_levels = compute_parallel_levels(&steps, &execution_order);
    let risk_summary = compute_risk_summary(&steps);
    let plan_hash = compute_plan_hash(
        plan_id,
        &steps,
        &execution_order,
        &parallel_levels,
        &risk_summary,
        &rejected_edges,
        &rejected_assignments,
    );

    TxPlan {
        plan_id: plan_id.to_string(),
        plan_hash,
        steps,
        execution_order,
        parallel_levels,
        risk_summary,
        rejected_edges,
        rejected_assignments,
    }
}

/// Classify risk based on tags and score.
fn classify_risk(tags: &[String], score: f64) -> StepRisk {
    if tags.iter().any(|t| t == "critical" || t == "destructive") {
        return StepRisk::Critical;
    }
    if tags.iter().any(|t| t == "risky" || t == "unsafe") {
        return StepRisk::High;
    }
    if score < 0.3 {
        return StepRisk::High;
    }
    if score < 0.6 {
        return StepRisk::Medium;
    }
    StepRisk::Low
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TopologicalSortResult {
    execution_order: Vec<String>,
    unresolved_step_ids: Vec<String>,
}

/// Topological sort of steps using Kahn's algorithm.
fn topological_sort_steps(steps: &[TxStep]) -> TopologicalSortResult {
    let step_ids: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

    for step in steps {
        in_degree.entry(step.id.as_str()).or_insert(0);
        adj.entry(step.id.as_str()).or_default();
        for dep in &step.depends_on {
            if step_ids.contains(dep.as_str()) {
                adj.entry(dep.as_str()).or_default().push(step.id.as_str());
                *in_degree.entry(step.id.as_str()).or_insert(0) += 1;
            }
        }
    }

    let mut roots: Vec<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(&id, _)| id)
        .collect();
    roots.sort(); // deterministic ordering
    let mut queue: VecDeque<&str> = roots.into();

    let mut order = Vec::new();
    while let Some(node) = queue.pop_front() {
        order.push(node.to_string());
        if let Some(neighbors) = adj.get(node) {
            for &neighbor in neighbors {
                if let Some(deg) = in_degree.get_mut(neighbor) {
                    *deg -= 1;
                    if *deg == 0 {
                        // Insert sorted for determinism.
                        let pos = queue
                            .iter()
                            .position(|existing| *existing > neighbor)
                            .unwrap_or(queue.len());
                        queue.insert(pos, neighbor);
                    }
                }
            }
        }
    }

    let resolved: HashSet<&str> = order.iter().map(String::as_str).collect();
    let mut unresolved_step_ids: Vec<String> = step_ids
        .into_iter()
        .filter(|step_id| !resolved.contains(*step_id))
        .map(str::to_string)
        .collect();
    unresolved_step_ids.sort();

    TopologicalSortResult {
        execution_order: order,
        unresolved_step_ids,
    }
}

fn record_cycle_rejections(
    steps: &[TxStep],
    unresolved_step_ids: &[String],
    rejected_edges: &mut Vec<RejectedEdge>,
) {
    if unresolved_step_ids.is_empty() {
        return;
    }

    let unresolved: HashSet<&str> = unresolved_step_ids.iter().map(String::as_str).collect();
    let mut cyclic_edges = Vec::new();
    for step in steps
        .iter()
        .filter(|step| unresolved.contains(step.id.as_str()))
    {
        for dep in step
            .depends_on
            .iter()
            .filter(|dep| unresolved.contains(dep.as_str()))
        {
            cyclic_edges.push(RejectedEdge {
                from_step: dep.clone(),
                to_step: step.id.clone(),
                reason: format!("Dependency cycle detected involving {} -> {}", dep, step.id),
            });
        }
    }
    cyclic_edges.sort_by(|left, right| {
        left.from_step
            .cmp(&right.from_step)
            .then_with(|| left.to_step.cmp(&right.to_step))
            .then_with(|| left.reason.cmp(&right.reason))
    });
    rejected_edges.extend(cyclic_edges);
}

/// Compute parallel execution levels (steps that can run concurrently).
fn compute_parallel_levels(steps: &[TxStep], execution_order: &[String]) -> Vec<Vec<String>> {
    let step_map: HashMap<&str, &TxStep> = steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut level_of: HashMap<String, usize> = HashMap::new();

    for step_id in execution_order {
        let step = match step_map.get(step_id.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let max_dep_level = step
            .depends_on
            .iter()
            .filter_map(|dep| level_of.get(dep))
            .max()
            .copied();

        let my_level = match max_dep_level {
            Some(l) => l + 1,
            None => 0,
        };

        level_of.insert(step_id.clone(), my_level);
        while levels.len() <= my_level {
            levels.push(Vec::new());
        }
        levels[my_level].push(step_id.clone());
    }

    levels
}

/// Compute risk summary for the plan.
fn compute_risk_summary(steps: &[TxStep]) -> TxRiskSummary {
    let total = steps.len();
    let high = steps.iter().filter(|s| s.risk == StepRisk::High).count();
    let critical = steps
        .iter()
        .filter(|s| s.risk == StepRisk::Critical)
        .count();
    let uncompensated = steps
        .iter()
        .filter(|s| {
            (s.risk == StepRisk::High || s.risk == StepRisk::Critical) && s.compensations.is_empty()
        })
        .count();

    let overall = if critical > 0 {
        StepRisk::Critical
    } else if high > 0 {
        StepRisk::High
    } else if steps.iter().any(|s| s.risk == StepRisk::Medium) {
        StepRisk::Medium
    } else {
        StepRisk::Low
    };

    TxRiskSummary {
        total_steps: total,
        high_risk_count: high,
        critical_risk_count: critical,
        uncompensated_steps: uncompensated,
        overall_risk: overall,
    }
}

/// Compute a deterministic content hash for the safety-relevant plan shape.
fn compute_plan_hash(
    plan_id: &str,
    steps: &[TxStep],
    execution_order: &[String],
    parallel_levels: &[Vec<String>],
    risk_summary: &TxRiskSummary,
    rejected_edges: &[RejectedEdge],
    rejected_assignments: &[RejectedAssignment],
) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    hash_str(&mut hash, "plan_id", plan_id);

    for step in steps {
        hash_str(&mut hash, "step.id", &step.id);
        hash_str(&mut hash, "step.bead_id", &step.bead_id);
        hash_str(&mut hash, "step.agent_id", &step.agent_id);
        hash_str(&mut hash, "step.description", &step.description);
        hash_string_slice(&mut hash, "step.depends_on", &step.depends_on);
        for precondition in &step.preconditions {
            hash_precondition(&mut hash, precondition);
        }
        for compensation in &step.compensations {
            hash_compensation(&mut hash, compensation);
        }
        hash_step_risk(&mut hash, "step.risk", step.risk);
        hash_u64(&mut hash, "step.score", step.score.to_bits());
    }

    hash_string_slice(&mut hash, "execution_order", execution_order);
    for level in parallel_levels {
        hash_string_slice(&mut hash, "parallel_level", level);
    }
    hash_usize(
        &mut hash,
        "risk_summary.total_steps",
        risk_summary.total_steps,
    );
    hash_usize(
        &mut hash,
        "risk_summary.high_risk_count",
        risk_summary.high_risk_count,
    );
    hash_usize(
        &mut hash,
        "risk_summary.critical_risk_count",
        risk_summary.critical_risk_count,
    );
    hash_usize(
        &mut hash,
        "risk_summary.uncompensated_steps",
        risk_summary.uncompensated_steps,
    );
    hash_step_risk(
        &mut hash,
        "risk_summary.overall_risk",
        risk_summary.overall_risk,
    );
    for rejected_edge in rejected_edges {
        hash_str(
            &mut hash,
            "rejected_edge.from_step",
            &rejected_edge.from_step,
        );
        hash_str(&mut hash, "rejected_edge.to_step", &rejected_edge.to_step);
        hash_str(&mut hash, "rejected_edge.reason", &rejected_edge.reason);
    }
    for rejected_assignment in rejected_assignments {
        hash_str(
            &mut hash,
            "rejected_assignment.bead_id",
            &rejected_assignment.bead_id,
        );
        hash_str(
            &mut hash,
            "rejected_assignment.agent_id",
            &rejected_assignment.agent_id,
        );
        hash_str(
            &mut hash,
            "rejected_assignment.reason",
            &rejected_assignment.reason,
        );
    }

    hash
}

fn hash_precondition(hash: &mut u64, precondition: &Precondition) {
    match &precondition.kind {
        PreconditionKind::PolicyApproved => hash_str(hash, "precondition.kind", "policy_approved"),
        PreconditionKind::ReservationHeld { paths } => {
            hash_str(hash, "precondition.kind", "reservation_held");
            hash_string_slice(hash, "precondition.paths", paths);
        }
        PreconditionKind::ApprovalRequired { approver } => {
            hash_str(hash, "precondition.kind", "approval_required");
            hash_str(hash, "precondition.approver", approver);
        }
        PreconditionKind::TargetReachable { target_id } => {
            hash_str(hash, "precondition.kind", "target_reachable");
            hash_str(hash, "precondition.target_id", target_id);
        }
        PreconditionKind::ContextFresh { max_age_ms } => {
            hash_str(hash, "precondition.kind", "context_fresh");
            hash_u64(hash, "precondition.max_age_ms", *max_age_ms);
        }
    }
    hash_str(hash, "precondition.description", &precondition.description);
    hash_bool(hash, "precondition.required", precondition.required);
}

fn hash_compensation(hash: &mut u64, compensation: &CompensatingAction) {
    hash_str(hash, "compensation.step_id", &compensation.step_id);
    hash_str(hash, "compensation.description", &compensation.description);
    match &compensation.action_type {
        CompensationKind::Rollback => hash_str(hash, "compensation.kind", "rollback"),
        CompensationKind::NotifyOperator => hash_str(hash, "compensation.kind", "notify_operator"),
        CompensationKind::RetryWithBackoff { max_retries } => {
            hash_str(hash, "compensation.kind", "retry_with_backoff");
            hash_u64(hash, "compensation.max_retries", u64::from(*max_retries));
        }
        CompensationKind::SkipAndContinue => {
            hash_str(hash, "compensation.kind", "skip_and_continue");
        }
        CompensationKind::Alternative {
            alternative_step_id,
        } => {
            hash_str(hash, "compensation.kind", "alternative");
            hash_str(
                hash,
                "compensation.alternative_step_id",
                alternative_step_id,
            );
        }
    }
}

fn hash_step_risk(hash: &mut u64, field: &str, risk: StepRisk) {
    let risk_name = match risk {
        StepRisk::Low => "low",
        StepRisk::Medium => "medium",
        StepRisk::High => "high",
        StepRisk::Critical => "critical",
    };
    hash_str(hash, field, risk_name);
}

fn hash_string_slice(hash: &mut u64, field: &str, values: &[String]) {
    hash_usize(hash, field, values.len());
    for value in values {
        hash_str(hash, field, value);
    }
}

fn hash_bool(hash: &mut u64, field: &str, value: bool) {
    hash_str(hash, field, if value { "true" } else { "false" });
}

fn hash_usize(hash: &mut u64, field: &str, value: usize) {
    hash_u64(hash, field, value as u64);
}

fn hash_u64(hash: &mut u64, field: &str, value: u64) {
    hash_bytes(hash, field.as_bytes());
    hash_bytes(hash, &value.to_le_bytes());
}

fn hash_str(hash: &mut u64, field: &str, value: &str) {
    hash_bytes(hash, field.as_bytes());
    hash_usize(hash, "len", value.len());
    hash_bytes(hash, value.as_bytes());
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn assignment(bead_id: &str, agent_id: &str, score: f64) -> PlannerAssignment {
        PlannerAssignment {
            bead_id: bead_id.to_string(),
            agent_id: agent_id.to_string(),
            score,
            tags: Vec::new(),
            dependency_bead_ids: Vec::new(),
        }
    }

    fn assignment_with_deps(
        bead_id: &str,
        agent_id: &str,
        score: f64,
        deps: &[&str],
    ) -> PlannerAssignment {
        PlannerAssignment {
            bead_id: bead_id.to_string(),
            agent_id: agent_id.to_string(),
            score,
            tags: Vec::new(),
            dependency_bead_ids: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn assignment_with_tags(
        bead_id: &str,
        agent_id: &str,
        score: f64,
        tags: &[&str],
    ) -> PlannerAssignment {
        PlannerAssignment {
            bead_id: bead_id.to_string(),
            agent_id: agent_id.to_string(),
            score,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            dependency_bead_ids: Vec::new(),
        }
    }

    #[test]
    fn compile_empty_assignments() {
        let plan = compile_tx_plan("p1", &[], &CompilerConfig::default());
        assert_eq!(plan.plan_id, "p1");
        assert!(plan.steps.is_empty());
        assert!(plan.execution_order.is_empty());
        assert!(plan.parallel_levels.is_empty());
        assert_eq!(plan.risk_summary.total_steps, 0);
    }

    #[test]
    fn compile_single_assignment() {
        let assignments = vec![assignment("b1", "a1", 0.8)];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].bead_id, "b1");
        assert_eq!(plan.steps[0].agent_id, "a1");
        assert_eq!(plan.execution_order, vec!["step-b1"]);
        assert_eq!(plan.parallel_levels.len(), 1);
    }

    #[test]
    fn compile_linear_chain() {
        let assignments = vec![
            assignment("b1", "a1", 0.9),
            assignment_with_deps("b2", "a1", 0.7, &["b1"]),
            assignment_with_deps("b3", "a1", 0.5, &["b2"]),
        ];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        assert_eq!(plan.execution_order, vec!["step-b1", "step-b2", "step-b3"]);
        assert_eq!(plan.parallel_levels.len(), 3);
        assert_eq!(plan.parallel_levels[0], vec!["step-b1"]);
        assert_eq!(plan.parallel_levels[1], vec!["step-b2"]);
        assert_eq!(plan.parallel_levels[2], vec!["step-b3"]);
    }

    #[test]
    fn compile_parallel_independent() {
        let assignments = vec![
            assignment("b1", "a1", 0.9),
            assignment("b2", "a2", 0.8),
            assignment("b3", "a1", 0.7),
        ];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        // All independent → single parallel level.
        assert_eq!(plan.parallel_levels.len(), 1);
        assert_eq!(plan.parallel_levels[0].len(), 3);
    }

    #[test]
    fn compile_diamond_dag() {
        // b1 → b2, b1 → b3, b2 → b4, b3 → b4
        let assignments = vec![
            assignment("b1", "a1", 0.9),
            assignment_with_deps("b2", "a1", 0.8, &["b1"]),
            assignment_with_deps("b3", "a2", 0.7, &["b1"]),
            assignment_with_deps("b4", "a1", 0.6, &["b2", "b3"]),
        ];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        assert_eq!(plan.parallel_levels.len(), 3);
        assert_eq!(plan.parallel_levels[0], vec!["step-b1"]);
        // b2 and b3 can be parallel.
        let level1: HashSet<&str> = plan.parallel_levels[1].iter().map(|s| s.as_str()).collect();
        assert!(level1.contains("step-b2"));
        assert!(level1.contains("step-b3"));
        assert_eq!(plan.parallel_levels[2], vec!["step-b4"]);
    }

    #[test]
    fn compile_rejected_edge_external_dep() {
        let assignments = vec![assignment_with_deps("b1", "a1", 0.9, &["external"])];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        assert_eq!(plan.rejected_edges.len(), 1);
        assert!(plan.rejected_edges[0].reason.contains("external"));
        // b1 has no in-plan deps → it's at level 0.
        assert_eq!(plan.steps[0].depends_on.len(), 0);
    }

    proptest! {
        #[test]
        fn cyclic_assignments_emit_rejected_cycle_edges(cycle_len in 1usize..8) {
            let assignments = (0..cycle_len)
                .map(|idx| {
                    let bead_id = format!("b{idx}");
                    let dep = format!("b{}", (idx + 1) % cycle_len);
                    PlannerAssignment {
                        bead_id,
                        agent_id: "agent-a".to_string(),
                        score: 0.9,
                        tags: Vec::new(),
                        dependency_bead_ids: vec![dep],
                    }
                })
                .collect::<Vec<_>>();

            let plan = compile_tx_plan("cycle", &assignments, &CompilerConfig::default());

            prop_assert!(plan.execution_order.len() < plan.steps.len());
            prop_assert!(!plan.rejected_edges.is_empty());
            let all_rejections_are_cycles = plan
                .rejected_edges
                .iter()
                .all(|edge| edge.reason.contains("Dependency cycle detected"));
            prop_assert!(all_rejections_are_cycles);

            let rejected_step_ids = plan
                .rejected_edges
                .iter()
                .flat_map(|edge| [edge.from_step.as_str(), edge.to_step.as_str()])
                .collect::<HashSet<_>>();
            for assignment in &assignments {
                let step_id = format!("step-{}", assignment.bead_id);
                prop_assert!(rejected_step_ids.contains(step_id.as_str()));
            }
        }
    }

    #[test]
    fn compile_policy_preflight_precondition() {
        let assignments = vec![assignment("b1", "a1", 0.8)];
        let config = CompilerConfig {
            require_policy_preflight: true,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p1", &assignments, &config);
        assert!(
            plan.steps[0]
                .preconditions
                .iter()
                .any(|p| p.kind == PreconditionKind::PolicyApproved)
        );
    }

    #[test]
    fn compile_no_policy_preflight() {
        let assignments = vec![assignment("b1", "a1", 0.8)];
        let config = CompilerConfig {
            require_policy_preflight: false,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p1", &assignments, &config);
        assert!(
            !plan.steps[0]
                .preconditions
                .iter()
                .any(|p| p.kind == PreconditionKind::PolicyApproved)
        );
    }

    #[test]
    fn compile_context_freshness_for_low_score() {
        let assignments = vec![assignment("b1", "a1", 0.3)];
        let config = CompilerConfig {
            context_freshness_threshold: 0.5,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p1", &assignments, &config);
        let has_freshness = plan.steps[0]
            .preconditions
            .iter()
            .any(|p| matches!(p.kind, PreconditionKind::ContextFresh { .. }));
        assert!(
            has_freshness,
            "Low-score step should require context freshness"
        );
    }

    #[test]
    fn compile_no_context_freshness_for_high_score() {
        let assignments = vec![assignment("b1", "a1", 0.9)];
        let config = CompilerConfig::default();
        let plan = compile_tx_plan("p1", &assignments, &config);
        let has_freshness = plan.steps[0]
            .preconditions
            .iter()
            .any(|p| matches!(p.kind, PreconditionKind::ContextFresh { .. }));
        assert!(
            !has_freshness,
            "High-score step should not require context freshness"
        );
    }

    #[test]
    fn compile_auto_compensate_critical() {
        let assignments = vec![assignment_with_tags("b1", "a1", 0.9, &["critical"])];
        let config = CompilerConfig {
            auto_compensate_high_risk: true,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p1", &assignments, &config);
        assert_eq!(plan.steps[0].risk, StepRisk::Critical);
        assert!(!plan.steps[0].compensations.is_empty());
    }

    #[test]
    fn compile_auto_compensate_high() {
        let assignments = vec![assignment_with_tags("b1", "a1", 0.9, &["risky"])];
        let config = CompilerConfig::default();
        let plan = compile_tx_plan("p1", &assignments, &config);
        assert_eq!(plan.steps[0].risk, StepRisk::High);
        assert!(!plan.steps[0].compensations.is_empty());
    }

    #[test]
    fn compile_no_auto_compensate_when_disabled() {
        let assignments = vec![assignment_with_tags("b1", "a1", 0.9, &["critical"])];
        let config = CompilerConfig {
            auto_compensate_high_risk: false,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p1", &assignments, &config);
        assert!(plan.steps[0].compensations.is_empty());
    }

    #[test]
    fn compile_risk_classification_low() {
        assert_eq!(classify_risk(&[], 0.8), StepRisk::Low);
    }

    #[test]
    fn compile_risk_classification_medium() {
        assert_eq!(classify_risk(&[], 0.5), StepRisk::Medium);
    }

    #[test]
    fn compile_risk_classification_high_score() {
        assert_eq!(classify_risk(&[], 0.2), StepRisk::High);
    }

    #[test]
    fn compile_risk_classification_critical_tag() {
        assert_eq!(
            classify_risk(&["critical".to_string()], 0.9),
            StepRisk::Critical
        );
    }

    #[test]
    fn compile_risk_classification_destructive_tag() {
        assert_eq!(
            classify_risk(&["destructive".to_string()], 0.9),
            StepRisk::Critical
        );
    }

    #[test]
    fn compile_risk_summary_all_low() {
        let assignments = vec![assignment("b1", "a1", 0.9), assignment("b2", "a2", 0.8)];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        assert_eq!(plan.risk_summary.total_steps, 2);
        assert_eq!(plan.risk_summary.high_risk_count, 0);
        assert_eq!(plan.risk_summary.critical_risk_count, 0);
        assert_eq!(plan.risk_summary.overall_risk, StepRisk::Low);
    }

    #[test]
    fn compile_risk_summary_with_critical() {
        let assignments = vec![
            assignment("b1", "a1", 0.9),
            assignment_with_tags("b2", "a2", 0.8, &["critical"]),
        ];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        assert_eq!(plan.risk_summary.critical_risk_count, 1);
        assert_eq!(plan.risk_summary.overall_risk, StepRisk::Critical);
        // Auto-compensated → uncompensated should be 0.
        assert_eq!(plan.risk_summary.uncompensated_steps, 0);
    }

    #[test]
    fn compile_risk_summary_uncompensated() {
        let assignments = vec![assignment_with_tags("b1", "a1", 0.9, &["risky"])];
        let config = CompilerConfig {
            auto_compensate_high_risk: false,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p1", &assignments, &config);
        assert_eq!(plan.risk_summary.uncompensated_steps, 1);
    }

    #[test]
    fn compile_deterministic_hash() {
        let assignments = vec![
            assignment("b1", "a1", 0.9),
            assignment_with_deps("b2", "a2", 0.8, &["b1"]),
        ];
        let config = CompilerConfig::default();
        let plan1 = compile_tx_plan("p1", &assignments, &config);
        let plan2 = compile_tx_plan("p1", &assignments, &config);
        assert_eq!(plan1.plan_hash, plan2.plan_hash);
        assert_eq!(plan1.execution_order, plan2.execution_order);
    }

    #[test]
    fn compile_hash_changes_with_different_steps() {
        let a1 = vec![assignment("b1", "a1", 0.9)];
        let a2 = vec![assignment("b2", "a2", 0.8)];
        let plan1 = compile_tx_plan("p1", &a1, &CompilerConfig::default());
        let plan2 = compile_tx_plan("p1", &a2, &CompilerConfig::default());
        assert_ne!(plan1.plan_hash, plan2.plan_hash);
    }

    proptest! {
        #[test]
        fn plan_hash_changes_for_safety_relevant_plan_content(
            retry_count in 1u32..64,
            context_max_age_ms in 1u64..120_000,
        ) {
            let config = CompilerConfig::default();
            let base = vec![
                assignment("b1", "a1", 0.9),
                assignment_with_deps("b2", "a2", 0.8, &["b1"]),
            ];
            let base_hash = compile_tx_plan("p1", &base, &config).plan_hash;

            let changed_plan_id = compile_tx_plan("p2", &base, &config).plan_hash;
            prop_assert_ne!(base_hash, changed_plan_id);

            let changed_risk = vec![
                assignment("b1", "a1", 0.9),
                assignment_with_tags("b2", "a2", 0.8, &["critical"]),
            ];
            let changed_risk_hash = compile_tx_plan("p1", &changed_risk, &config).plan_hash;
            prop_assert_ne!(base_hash, changed_risk_hash);

            let context_config = CompilerConfig {
                context_freshness_threshold: 0.5,
                context_freshness_max_age_ms: context_max_age_ms,
                ..CompilerConfig::default()
            };
            let changed_context = vec![
                assignment("b1", "a1", 0.9),
                assignment_with_deps("b2", "a2", 0.1, &["b1"]),
            ];
            let changed_context_hash =
                compile_tx_plan("p1", &changed_context, &context_config).plan_hash;
            prop_assert_ne!(base_hash, changed_context_hash);

            let compensation_assignments =
                vec![assignment_with_tags("b1", "a1", 0.9, &["critical"])];
            let notify_hash = compile_tx_plan(
                "p1",
                &compensation_assignments,
                &CompilerConfig {
                    default_compensation: CompensationKind::NotifyOperator,
                    ..CompilerConfig::default()
                },
            )
            .plan_hash;
            let retry_hash = compile_tx_plan(
                "p1",
                &compensation_assignments,
                &CompilerConfig {
                    default_compensation: CompensationKind::RetryWithBackoff {
                        max_retries: retry_count,
                    },
                    ..CompilerConfig::default()
                },
            )
            .plan_hash;
            prop_assert_ne!(notify_hash, retry_hash);

            let rejected_external_a =
                vec![assignment_with_deps("b1", "a1", 0.9, &["external-a"])];
            let rejected_external_b =
                vec![assignment_with_deps("b1", "a1", 0.9, &["external-b"])];
            let rejected_a_hash =
                compile_tx_plan("p1", &rejected_external_a, &config).plan_hash;
            let rejected_b_hash =
                compile_tx_plan("p1", &rejected_external_b, &config).plan_hash;
            prop_assert_ne!(rejected_a_hash, rejected_b_hash);
        }
    }

    #[test]
    fn compile_step_description() {
        let assignments = vec![assignment("b1", "agent-x", 0.9)];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        assert!(plan.steps[0].description.contains("b1"));
        assert!(plan.steps[0].description.contains("agent-x"));
    }

    #[test]
    fn tx_plan_serde_roundtrip() {
        let assignments = vec![
            assignment("b1", "a1", 0.9),
            assignment_with_deps("b2", "a2", 0.7, &["b1"]),
        ];
        let plan = compile_tx_plan("p1", &assignments, &CompilerConfig::default());
        let json = serde_json::to_string(&plan).unwrap();
        let back: TxPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plan_id, "p1");
        assert_eq!(back.steps.len(), 2);
        assert_eq!(back.execution_order, plan.execution_order);
        assert_eq!(back.plan_hash, plan.plan_hash);
    }

    #[test]
    fn tx_step_serde_roundtrip() {
        let step = TxStep {
            id: "s1".to_string(),
            bead_id: "b1".to_string(),
            agent_id: "a1".to_string(),
            description: "test".to_string(),
            depends_on: vec!["s0".to_string()],
            preconditions: vec![Precondition {
                kind: PreconditionKind::PolicyApproved,
                description: "test".to_string(),
                required: true,
            }],
            compensations: vec![CompensatingAction {
                step_id: "s1".to_string(),
                description: "rollback".to_string(),
                action_type: CompensationKind::Rollback,
            }],
            risk: StepRisk::High,
            score: 0.5,
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: TxStep = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "s1");
        assert_eq!(back.risk, StepRisk::High);
    }

    #[test]
    fn precondition_kind_serde_roundtrip() {
        let kinds = vec![
            PreconditionKind::PolicyApproved,
            PreconditionKind::ReservationHeld {
                paths: vec!["a.rs".to_string()],
            },
            PreconditionKind::ApprovalRequired {
                approver: "ops".to_string(),
            },
            PreconditionKind::TargetReachable {
                target_id: "pane-1".to_string(),
            },
            PreconditionKind::ContextFresh { max_age_ms: 5000 },
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: PreconditionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, kind);
        }
    }

    #[test]
    fn compensation_kind_serde_roundtrip() {
        let kinds = vec![
            CompensationKind::Rollback,
            CompensationKind::NotifyOperator,
            CompensationKind::RetryWithBackoff { max_retries: 3 },
            CompensationKind::SkipAndContinue,
            CompensationKind::Alternative {
                alternative_step_id: "alt-1".to_string(),
            },
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: CompensationKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, kind);
        }
    }

    #[test]
    fn step_risk_ordering() {
        assert!(StepRisk::Low < StepRisk::Medium);
        assert!(StepRisk::Medium < StepRisk::High);
        assert!(StepRisk::High < StepRisk::Critical);
    }

    #[test]
    fn compiler_config_serde_roundtrip() {
        let config = CompilerConfig {
            require_policy_preflight: false,
            auto_compensate_high_risk: false,
            default_compensation: CompensationKind::Rollback,
            context_freshness_threshold: 0.3,
            context_freshness_max_age_ms: 30_000,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: CompilerConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.require_policy_preflight);
        assert!((back.context_freshness_threshold - 0.3).abs() < 1e-9);
    }

    // br-ft-6gf4j: strict-variant validation tests.

    fn make_assignment(bead_id: &str) -> PlannerAssignment {
        PlannerAssignment {
            bead_id: bead_id.to_string(),
            agent_id: "agent-A".to_string(),
            score: 0.8,
            tags: vec![],
            dependency_bead_ids: vec![],
        }
    }

    #[test]
    fn compile_tx_plan_strict_rejects_empty_bead_id() {
        let assignments = vec![
            make_assignment("b0"),
            make_assignment(""),
            make_assignment("b2"),
        ];
        let result = compile_tx_plan_strict("p", &assignments, &CompilerConfig::default());
        match result {
            Err(TxPlanCompileError::EmptyBeadId { assignment_index }) => {
                assert_eq!(assignment_index, 1);
            }
            other => panic!("expected EmptyBeadId at index 1, got {other:?}"),
        }
    }

    #[test]
    fn compile_tx_plan_strict_rejects_duplicate_bead_id() {
        let assignments = vec![
            make_assignment("b0"),
            make_assignment("b1"),
            make_assignment("b0"), // duplicate of index 0
        ];
        let result = compile_tx_plan_strict("p", &assignments, &CompilerConfig::default());
        match result {
            Err(TxPlanCompileError::DuplicateBeadId {
                bead_id,
                first_index,
                duplicate_index,
            }) => {
                assert_eq!(bead_id, "b0");
                assert_eq!(first_index, 0);
                assert_eq!(duplicate_index, 2);
            }
            other => panic!("expected DuplicateBeadId, got {other:?}"),
        }
    }

    #[test]
    fn compile_tx_plan_strict_accepts_unique_bead_ids() {
        let assignments = vec![
            make_assignment("b0"),
            make_assignment("b1"),
            make_assignment("b2"),
        ];
        let plan = compile_tx_plan_strict("p", &assignments, &CompilerConfig::default())
            .expect("strict mode accepts unique non-empty bead_ids");
        assert_eq!(plan.steps.len(), 3);
        // All step ids unique post-compile.
        let mut ids: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn compile_tx_plan_strict_error_serde_roundtrip() {
        let err = TxPlanCompileError::DuplicateBeadId {
            bead_id: "b-test".to_string(),
            first_index: 0,
            duplicate_index: 5,
        };
        let json = serde_json::to_string(&err).expect("serialize");
        let back: TxPlanCompileError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(err, back);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// br-ft-6gf4j: any duplicate bead_id in the assignment
        /// list MUST cause `compile_tx_plan_strict` to fail —
        /// regardless of duplicate position, total assignment
        /// count, or which bead_id is the duplicate.
        #[test]
        fn compile_tx_plan_strict_fails_on_any_duplicate(
            base in proptest::prelude::prop::collection::vec("b[0-9]{1,3}", 2..=8),
            dup_idx in 0usize..8,
        ) {
            // Force a duplicate by appending base[0] at the end.
            let mut ids = base.clone();
            let target = ids[dup_idx % ids.len()].clone();
            ids.push(target);

            let assignments: Vec<PlannerAssignment> =
                ids.iter().map(|id| make_assignment(id)).collect();
            let result = compile_tx_plan_strict(
                "prop",
                &assignments,
                &CompilerConfig::default(),
            );
            // The strict variant must NOT produce a TxPlan with
            // duplicate step ids; it must fail loudly instead.
            let is_dup_err =
                matches!(result, Err(TxPlanCompileError::DuplicateBeadId { .. }));
            proptest::prop_assert!(
                is_dup_err,
                "strict mode must reject duplicate bead_ids; got result"
            );
        }

        /// br-ft-6gf4j: lenient `compile_tx_plan` MUST never
        /// produce duplicate step ids in the resulting TxPlan,
        /// even when input has duplicates — they're folded into
        /// `rejected_assignments` instead.
        #[test]
        fn compile_tx_plan_lenient_dedups_duplicate_step_ids(
            base in proptest::prelude::prop::collection::vec("b[0-9]{1,3}", 2..=8),
            dup_idx in 0usize..8,
        ) {
            let mut ids = base.clone();
            let target = ids[dup_idx % ids.len()].clone();
            ids.push(target);

            let assignments: Vec<PlannerAssignment> =
                ids.iter().map(|id| make_assignment(id)).collect();
            let plan = compile_tx_plan(
                "prop",
                &assignments,
                &CompilerConfig::default(),
            );

            let mut step_ids: Vec<&str> =
                plan.steps.iter().map(|s| s.id.as_str()).collect();
            step_ids.sort();
            let total = step_ids.len();
            step_ids.dedup();
            proptest::prop_assert_eq!(
                step_ids.len(),
                total,
                "lenient mode must produce unique step ids; rejected_assignments len={}",
                plan.rejected_assignments.len()
            );
        }
    }
}
