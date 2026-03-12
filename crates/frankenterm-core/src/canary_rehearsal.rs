//! Canary rollout rehearsal and fail-safe drill orchestration (ft-e34d9.10.8.3).
//!
//! Builds on `canary_rollout_controller` (phase state machine) and
//! `soak_confidence_gate` (confidence evaluation) to provide rehearsal
//! execution, rollback drill validation, and user-disruption budgets.
//!
//! # Architecture
//!
//! ```text
//! RehearsalPlan
//!   ├── CohortDefinition[] (agent groups for staged rollout)
//!   ├── PromotionCriteria (thresholds per phase transition)
//!   ├── RollbackTrigger[] (conditions that force rollback)
//!   └── DisruptionBudget (max tolerable user-facing impact)
//!
//! RehearsalRunner
//!   ├── execute_promotion_drill() → DrillResult
//!   ├── execute_rollback_drill() → DrillResult
//!   └── execute_fail_safe_drill() → DrillResult
//!
//! RehearsalReport
//!   ├── DrillResult[] (per-drill evidence)
//!   ├── DisruptionAccounting (actual vs budget)
//!   └── RehearsalVerdict (Ready/Conditional/NotReady)
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Canary cohort management
// =============================================================================

/// A cohort of agents/panes for staged rollout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CohortDefinition {
    /// Cohort identifier (e.g., "canary-1", "early-adopters").
    pub cohort_id: String,
    /// Fraction of total fleet in this cohort (0.0–1.0).
    pub fraction: f64,
    /// Agent/pane IDs assigned to this cohort.
    pub members: Vec<String>,
    /// Promotion order (lower = earlier).
    pub order: u32,
    /// Whether this cohort can be rolled back independently.
    pub independent_rollback: bool,
}

impl CohortDefinition {
    /// Create a new cohort.
    #[must_use]
    pub fn new(cohort_id: impl Into<String>, fraction: f64, order: u32) -> Self {
        Self {
            cohort_id: cohort_id.into(),
            fraction,
            members: Vec::new(),
            order,
            independent_rollback: true,
        }
    }

    /// Add a member to the cohort.
    pub fn add_member(&mut self, member_id: impl Into<String>) {
        self.members.push(member_id.into());
    }

    /// Number of members.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.members.len()
    }
}

/// Standard cohort layout for a phased rollout.
#[must_use]
pub fn standard_cohorts() -> Vec<CohortDefinition> {
    vec![
        CohortDefinition {
            cohort_id: "canary".into(),
            fraction: 0.05,
            members: Vec::new(),
            order: 0,
            independent_rollback: true,
        },
        CohortDefinition {
            cohort_id: "early-adopters".into(),
            fraction: 0.20,
            members: Vec::new(),
            order: 1,
            independent_rollback: true,
        },
        CohortDefinition {
            cohort_id: "general-availability".into(),
            fraction: 0.75,
            members: Vec::new(),
            order: 2,
            independent_rollback: false,
        },
    ]
}

// =============================================================================
// Promotion and rollback criteria
// =============================================================================

/// Criteria for promoting a cohort to the next phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionCriteria {
    /// Minimum soak period before promotion (ms).
    pub min_soak_ms: u64,
    /// Minimum pass rate during soak.
    pub min_pass_rate: f64,
    /// Maximum error rate during soak.
    pub max_error_rate: f64,
    /// Maximum p95 latency allowed (ms).
    pub max_p95_latency_ms: u64,
    /// Whether human approval is required for promotion.
    pub requires_human_approval: bool,
    /// Maximum user-facing disruptions allowed.
    pub max_disruptions: u32,
}

impl PromotionCriteria {
    /// Conservative criteria for production cutover.
    #[must_use]
    pub fn production() -> Self {
        Self {
            min_soak_ms: 3_600_000,  // 1 hour
            min_pass_rate: 0.99,
            max_error_rate: 0.01,
            max_p95_latency_ms: 500,
            requires_human_approval: true,
            max_disruptions: 0,
        }
    }

    /// Relaxed criteria for rehearsal/staging.
    #[must_use]
    pub fn rehearsal() -> Self {
        Self {
            min_soak_ms: 60_000,  // 1 minute
            min_pass_rate: 0.95,
            max_error_rate: 0.05,
            max_p95_latency_ms: 2000,
            requires_human_approval: false,
            max_disruptions: 5,
        }
    }
}

/// Condition that triggers automatic rollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackTrigger {
    /// Trigger identifier.
    pub trigger_id: String,
    /// Human description.
    pub description: String,
    /// Trigger type.
    pub trigger_type: RollbackTriggerType,
    /// Threshold value.
    pub threshold: f64,
    /// Time window for evaluation (ms).
    pub window_ms: u64,
}

/// Types of rollback triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RollbackTriggerType {
    /// Error rate exceeds threshold.
    ErrorRateSpike,
    /// Latency exceeds threshold.
    LatencySpike,
    /// Agent crash rate exceeds threshold.
    CrashRate,
    /// User-facing disruption count exceeds budget.
    DisruptionBudgetExceeded,
    /// Health check failures exceed threshold.
    HealthCheckFailures,
    /// Manual operator trigger.
    OperatorManual,
}

/// Standard rollback triggers.
#[must_use]
pub fn standard_rollback_triggers() -> Vec<RollbackTrigger> {
    vec![
        RollbackTrigger {
            trigger_id: "RB-01-error-spike".into(),
            description: "Error rate exceeds 5% over 5 minutes".into(),
            trigger_type: RollbackTriggerType::ErrorRateSpike,
            threshold: 0.05,
            window_ms: 300_000,
        },
        RollbackTrigger {
            trigger_id: "RB-02-latency-spike".into(),
            description: "P95 latency exceeds 2000ms over 5 minutes".into(),
            trigger_type: RollbackTriggerType::LatencySpike,
            threshold: 2000.0,
            window_ms: 300_000,
        },
        RollbackTrigger {
            trigger_id: "RB-03-crash-rate".into(),
            description: "Agent crash rate exceeds 1% over 10 minutes".into(),
            trigger_type: RollbackTriggerType::CrashRate,
            threshold: 0.01,
            window_ms: 600_000,
        },
        RollbackTrigger {
            trigger_id: "RB-04-disruption-budget".into(),
            description: "User-facing disruptions exceed budget".into(),
            trigger_type: RollbackTriggerType::DisruptionBudgetExceeded,
            threshold: 1.0,
            window_ms: 0, // Cumulative, no window.
        },
        RollbackTrigger {
            trigger_id: "RB-05-health-failures".into(),
            description: "Health check failure rate exceeds 10% over 5 minutes".into(),
            trigger_type: RollbackTriggerType::HealthCheckFailures,
            threshold: 0.10,
            window_ms: 300_000,
        },
    ]
}

// =============================================================================
// Disruption budget
// =============================================================================

