//! Transaction plan DAG compiler (ft-1i2ge.8.3).
//!
//! Compiles mission assignment outputs into an executable dependency graph
//! with explicit preconditions and compensating actions.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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

/// Compile planner assignments into a transaction plan.
#[must_use]
pub fn compile_tx_plan(
    plan_id: &str,
    assignments: &[PlannerAssignment],
    config: &CompilerConfig,
) -> TxPlan {
    let assigned_beads: HashSet<&str> = assignments.iter().map(|a| a.bead_id.as_str()).collect();
    let mut steps = Vec::new();
    let mut rejected_edges = Vec::new();

    for assignment in assignments {
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
    let execution_order = topological_sort_steps(&steps);
    let parallel_levels = compute_parallel_levels(&steps, &execution_order);
    let risk_summary = compute_risk_summary(&steps);
    let plan_hash = compute_plan_hash(&steps, &execution_order);

    TxPlan {
        plan_id: plan_id.to_string(),
        plan_hash,
        steps,
        execution_order,
        parallel_levels,
        risk_summary,
        rejected_edges,
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

/// Topological sort of steps using Kahn's algorithm.
fn topological_sort_steps(steps: &[TxStep]) -> Vec<String> {
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

    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(&id, _)| id)
        .collect();
    queue.sort(); // deterministic ordering

    let mut order = Vec::new();
    while let Some(node) = queue.first().copied() {
        queue.remove(0);
        order.push(node.to_string());
        if let Some(neighbors) = adj.get(node) {
            for &neighbor in neighbors {
                if let Some(deg) = in_degree.get_mut(neighbor) {
                    *deg -= 1;
                    if *deg == 0 {
                        // Insert sorted for determinism.
                        let pos = queue.binary_search(&neighbor).unwrap_or_else(|p| p);
                        queue.insert(pos, neighbor);
                    }
                }
            }
        }
    }

    order
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

/// Compute a deterministic hash for the plan.
fn compute_plan_hash(steps: &[TxStep], execution_order: &[String]) -> u64 {
    // FNV-1a hash over step IDs and execution order.
    let mut hash: u64 = 0xcbf29ce484222325;
    for step_id in execution_order {
        for byte in step_id.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    for step in steps {
        for byte in step.bead_id.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        for byte in step.agent_id.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