/// User-facing disruption budget for the rollout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisruptionBudget {
    /// Maximum tolerable latency increase (ms).
    pub max_latency_increase_ms: u64,
    /// Maximum error rate increase (percentage points).
    pub max_error_rate_increase: f64,
    /// Maximum total disruption events.
    pub max_disruption_events: u32,
    /// Maximum time-to-recovery if rollback needed (ms).
    pub max_recovery_time_ms: u64,
}

impl DisruptionBudget {
    /// Strict budget for production.
    #[must_use]
    pub fn production() -> Self {
        Self {
            max_latency_increase_ms: 100,
            max_error_rate_increase: 0.005,
            max_disruption_events: 0,
            max_recovery_time_ms: 60_000, // 1 minute
        }
    }

    /// Relaxed budget for rehearsals.
    #[must_use]
    pub fn rehearsal() -> Self {
        Self {
            max_latency_increase_ms: 500,
            max_error_rate_increase: 0.05,
            max_disruption_events: 10,
            max_recovery_time_ms: 300_000, // 5 minutes
        }
    }
}

/// Actual disruption observed during a drill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisruptionAccounting {
    /// Observed latency increase (ms).
    pub latency_increase_ms: u64,
    /// Observed error rate increase.
    pub error_rate_increase: f64,
    /// Disruption events observed.
    pub disruption_events: u32,
    /// Actual recovery time (ms), if rollback was exercised.
    pub recovery_time_ms: Option<u64>,
}

impl DisruptionAccounting {
    /// Check if the accounting is within the given budget.
    #[must_use]
    pub fn within_budget(&self, budget: &DisruptionBudget) -> bool {
        if self.latency_increase_ms > budget.max_latency_increase_ms {
            return false;
        }
        if self.error_rate_increase > budget.max_error_rate_increase {
            return false;
        }
        if self.disruption_events > budget.max_disruption_events {
            return false;
        }
        if let Some(rt) = self.recovery_time_ms {
            if rt > budget.max_recovery_time_ms {
                return false;
            }
        }
        true
    }

    /// Zero disruption accounting.
    #[must_use]
    pub fn zero() -> Self {
        Self {
            latency_increase_ms: 0,
            error_rate_increase: 0.0,
            disruption_events: 0,
            recovery_time_ms: None,
        }
    }
}

// =============================================================================
// Drill execution
// =============================================================================

/// Type of drill being rehearsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DrillType {
    /// Promote canary cohort to next phase.
    Promotion,
    /// Roll back a cohort to previous phase.
    Rollback,
    /// Simulate a fail-safe scenario (emergency stop + rollback).
    FailSafe,
    /// End-to-end rollout (promote all cohorts through all phases).
    FullRollout,
    /// Verify recovery from partial failure during promotion.
    PartialFailureRecovery,
}

impl DrillType {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Promotion => "promotion",
            Self::Rollback => "rollback",
            Self::FailSafe => "fail-safe",
            Self::FullRollout => "full-rollout",
            Self::PartialFailureRecovery => "partial-failure-recovery",
        }
    }
}

/// A single drill step result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillStep {
    /// Step identifier.
    pub step_id: String,
    /// Description of what this step does.
    pub description: String,
    /// Whether the step succeeded.
    pub success: bool,
    /// Elapsed time (ms).
    pub elapsed_ms: u64,
    /// Error if failed.
    pub error: Option<String>,
    /// Observations during the step.
    pub observations: Vec<String>,
}

impl DrillStep {
    /// Create a passing step.
    #[must_use]
    pub fn pass(step_id: impl Into<String>, description: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            step_id: step_id.into(),
            description: description.into(),
            success: true,
            elapsed_ms,
            error: None,
            observations: Vec::new(),
        }
    }

    /// Create a failing step.
    #[must_use]
    pub fn fail(
        step_id: impl Into<String>,
        description: impl Into<String>,
        elapsed_ms: u64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            step_id: step_id.into(),
            description: description.into(),
            success: false,
            elapsed_ms,
            error: Some(error.into()),
            observations: Vec::new(),
        }
    }

    /// Add an observation.
    pub fn observe(&mut self, note: impl Into<String>) {
        self.observations.push(note.into());
    }
}

/// Result of executing a single drill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillResult {
    /// Drill identifier.
    pub drill_id: String,
    /// Drill type.
    pub drill_type: DrillType,
    /// Cohort being exercised.
    pub cohort_id: String,
    /// When the drill started (epoch ms).
    pub started_at_ms: u64,
    /// When the drill ended (epoch ms).
    pub ended_at_ms: u64,
    /// Individual steps.
    pub steps: Vec<DrillStep>,
    /// Disruption observed.
    pub disruption: DisruptionAccounting,
    /// Overall success.
    pub success: bool,
    /// Failure reason if not successful.
    pub failure_reason: Option<String>,
}

impl DrillResult {
    /// Total elapsed time.
    #[must_use]
    pub fn total_elapsed_ms(&self) -> u64 {
        self.ended_at_ms.saturating_sub(self.started_at_ms)
    }

    /// Count of steps passed.
    #[must_use]
    pub fn steps_passed(&self) -> usize {
        self.steps.iter().filter(|s| s.success).count()
    }

    /// Count of steps failed.
    #[must_use]
    pub fn steps_failed(&self) -> usize {
        self.steps.iter().filter(|s| !s.success).count()
    }

    /// Step pass rate.
    #[must_use]
    pub fn step_pass_rate(&self) -> f64 {
        if self.steps.is_empty() {
            return 0.0;
        }
        self.steps_passed() as f64 / self.steps.len() as f64
    }
}

// =============================================================================
// Rehearsal plan and report
// =============================================================================

/// Complete rehearsal plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalPlan {
    /// Plan identifier.
    pub plan_id: String,
    /// Cohorts for staged rollout.
    pub cohorts: Vec<CohortDefinition>,
    /// Promotion criteria per phase.
    pub promotion_criteria: PromotionCriteria,
    /// Rollback triggers.
    pub rollback_triggers: Vec<RollbackTrigger>,
    /// Disruption budget.
    pub disruption_budget: DisruptionBudget,
    /// Drills to execute.
    pub drill_types: Vec<DrillType>,
}

impl RehearsalPlan {
    /// Create a standard rehearsal plan.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            plan_id: "RP-asupersync-cutover".into(),
            cohorts: standard_cohorts(),
            promotion_criteria: PromotionCriteria::rehearsal(),
            rollback_triggers: standard_rollback_triggers(),
            disruption_budget: DisruptionBudget::rehearsal(),
            drill_types: vec![
                DrillType::Promotion,
                DrillType::Rollback,
                DrillType::FailSafe,
                DrillType::PartialFailureRecovery,
            ],
        }
    }

    /// Create a production rehearsal plan.
    #[must_use]
    pub fn production() -> Self {
        Self {
            plan_id: "RP-production-cutover".into(),
            cohorts: standard_cohorts(),
            promotion_criteria: PromotionCriteria::production(),
            rollback_triggers: standard_rollback_triggers(),
            disruption_budget: DisruptionBudget::production(),
            drill_types: vec![
                DrillType::Promotion,
                DrillType::Rollback,
                DrillType::FailSafe,
                DrillType::FullRollout,
                DrillType::PartialFailureRecovery,
            ],
        }
    }

    /// Total drills to execute (drill_types × cohorts with independent rollback).
    #[must_use]
    pub fn total_drill_count(&self) -> usize {
        let rollback_cohorts = self
            .cohorts
            .iter()
            .filter(|c| c.independent_rollback)
            .count()
            .max(1);
        self.drill_types.len() * rollback_cohorts
    }
}

/// Overall rehearsal verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RehearsalVerdict {
    /// All drills passed and disruption within budget.
    Ready,
    /// All drills passed but disruption is borderline or minor issues found.
    Conditional,
    /// One or more critical drills failed or disruption exceeded budget.
    NotReady,
}

/// Complete rehearsal report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalReport {
    /// Plan that was executed.
    pub plan_id: String,
    /// When the rehearsal was executed (epoch ms).
    pub executed_at_ms: u64,
    /// Drill results.
    pub drills: Vec<DrillResult>,
    /// Aggregate disruption accounting.
    pub disruption: DisruptionAccounting,
    /// Overall verdict.
    pub verdict: RehearsalVerdict,
    /// Summary notes.
    pub notes: Vec<String>,
}

impl RehearsalReport {
    /// Create a report from drill results.
    #[must_use]
    pub fn from_drills(
        plan_id: impl Into<String>,
        executed_at_ms: u64,
        drills: Vec<DrillResult>,
        budget: &DisruptionBudget,
    ) -> Self {
        // Aggregate disruption.
        let disruption = aggregate_disruption(&drills);

        // Compute verdict.
        let all_passed = drills.iter().all(|d| d.success);
        let within_budget = disruption.within_budget(budget);

        let verdict = if all_passed && within_budget {
            RehearsalVerdict::Ready
        } else if all_passed {
            // Passed but over budget.
            RehearsalVerdict::Conditional
        } else {
            RehearsalVerdict::NotReady
        };

        let mut notes = Vec::new();
        if !all_passed {
            let failed: Vec<&str> = drills
                .iter()
                .filter(|d| !d.success)
                .map(|d| d.drill_id.as_str())
                .collect();
            notes.push(format!("Failed drills: {}", failed.join(", ")));
        }
        if !within_budget {
            notes.push("Disruption budget exceeded".into());
        }

        Self {
            plan_id: plan_id.into(),
            executed_at_ms,
            drills,
            disruption,
            verdict,
            notes,
        }
    }

    /// Count of drills passed.
    #[must_use]
    pub fn drills_passed(&self) -> usize {
        self.drills.iter().filter(|d| d.success).count()
    }

    /// Count of drills failed.
    #[must_use]
    pub fn drills_failed(&self) -> usize {
        self.drills.iter().filter(|d| !d.success).count()
    }

    /// Drill pass rate.
    #[must_use]
    pub fn drill_pass_rate(&self) -> f64 {
        if self.drills.is_empty() {
            return 0.0;
        }
        self.drills_passed() as f64 / self.drills.len() as f64
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push("=== Canary Rehearsal Report ===".to_string());
        lines.push(format!("Plan: {}", self.plan_id));
        lines.push(format!("Verdict: {:?}", self.verdict));
        lines.push(format!(
            "Drills: {}/{} passed",
            self.drills_passed(),
            self.drills.len()
        ));
        lines.push(String::new());

        lines.push("--- Disruption Accounting ---".to_string());
        lines.push(format!(
            "  Latency increase: {}ms",
            self.disruption.latency_increase_ms
        ));
        lines.push(format!(
            "  Error rate increase: {:.4}",
            self.disruption.error_rate_increase
        ));
        lines.push(format!(
            "  Disruption events: {}",
            self.disruption.disruption_events
        ));
        if let Some(rt) = self.disruption.recovery_time_ms {
            lines.push(format!("  Recovery time: {}ms", rt));
        }

        lines.push(String::new());
        lines.push("--- Drill Details ---".to_string());
        for drill in &self.drills {
            let icon = if drill.success { "PASS" } else { "FAIL" };
            lines.push(format!(
                "  [{}] {} ({}) — {}/{} steps, {}ms",
                icon,
                drill.drill_id,
                drill.drill_type.label(),
                drill.steps_passed(),
                drill.steps.len(),
                drill.total_elapsed_ms(),
            ));
            if let Some(reason) = &drill.failure_reason {
                lines.push(format!("       Reason: {}", reason));
            }
        }

        if !self.notes.is_empty() {
            lines.push(String::new());
            lines.push("--- Notes ---".to_string());
            for note in &self.notes {
                lines.push(format!("  - {}", note));
            }
        }

        lines.join("\n")
    }
}

/// Aggregate disruption from multiple drill results.
fn aggregate_disruption(drills: &[DrillResult]) -> DisruptionAccounting {
    let mut acc = DisruptionAccounting::zero();
    let mut max_recovery: Option<u64> = None;

    for drill in drills {
        acc.latency_increase_ms = acc
            .latency_increase_ms
            .max(drill.disruption.latency_increase_ms);
        if drill.disruption.error_rate_increase > acc.error_rate_increase {
            acc.error_rate_increase = drill.disruption.error_rate_increase;
        }
        acc.disruption_events += drill.disruption.disruption_events;
        if let Some(rt) = drill.disruption.recovery_time_ms {
            max_recovery = Some(max_recovery.map_or(rt, |prev: u64| prev.max(rt)));
        }
    }

    acc.recovery_time_ms = max_recovery;
    acc
}

// =============================================================================
// Rollback trigger evaluator
// =============================================================================

/// Observed metrics for trigger evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedMetrics {
    /// Current error rate.
    pub error_rate: f64,
    /// Current p95 latency (ms).
    pub p95_latency_ms: f64,
    /// Current crash rate.
    pub crash_rate: f64,
    /// Total disruption events so far.
    pub disruption_events: u32,
    /// Health check failure rate.
    pub health_check_failure_rate: f64,
    /// Whether an operator has manually triggered rollback.
    pub operator_triggered: bool,
}

/// Evaluate rollback triggers against observed metrics.
#[must_use]
pub fn evaluate_rollback_triggers(
    triggers: &[RollbackTrigger],
    metrics: &ObservedMetrics,
) -> Vec<TriggeredRollback> {
    let mut fired = Vec::new();

    for trigger in triggers {
        let tripped = match trigger.trigger_type {
            RollbackTriggerType::ErrorRateSpike => metrics.error_rate > trigger.threshold,
            RollbackTriggerType::LatencySpike => metrics.p95_latency_ms > trigger.threshold,
            RollbackTriggerType::CrashRate => metrics.crash_rate > trigger.threshold,
            RollbackTriggerType::DisruptionBudgetExceeded => {
                (metrics.disruption_events as f64) > trigger.threshold
            }
            RollbackTriggerType::HealthCheckFailures => {
                metrics.health_check_failure_rate > trigger.threshold
            }
            RollbackTriggerType::OperatorManual => metrics.operator_triggered,
        };

        if tripped {
            fired.push(TriggeredRollback {
                trigger_id: trigger.trigger_id.clone(),
                trigger_type: trigger.trigger_type,
                observed_value: match trigger.trigger_type {
                    RollbackTriggerType::ErrorRateSpike => metrics.error_rate,
                    RollbackTriggerType::LatencySpike => metrics.p95_latency_ms,
                    RollbackTriggerType::CrashRate => metrics.crash_rate,
                    RollbackTriggerType::DisruptionBudgetExceeded => {
                        metrics.disruption_events as f64
                    }
                    RollbackTriggerType::HealthCheckFailures => {
                        metrics.health_check_failure_rate
                    }
                    RollbackTriggerType::OperatorManual => 1.0,
                },
                threshold: trigger.threshold,
            });
        }
    }

    fired
}

/// A rollback trigger that has fired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggeredRollback {
    /// Which trigger fired.
    pub trigger_id: String,
    /// Trigger type.
    pub trigger_type: RollbackTriggerType,
    /// Observed value that exceeded threshold.
    pub observed_value: f64,
    /// Threshold that was exceeded.
    pub threshold: f64,
}

// =============================================================================
// Promotion evaluator
// =============================================================================

/// Observed soak metrics for promotion evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakMetrics {
    /// How long the cohort has been soaking (ms).
    pub soak_duration_ms: u64,
    /// Pass rate during soak.
    pub pass_rate: f64,
    /// Error rate during soak.
    pub error_rate: f64,
    /// P95 latency during soak (ms).
    pub p95_latency_ms: u64,
    /// Number of user-facing disruptions during soak.
    pub disruption_count: u32,
    /// Whether human approval has been granted.
    pub human_approved: bool,
}

/// Result of evaluating promotion readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionEvaluation {
    /// Whether promotion is approved.
    pub approved: bool,
    /// Individual check results.
    pub checks: Vec<PromotionCheck>,
    /// Summary.
    pub summary: String,
}

/// A single promotion gate check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionCheck {
    /// Check name.
    pub name: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Observed value.
    pub observed: String,
    /// Required threshold.
    pub required: String,
}

/// Evaluate whether a cohort is ready for promotion.
#[must_use]
pub fn evaluate_promotion(criteria: &PromotionCriteria, metrics: &SoakMetrics) -> PromotionEvaluation {
    let mut checks = Vec::new();

    checks.push(PromotionCheck {
        name: "soak-duration".into(),
        passed: metrics.soak_duration_ms >= criteria.min_soak_ms,
        observed: format!("{}ms", metrics.soak_duration_ms),
        required: format!(">= {}ms", criteria.min_soak_ms),
    });

    checks.push(PromotionCheck {
        name: "pass-rate".into(),
        passed: metrics.pass_rate >= criteria.min_pass_rate,
        observed: format!("{:.4}", metrics.pass_rate),
        required: format!(">= {:.4}", criteria.min_pass_rate),
    });

    checks.push(PromotionCheck {
        name: "error-rate".into(),
        passed: metrics.error_rate <= criteria.max_error_rate,
        observed: format!("{:.4}", metrics.error_rate),
        required: format!("<= {:.4}", criteria.max_error_rate),
    });

    checks.push(PromotionCheck {
        name: "p95-latency".into(),
        passed: metrics.p95_latency_ms <= criteria.max_p95_latency_ms,
        observed: format!("{}ms", metrics.p95_latency_ms),
        required: format!("<= {}ms", criteria.max_p95_latency_ms),
    });

    checks.push(PromotionCheck {
        name: "disruptions".into(),
        passed: metrics.disruption_count <= criteria.max_disruptions,
        observed: format!("{}", metrics.disruption_count),
        required: format!("<= {}", criteria.max_disruptions),
    });

    if criteria.requires_human_approval {
        checks.push(PromotionCheck {
            name: "human-approval".into(),
            passed: metrics.human_approved,
            observed: format!("{}", metrics.human_approved),
            required: "true".into(),
        });
    }

    let approved = checks.iter().all(|c| c.passed);
    let pass_count = checks.iter().filter(|c| c.passed).count();
    let total = checks.len();
    let summary = if approved {
        format!("APPROVED: all {total} checks passed")
    } else {
        format!("BLOCKED: {pass_count}/{total} checks passed")
    };

    PromotionEvaluation {
        approved,
        checks,
        summary,
    }
}

// =============================================================================
// Rehearsal telemetry snapshot
// =============================================================================

/// Telemetry snapshot for the rehearsal subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalTelemetry {
    /// Total rehearsals executed.
    pub rehearsals_executed: u64,
    /// Rehearsals that achieved Ready verdict.
    pub rehearsals_ready: u64,
    /// Rehearsals that achieved Conditional verdict.
    pub rehearsals_conditional: u64,
    /// Rehearsals that were NotReady.
    pub rehearsals_not_ready: u64,
    /// Total drills executed across all rehearsals.
    pub total_drills: u64,
    /// Drills passed.
    pub drills_passed: u64,
    /// Rollback triggers fired.
    pub rollback_triggers_fired: u64,
    /// Promotions evaluated.
    pub promotions_evaluated: u64,
    /// Promotions approved.
    pub promotions_approved: u64,
    /// Per-drill-type pass rates.
    pub drill_type_pass_rates: HashMap<String, (u64, u64)>,
}

impl RehearsalTelemetry {
    /// Create empty telemetry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rehearsals_executed: 0,
            rehearsals_ready: 0,
            rehearsals_conditional: 0,
            rehearsals_not_ready: 0,
            total_drills: 0,
            drills_passed: 0,
            rollback_triggers_fired: 0,
            promotions_evaluated: 0,
            promotions_approved: 0,
            drill_type_pass_rates: HashMap::new(),
        }
    }

    /// Record a rehearsal report.
    pub fn record_rehearsal(&mut self, report: &RehearsalReport) {
        self.rehearsals_executed += 1;
        match report.verdict {
            RehearsalVerdict::Ready => self.rehearsals_ready += 1,
            RehearsalVerdict::Conditional => self.rehearsals_conditional += 1,
            RehearsalVerdict::NotReady => self.rehearsals_not_ready += 1,
        }

        for drill in &report.drills {
            self.total_drills += 1;
            if drill.success {
                self.drills_passed += 1;
            }
            let entry = self
                .drill_type_pass_rates
                .entry(drill.drill_type.label().to_string())
                .or_insert((0, 0));
            entry.1 += 1;
            if drill.success {
                entry.0 += 1;
            }
        }
    }

    /// Record a promotion evaluation.
    pub fn record_promotion(&mut self, eval: &PromotionEvaluation) {
        self.promotions_evaluated += 1;
        if eval.approved {
            self.promotions_approved += 1;
        }
    }

    /// Record triggered rollbacks.
    pub fn record_rollback_triggers(&mut self, count: u64) {
        self.rollback_triggers_fired += count;
    }

    /// Overall drill pass rate.
    #[must_use]
    pub fn overall_drill_pass_rate(&self) -> f64 {
        if self.total_drills == 0 {
            return 0.0;
        }
        self.drills_passed as f64 / self.total_drills as f64
    }

    /// Promotion approval rate.
    #[must_use]
    pub fn promotion_approval_rate(&self) -> f64 {
        if self.promotions_evaluated == 0 {
            return 0.0;
        }
        self.promotions_approved as f64 / self.promotions_evaluated as f64
    }
}

impl Default for RehearsalTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Cohort tests ---

    #[test]
    fn standard_cohorts_sum_to_one() {
        let cohorts = standard_cohorts();
        let total: f64 = cohorts.iter().map(|c| c.fraction).sum();
        assert!((total - 1.0).abs() < 0.01);
    }

    #[test]
    fn standard_cohorts_ordered() {
        let cohorts = standard_cohorts();
        for w in cohorts.windows(2) {
            assert!(w[0].order < w[1].order);
        }
    }

    #[test]
    fn cohort_member_management() {
        let mut cohort = CohortDefinition::new("test", 0.1, 0);
        assert_eq!(cohort.member_count(), 0);
        cohort.add_member("agent-1");
        cohort.add_member("agent-2");
        assert_eq!(cohort.member_count(), 2);
    }

    // --- Promotion criteria tests ---

    #[test]
    fn production_criteria_stricter_than_rehearsal() {
        let prod = PromotionCriteria::production();
        let reh = PromotionCriteria::rehearsal();
        assert!(prod.min_soak_ms >= reh.min_soak_ms);
        assert!(prod.min_pass_rate >= reh.min_pass_rate);
        assert!(prod.max_error_rate <= reh.max_error_rate);
        assert!(prod.max_p95_latency_ms <= reh.max_p95_latency_ms);
    }

    #[test]
    fn production_requires_human_approval() {
        assert!(PromotionCriteria::production().requires_human_approval);
        assert!(!PromotionCriteria::rehearsal().requires_human_approval);
    }

    // --- Promotion evaluation tests ---

    #[test]
    fn promotion_approved_when_all_pass() {
        let criteria = PromotionCriteria::rehearsal();
        let metrics = SoakMetrics {
            soak_duration_ms: 120_000,
            pass_rate: 0.98,
            error_rate: 0.02,
            p95_latency_ms: 500,
            disruption_count: 1,
            human_approved: false,
        };
        let eval = evaluate_promotion(&criteria, &metrics);
        assert!(eval.approved);
        assert!(eval.checks.iter().all(|c| c.passed));
    }

    #[test]
    fn promotion_blocked_insufficient_soak() {
        let criteria = PromotionCriteria::rehearsal();
        let metrics = SoakMetrics {
            soak_duration_ms: 30_000, // Under 60s.
            pass_rate: 0.99,
            error_rate: 0.01,
            p95_latency_ms: 100,
            disruption_count: 0,
            human_approved: false,
        };
        let eval = evaluate_promotion(&criteria, &metrics);
        assert!(!eval.approved);
        let soak_check = eval.checks.iter().find(|c| c.name == "soak-duration").unwrap();
        assert!(!soak_check.passed);
    }

    #[test]
    fn promotion_blocked_high_error_rate() {
        let criteria = PromotionCriteria::rehearsal();
        let metrics = SoakMetrics {
            soak_duration_ms: 120_000,
            pass_rate: 0.80,
            error_rate: 0.20, // Over 5%.
            p95_latency_ms: 500,
            disruption_count: 0,
            human_approved: false,
        };
        let eval = evaluate_promotion(&criteria, &metrics);
        assert!(!eval.approved);
    }

    #[test]
    fn promotion_requires_human_approval_in_production() {
        let criteria = PromotionCriteria::production();
        let metrics = SoakMetrics {
            soak_duration_ms: 7_200_000,
            pass_rate: 0.999,
            error_rate: 0.001,
            p95_latency_ms: 100,
            disruption_count: 0,
            human_approved: false, // Not yet.
        };
        let eval = evaluate_promotion(&criteria, &metrics);
        assert!(!eval.approved);
        let human_check = eval.checks.iter().find(|c| c.name == "human-approval").unwrap();
        assert!(!human_check.passed);
    }

    // --- Rollback trigger tests ---

    #[test]
    fn no_triggers_fire_on_healthy_metrics() {
        let triggers = standard_rollback_triggers();
        let metrics = ObservedMetrics {
            error_rate: 0.001,
            p95_latency_ms: 200.0,
            crash_rate: 0.0,
            disruption_events: 0,
            health_check_failure_rate: 0.0,
            operator_triggered: false,
        };
        let fired = evaluate_rollback_triggers(&triggers, &metrics);
        assert!(fired.is_empty());
    }

    #[test]
    fn error_spike_trigger_fires() {
        let triggers = standard_rollback_triggers();
        let metrics = ObservedMetrics {
            error_rate: 0.10, // 10% > 5% threshold.
            p95_latency_ms: 200.0,
            crash_rate: 0.0,
            disruption_events: 0,
            health_check_failure_rate: 0.0,
            operator_triggered: false,
        };
        let fired = evaluate_rollback_triggers(&triggers, &metrics);
        assert!(!fired.is_empty());
        assert!(fired.iter().any(|t| t.trigger_type == RollbackTriggerType::ErrorRateSpike));
    }

    #[test]
    fn latency_spike_trigger_fires() {
        let triggers = standard_rollback_triggers();
        let metrics = ObservedMetrics {
            error_rate: 0.001,
            p95_latency_ms: 5000.0, // 5s > 2s threshold.
            crash_rate: 0.0,
            disruption_events: 0,
            health_check_failure_rate: 0.0,
            operator_triggered: false,
        };
        let fired = evaluate_rollback_triggers(&triggers, &metrics);
        assert!(fired.iter().any(|t| t.trigger_type == RollbackTriggerType::LatencySpike));
    }

    #[test]
    fn operator_manual_trigger_fires() {
        let triggers = standard_rollback_triggers();
        // Note: standard triggers don't include OperatorManual.
        let mut triggers = triggers;
        triggers.push(RollbackTrigger {
            trigger_id: "RB-06-manual".into(),
            description: "Manual rollback".into(),
            trigger_type: RollbackTriggerType::OperatorManual,
            threshold: 0.0,
            window_ms: 0,
        });
        let metrics = ObservedMetrics {
            error_rate: 0.0,
            p95_latency_ms: 100.0,
            crash_rate: 0.0,
            disruption_events: 0,
            health_check_failure_rate: 0.0,
            operator_triggered: true,
        };
        let fired = evaluate_rollback_triggers(&triggers, &metrics);
        assert!(fired.iter().any(|t| t.trigger_type == RollbackTriggerType::OperatorManual));
    }

    #[test]
    fn multiple_triggers_can_fire() {
        let triggers = standard_rollback_triggers();
        let metrics = ObservedMetrics {
            error_rate: 0.10,
            p95_latency_ms: 5000.0,
            crash_rate: 0.05,
            disruption_events: 10,
            health_check_failure_rate: 0.20,
            operator_triggered: false,
        };
        let fired = evaluate_rollback_triggers(&triggers, &metrics);
        assert!(fired.len() >= 4);
    }

    // --- Disruption budget tests ---

    #[test]
    fn zero_disruption_within_any_budget() {
        let acc = DisruptionAccounting::zero();
        assert!(acc.within_budget(&DisruptionBudget::production()));
        assert!(acc.within_budget(&DisruptionBudget::rehearsal()));
    }

    #[test]
    fn production_budget_stricter() {
        let prod = DisruptionBudget::production();
        let reh = DisruptionBudget::rehearsal();
        assert!(prod.max_latency_increase_ms <= reh.max_latency_increase_ms);
        assert!(prod.max_disruption_events <= reh.max_disruption_events);
        assert!(prod.max_recovery_time_ms <= reh.max_recovery_time_ms);
    }

    #[test]
    fn over_budget_latency() {
        let acc = DisruptionAccounting {
            latency_increase_ms: 200,
            error_rate_increase: 0.0,
            disruption_events: 0,
            recovery_time_ms: None,
        };
        assert!(!acc.within_budget(&DisruptionBudget::production())); // 200 > 100.
        assert!(acc.within_budget(&DisruptionBudget::rehearsal()));   // 200 < 500.
    }

    #[test]
    fn over_budget_recovery_time() {
        let acc = DisruptionAccounting {
            latency_increase_ms: 0,
            error_rate_increase: 0.0,
            disruption_events: 0,
            recovery_time_ms: Some(120_000), // 2 minutes.
        };
        assert!(!acc.within_budget(&DisruptionBudget::production())); // 120s > 60s.
        assert!(acc.within_budget(&DisruptionBudget::rehearsal()));   // 120s < 300s.
    }

    // --- Drill result tests ---

    #[test]
    fn drill_step_pass_and_fail() {
        let pass = DrillStep::pass("s1", "step one", 100);
        assert!(pass.success);
        assert!(pass.error.is_none());

        let fail = DrillStep::fail("s2", "step two", 200, "timeout");
        assert!(!fail.success);
        assert_eq!(fail.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn drill_step_observations() {
        let mut step = DrillStep::pass("s1", "test", 50);
        step.observe("latency nominal");
        step.observe("no errors");
        assert_eq!(step.observations.len(), 2);
    }

    #[test]
    fn drill_result_stats() {
        let result = DrillResult {
            drill_id: "DR-01".into(),
            drill_type: DrillType::Promotion,
            cohort_id: "canary".into(),
            started_at_ms: 1000,
            ended_at_ms: 5000,
            steps: vec![
                DrillStep::pass("s1", "prep", 500),
                DrillStep::pass("s2", "exec", 1000),
                DrillStep::fail("s3", "verify", 2000, "mismatch"),
            ],
            disruption: DisruptionAccounting::zero(),
            success: false,
            failure_reason: Some("verification failed".into()),
        };

        assert_eq!(result.total_elapsed_ms(), 4000);
        assert_eq!(result.steps_passed(), 2);
        assert_eq!(result.steps_failed(), 1);
        assert!((result.step_pass_rate() - 0.6667).abs() < 0.01);
    }

    // --- Rehearsal plan tests ---

    #[test]
    fn standard_plan_has_all_drill_types() {
        let plan = RehearsalPlan::standard();
        assert!(plan.drill_types.len() >= 4);
        assert!(plan.drill_types.contains(&DrillType::Promotion));
        assert!(plan.drill_types.contains(&DrillType::Rollback));
        assert!(plan.drill_types.contains(&DrillType::FailSafe));
    }

    #[test]
    fn production_plan_includes_full_rollout() {
        let plan = RehearsalPlan::production();
        assert!(plan.drill_types.contains(&DrillType::FullRollout));
    }

    #[test]
    fn plan_drill_count_calculation() {
        let plan = RehearsalPlan::standard();
        let rollback_cohorts = plan
            .cohorts
            .iter()
            .filter(|c| c.independent_rollback)
            .count();
        assert_eq!(plan.total_drill_count(), plan.drill_types.len() * rollback_cohorts);
    }

    // --- Rehearsal report tests ---

    #[test]
    fn report_all_pass_is_ready() {
        let drills = vec![
            DrillResult {
                drill_id: "D1".into(),
                drill_type: DrillType::Promotion,
                cohort_id: "canary".into(),
                started_at_ms: 0,
                ended_at_ms: 1000,
                steps: vec![DrillStep::pass("s1", "ok", 500)],
                disruption: DisruptionAccounting::zero(),
                success: true,
                failure_reason: None,
            },
            DrillResult {
                drill_id: "D2".into(),
                drill_type: DrillType::Rollback,
                cohort_id: "canary".into(),
                started_at_ms: 1000,
                ended_at_ms: 2000,
                steps: vec![DrillStep::pass("s1", "ok", 400)],
                disruption: DisruptionAccounting::zero(),
                success: true,
                failure_reason: None,
            },
        ];

        let report = RehearsalReport::from_drills(
            "plan-1",
            1000,
            drills,
            &DisruptionBudget::rehearsal(),
        );

        assert_eq!(report.verdict, RehearsalVerdict::Ready);
        assert_eq!(report.drills_passed(), 2);
        assert_eq!(report.drill_pass_rate(), 1.0);
    }

    #[test]
    fn report_fail_is_not_ready() {
        let drills = vec![DrillResult {
            drill_id: "D1".into(),
            drill_type: DrillType::FailSafe,
            cohort_id: "canary".into(),
            started_at_ms: 0,
            ended_at_ms: 5000,
            steps: vec![DrillStep::fail("s1", "emergency", 3000, "hung")],
            disruption: DisruptionAccounting::zero(),
            success: false,
            failure_reason: Some("emergency stop hung".into()),
        }];

        let report = RehearsalReport::from_drills(
            "plan-1",
            1000,
            drills,
            &DisruptionBudget::rehearsal(),
        );

        assert_eq!(report.verdict, RehearsalVerdict::NotReady);
        assert_eq!(report.drills_failed(), 1);
    }

    #[test]
    fn report_pass_but_over_budget_is_conditional() {
        let drills = vec![DrillResult {
            drill_id: "D1".into(),
            drill_type: DrillType::Promotion,
            cohort_id: "canary".into(),
            started_at_ms: 0,
            ended_at_ms: 2000,
            steps: vec![DrillStep::pass("s1", "ok", 1000)],
            disruption: DisruptionAccounting {
                latency_increase_ms: 600,   // Over 500ms rehearsal budget.
                error_rate_increase: 0.0,
                disruption_events: 0,
                recovery_time_ms: None,
            },
            success: true,
            failure_reason: None,
        }];

        let report = RehearsalReport::from_drills(
            "plan-1",
            1000,
            drills,
            &DisruptionBudget::rehearsal(),
        );

        assert_eq!(report.verdict, RehearsalVerdict::Conditional);
    }

    #[test]
    fn report_render_contains_details() {
        let drills = vec![DrillResult {
            drill_id: "D1".into(),
            drill_type: DrillType::Promotion,
            cohort_id: "canary".into(),
            started_at_ms: 0,
            ended_at_ms: 1000,
            steps: vec![DrillStep::pass("s1", "ok", 500)],
            disruption: DisruptionAccounting::zero(),
            success: true,
            failure_reason: None,
        }];

        let report = RehearsalReport::from_drills(
            "plan-1",
            1000,
            drills,
            &DisruptionBudget::rehearsal(),
        );

        let rendered = report.render_summary();
        assert!(rendered.contains("Rehearsal Report"));
        assert!(rendered.contains("Ready"));
        assert!(rendered.contains("PASS"));
        assert!(rendered.contains("D1"));
    }

    // --- Telemetry tests ---

    #[test]
    fn telemetry_empty_defaults() {
        let t = RehearsalTelemetry::new();
        assert_eq!(t.rehearsals_executed, 0);
        assert_eq!(t.overall_drill_pass_rate(), 0.0);
        assert_eq!(t.promotion_approval_rate(), 0.0);
    }

    #[test]
    fn telemetry_records_rehearsal() {
        let drills = vec![
            DrillResult {
                drill_id: "D1".into(),
                drill_type: DrillType::Promotion,
                cohort_id: "canary".into(),
                started_at_ms: 0,
                ended_at_ms: 1000,
                steps: vec![DrillStep::pass("s1", "ok", 500)],
                disruption: DisruptionAccounting::zero(),
                success: true,
                failure_reason: None,
            },
            DrillResult {
                drill_id: "D2".into(),
                drill_type: DrillType::Rollback,
                cohort_id: "canary".into(),
                started_at_ms: 1000,
                ended_at_ms: 2000,
                steps: vec![DrillStep::fail("s1", "fail", 800, "slow")],
                disruption: DisruptionAccounting::zero(),
                success: false,
                failure_reason: Some("slow rollback".into()),
            },
        ];

        let report = RehearsalReport::from_drills("plan-1", 1000, drills, &DisruptionBudget::rehearsal());
        let mut telem = RehearsalTelemetry::new();
        telem.record_rehearsal(&report);

        assert_eq!(telem.rehearsals_executed, 1);
        assert_eq!(telem.total_drills, 2);
        assert_eq!(telem.drills_passed, 1);
        assert_eq!(telem.overall_drill_pass_rate(), 0.5);

        let promo_rate = telem.drill_type_pass_rates.get("promotion").unwrap();
        assert_eq!(promo_rate, &(1, 1));
        let rb_rate = telem.drill_type_pass_rates.get("rollback").unwrap();
        assert_eq!(rb_rate, &(0, 1));
    }

    #[test]
    fn telemetry_records_promotion() {
        let criteria = PromotionCriteria::rehearsal();
        let good_metrics = SoakMetrics {
            soak_duration_ms: 120_000,
            pass_rate: 0.99,
            error_rate: 0.01,
            p95_latency_ms: 500,
            disruption_count: 0,
            human_approved: false,
        };
        let eval = evaluate_promotion(&criteria, &good_metrics);

        let mut telem = RehearsalTelemetry::new();
        telem.record_promotion(&eval);
        assert_eq!(telem.promotions_evaluated, 1);
        assert_eq!(telem.promotions_approved, 1);
        assert_eq!(telem.promotion_approval_rate(), 1.0);
    }

    #[test]
    fn telemetry_records_rollback_triggers() {
        let mut telem = RehearsalTelemetry::new();
        telem.record_rollback_triggers(3);
        assert_eq!(telem.rollback_triggers_fired, 3);
        telem.record_rollback_triggers(2);
        assert_eq!(telem.rollback_triggers_fired, 5);
    }

    // --- Disruption aggregation tests ---

    #[test]
    fn aggregation_takes_max_latency_and_sums_events() {
        let drills = vec![
            DrillResult {
                drill_id: "D1".into(),
                drill_type: DrillType::Promotion,
                cohort_id: "c1".into(),
                started_at_ms: 0,
                ended_at_ms: 1000,
                steps: vec![],
                disruption: DisruptionAccounting {
                    latency_increase_ms: 50,
                    error_rate_increase: 0.01,
                    disruption_events: 2,
                    recovery_time_ms: Some(5000),
                },
                success: true,
                failure_reason: None,
            },
            DrillResult {
                drill_id: "D2".into(),
                drill_type: DrillType::Rollback,
                cohort_id: "c1".into(),
                started_at_ms: 1000,
                ended_at_ms: 2000,
                steps: vec![],
                disruption: DisruptionAccounting {
                    latency_increase_ms: 100,
                    error_rate_increase: 0.03,
                    disruption_events: 3,
                    recovery_time_ms: Some(8000),
                },
                success: true,
                failure_reason: None,
            },
        ];

        let agg = aggregate_disruption(&drills);
        assert_eq!(agg.latency_increase_ms, 100); // Max.
        assert!((agg.error_rate_increase - 0.03).abs() < 0.001); // Max.
        assert_eq!(agg.disruption_events, 5); // Sum.
        assert_eq!(agg.recovery_time_ms, Some(8000)); // Max.
    }

    // --- Serde roundtrip tests ---

    #[test]
    fn rehearsal_plan_serde_roundtrip() {
        let plan = RehearsalPlan::standard();
        let json = serde_json::to_string(&plan).expect("serialize");
        let restored: RehearsalPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.plan_id, plan.plan_id);
        assert_eq!(restored.cohorts.len(), plan.cohorts.len());
    }

    #[test]
    fn drill_result_serde_roundtrip() {
        let result = DrillResult {
            drill_id: "D1".into(),
            drill_type: DrillType::FailSafe,
            cohort_id: "canary".into(),
            started_at_ms: 1000,
            ended_at_ms: 5000,
            steps: vec![DrillStep::pass("s1", "ok", 500)],
            disruption: DisruptionAccounting::zero(),
            success: true,
            failure_reason: None,
        };

        let json = serde_json::to_string(&result).expect("serialize");
        let restored: DrillResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.drill_id, "D1");
        assert_eq!(restored.drill_type, DrillType::FailSafe);
    }

    #[test]
    fn rehearsal_report_serde_roundtrip() {
        let drills = vec![DrillResult {
            drill_id: "D1".into(),
            drill_type: DrillType::Promotion,
            cohort_id: "canary".into(),
            started_at_ms: 0,
            ended_at_ms: 1000,
            steps: vec![DrillStep::pass("s1", "ok", 500)],
            disruption: DisruptionAccounting::zero(),
            success: true,
            failure_reason: None,
        }];

        let report = RehearsalReport::from_drills("plan-1", 1000, drills, &DisruptionBudget::rehearsal());
        let json = serde_json::to_string(&report).expect("serialize");
        let restored: RehearsalReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.plan_id, "plan-1");
        assert_eq!(restored.verdict, RehearsalVerdict::Ready);
    }

    // --- E2E lifecycle test ---

    #[test]
    fn e2e_rehearsal_lifecycle() {
        // 1. Create plan.
        let plan = RehearsalPlan::standard();
        let mut telem = RehearsalTelemetry::new();

        // 2. Execute drills for each cohort.
        let mut all_drills = Vec::new();
        for cohort in &plan.cohorts {
            if !cohort.independent_rollback {
                continue;
            }

            // Promotion drill.
            all_drills.push(DrillResult {
                drill_id: format!("DR-promo-{}", cohort.cohort_id),
                drill_type: DrillType::Promotion,
                cohort_id: cohort.cohort_id.clone(),
                started_at_ms: 0,
                ended_at_ms: 2000,
                steps: vec![
                    DrillStep::pass("prep", "prepare promotion", 300),
                    DrillStep::pass("exec", "execute promotion", 800),
                    DrillStep::pass("verify", "verify promotion", 500),
                ],
                disruption: DisruptionAccounting {
                    latency_increase_ms: 30,
                    error_rate_increase: 0.001,
                    disruption_events: 0,
                    recovery_time_ms: None,
                },
                success: true,
                failure_reason: None,
            });

            // Rollback drill.
            all_drills.push(DrillResult {
                drill_id: format!("DR-rollback-{}", cohort.cohort_id),
                drill_type: DrillType::Rollback,
                cohort_id: cohort.cohort_id.clone(),
                started_at_ms: 2000,
                ended_at_ms: 4000,
                steps: vec![
                    DrillStep::pass("trigger", "trigger rollback", 200),
                    DrillStep::pass("rollback", "execute rollback", 1000),
                    DrillStep::pass("verify", "verify rollback", 400),
                ],
                disruption: DisruptionAccounting {
                    latency_increase_ms: 50,
                    error_rate_increase: 0.002,
                    disruption_events: 1,
                    recovery_time_ms: Some(30_000),
                },
                success: true,
                failure_reason: None,
            });
        }

        // 3. Create report.
        let report = RehearsalReport::from_drills(
            &plan.plan_id,
            1000,
            all_drills,
            &plan.disruption_budget,
        );

        assert_eq!(report.verdict, RehearsalVerdict::Ready);
        assert!(report.drills_passed() >= 4);

        // 4. Record telemetry.
        telem.record_rehearsal(&report);
        assert_eq!(telem.rehearsals_executed, 1);
        assert_eq!(telem.rehearsals_ready, 1);
        assert!(telem.overall_drill_pass_rate() > 0.99);

        // 5. Check rollback triggers (none should fire for healthy metrics).
        let metrics = ObservedMetrics {
            error_rate: 0.001,
            p95_latency_ms: 200.0,
            crash_rate: 0.0,
            disruption_events: 0,
            health_check_failure_rate: 0.0,
            operator_triggered: false,
        };
        let fired = evaluate_rollback_triggers(&plan.rollback_triggers, &metrics);
        assert!(fired.is_empty());

        // 6. Evaluate promotion.
        let soak = SoakMetrics {
            soak_duration_ms: 120_000,
            pass_rate: 0.99,
            error_rate: 0.01,
            p95_latency_ms: 300,
            disruption_count: 0,
            human_approved: false,
        };
        let eval = evaluate_promotion(&plan.promotion_criteria, &soak);
        assert!(eval.approved);
        telem.record_promotion(&eval);
        assert_eq!(telem.promotions_approved, 1);
    }

    #[test]
    fn e2e_failed_rehearsal_lifecycle() {
        let plan = RehearsalPlan::production();
        let mut telem = RehearsalTelemetry::new();

        // Simulate a failed fail-safe drill.
        let drills = vec![
            DrillResult {
                drill_id: "DR-promo".into(),
                drill_type: DrillType::Promotion,
                cohort_id: "canary".into(),
                started_at_ms: 0,
                ended_at_ms: 2000,
                steps: vec![DrillStep::pass("s1", "ok", 500)],
                disruption: DisruptionAccounting::zero(),
                success: true,
                failure_reason: None,
            },
            DrillResult {
                drill_id: "DR-failsafe".into(),
                drill_type: DrillType::FailSafe,
                cohort_id: "canary".into(),
                started_at_ms: 2000,
                ended_at_ms: 10000,
                steps: vec![
                    DrillStep::pass("emergency-trigger", "trigger e-stop", 500),
                    DrillStep::fail("emergency-verify", "verify e-stop effect", 5000, "panes still running"),
                ],
                disruption: DisruptionAccounting {
                    latency_increase_ms: 200,
                    error_rate_increase: 0.01,
                    disruption_events: 3,
                    recovery_time_ms: Some(120_000), // Over production budget.
                },
                success: false,
                failure_reason: Some("emergency stop verification failed".into()),
            },
        ];

        let report = RehearsalReport::from_drills(
            &plan.plan_id,
            1000,
            drills,
            &plan.disruption_budget,
        );

        assert_eq!(report.verdict, RehearsalVerdict::NotReady);
        assert_eq!(report.drills_failed(), 1);
        assert!(report.notes.iter().any(|n| n.contains("Failed drills")));

        telem.record_rehearsal(&report);
        assert_eq!(telem.rehearsals_not_ready, 1);
        assert!(telem.overall_drill_pass_rate() < 1.0);

        // Rollback triggers should fire on degraded metrics.
        let metrics = ObservedMetrics {
            error_rate: 0.10,
            p95_latency_ms: 5000.0,
            crash_rate: 0.0,
            disruption_events: 5,
            health_check_failure_rate: 0.0,
            operator_triggered: false,
        };
        let fired = evaluate_rollback_triggers(&plan.rollback_triggers, &metrics);
        assert!(fired.len() >= 2);
        telem.record_rollback_triggers(fired.len() as u64);
        assert!(telem.rollback_triggers_fired >= 2);
    }

    // --- Drill type label uniqueness ---

    #[test]
    fn drill_type_labels_unique() {
        let types = [
            DrillType::Promotion,
            DrillType::Rollback,
            DrillType::FailSafe,
            DrillType::FullRollout,
            DrillType::PartialFailureRecovery,
        ];
        let labels: Vec<&str> = types.iter().map(|t| t.label()).collect();
        let mut deduped = labels.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(labels.len(), deduped.len());
    }
}
