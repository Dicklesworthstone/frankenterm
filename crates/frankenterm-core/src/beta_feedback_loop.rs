//! Structured beta feedback loop with user-perceived smoothness telemetry (ft-1u90p.8.7).
//!
//! Composes existing subsystems into a controlled beta validation cycle:
//!
//! ```text
//! CohortRegistry
//!   ├── BetaCohort[] (named groups with rollout order)
//!   └── member → cohort mapping
//!
//! FeedbackCollector
//!   ├── QualitativeFeedback (category, severity, description)
//!   ├── SmoothnessObservation (QoE metrics per member)
//!   └── correlation: feedback ↔ telemetry timestamp window
//!
//! BetaEvaluator
//!   ├── per-cohort SLO conformance (smoothness, latency, friction)
//!   ├── qualitative signal aggregation (NPS-style scoring)
//!   └── PromotionDecision (Promote / Hold / Rollback)
//!
//! BetaLoopController
//!   ├── advance_stage() — gate-checked stage transitions
//!   ├── record_feedback() / record_observation() — data ingest
//!   ├── evaluate() → StageEvaluation with decision + evidence
//!   └── snapshot() → BetaLoopSnapshot (serde-roundtrippable)
//! ```
//!
//! # Decision Rubric
//!
//! Promotion requires ALL of:
//! - Smoothness SLO met at configured percentile (default: p50 ≥ 0.90)
//! - Feedback NPS ≥ threshold (default: 0 — net neutral or better)
//! - No critical friction points unresolved
//! - Minimum observation count per cohort
//! - Minimum qualitative feedback count per cohort
//!
//! Rollback triggers on ANY of:
//! - Smoothness SLO breached by > 2× error budget
//! - NPS drops below rollback threshold (default: −30)
//! - Critical friction count exceeds limit (default: 3)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Rollout Stage ──────────────────────────────────────────────────────

/// Stages of the beta feedback loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum BetaStage {
    /// Collecting baseline telemetry, no user exposure.
    Baseline,
    /// Internal dogfooding cohort.
    InternalBeta,
    /// Small external cohort with active feedback collection.
    ClosedBeta,
    /// Larger cohort, monitoring only.
    OpenBeta,
    /// General availability — loop complete.
    GeneralAvailability,
}

impl BetaStage {
    /// Numeric rank for ordering comparisons.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Baseline => 0,
            Self::InternalBeta => 1,
            Self::ClosedBeta => 2,
            Self::OpenBeta => 3,
            Self::GeneralAvailability => 4,
        }
    }

    /// Next stage, if any.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Baseline => Some(Self::InternalBeta),
            Self::InternalBeta => Some(Self::ClosedBeta),
            Self::ClosedBeta => Some(Self::OpenBeta),
            Self::OpenBeta => Some(Self::GeneralAvailability),
            Self::GeneralAvailability => None,
        }
    }

    /// Previous stage, if any.
    #[must_use]
    pub fn prev(self) -> Option<Self> {
        match self {
            Self::Baseline => None,
            Self::InternalBeta => Some(Self::Baseline),
            Self::ClosedBeta => Some(Self::InternalBeta),
            Self::OpenBeta => Some(Self::ClosedBeta),
            Self::GeneralAvailability => Some(Self::OpenBeta),
        }
    }
}

// ── Beta Cohort ────────────────────────────────────────────────────────

/// A named cohort participating in the beta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BetaCohort {
    /// Cohort name (e.g., "internal-team", "early-adopters").
    pub name: String,
    /// Stage at which this cohort is activated.
    pub activation_stage: BetaStage,
    /// Member identifiers (agent IDs, user IDs, pane IDs).
    pub members: Vec<String>,
}

impl BetaCohort {
    /// Create a new cohort.
    #[must_use]
    pub fn new(name: impl Into<String>, activation_stage: BetaStage) -> Self {
        Self {
            name: name.into(),
            activation_stage,
            members: Vec::new(),
        }
    }

    /// Add a member.
    pub fn add_member(&mut self, member_id: impl Into<String>) {
        self.members.push(member_id.into());
    }

    /// Number of members.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Whether this cohort is active at the given stage.
    #[must_use]
    pub fn is_active_at(&self, stage: BetaStage) -> bool {
        stage.rank() >= self.activation_stage.rank()
    }
}

// ── Feedback Types ─────────────────────────────────────────────────────

/// Category of qualitative user feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackCategory {
    /// Perceived smoothness / responsiveness.
    Smoothness,
    /// Visual glitches or rendering artifacts.
    VisualGlitch,
    /// Unexpected behavior or confusion.
    Confusion,
    /// Workflow disruption.
    WorkflowDisruption,
    /// Performance regression.
    PerformanceRegression,
    /// Positive experience — things working well.
    Positive,
    /// General / uncategorized.
    General,
}

/// Severity of a feedback item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSeverity {
    /// Informational — no impact.
    Info,
    /// Minor annoyance.
    Minor,
    /// Noticeable impact on workflow.
    Moderate,
    /// Blocks or seriously degrades workflow.
    Critical,
}

/// A single qualitative feedback entry from a beta participant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualitativeFeedback {
    /// Who submitted this feedback.
    pub member_id: String,
    /// Feedback category.
    pub category: FeedbackCategory,
    /// Severity.
    pub severity: FeedbackSeverity,
    /// Free-text description.
    pub description: String,
    /// Timestamp (ms since epoch).
    pub timestamp_ms: u64,
    /// NPS-style score: −100 to +100 (detractor < 0, promoter > 0).
    pub nps_score: i32,
}

/// A smoothness observation from telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmoothnessObservation {
    /// Member producing this observation.
    pub member_id: String,
    /// Smoothness score (0.0..=1.0, 1.0 = perfect).
    pub smoothness: f64,
    /// Input-to-paint latency in microseconds.
    pub input_to_paint_us: Option<u64>,
    /// Frame jitter in microseconds.
    pub frame_jitter_us: Option<u64>,
    /// Keystroke echo latency in microseconds.
    pub keystroke_echo_us: Option<u64>,
    /// Timestamp (ms since epoch).
    pub timestamp_ms: u64,
}

// ── Evaluation Types ───────────────────────────────────────────────────

/// Configuration for the beta feedback loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BetaLoopConfig {
    /// Smoothness SLO target (default: 0.90 at p50).
    pub smoothness_target: f64,
    /// Smoothness percentile (default: 0.50).
    pub smoothness_percentile: f64,
    /// Minimum observations per cohort before evaluation.
    pub min_observations_per_cohort: usize,
    /// Minimum qualitative feedback items per cohort before promotion.
    pub min_feedback_per_cohort: usize,
    /// NPS threshold for promotion (default: 0).
    pub promotion_nps_threshold: i32,
    /// NPS threshold triggering rollback (default: −30).
    pub rollback_nps_threshold: i32,
    /// Max critical friction points before rollback (default: 3).
    pub max_critical_friction: usize,
    /// Error budget multiplier triggering rollback (default: 2.0).
    pub rollback_budget_multiplier: f64,
    /// Maximum feedback entries retained per cohort.
    pub max_feedback_per_cohort: usize,
    /// Maximum observations retained per cohort.
    pub max_observations_per_cohort: usize,
}

impl Default for BetaLoopConfig {
    fn default() -> Self {
        Self {
            smoothness_target: 0.90,
            smoothness_percentile: 0.50,
            min_observations_per_cohort: 30,
            min_feedback_per_cohort: 10,
            promotion_nps_threshold: 0,
            rollback_nps_threshold: -30,
            max_critical_friction: 3,
            rollback_budget_multiplier: 2.0,
            max_feedback_per_cohort: 10_000,
            max_observations_per_cohort: 100_000,
        }
    }
}

/// Decision for stage transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionDecision {
    /// Advance to the next stage.
    Promote,
    /// Stay at current stage — insufficient data or marginal results.
    Hold,
    /// Roll back to previous stage.
    Rollback,
}

/// Severity for rollout anomalies tracked alongside beta feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AnomalySeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Lifecycle state for a tracked rollout anomaly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyStatus {
    Open,
    Investigating,
    Mitigated,
    Closed,
}

/// Structured anomaly ledger entry for rollout blockers and regressions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BetaAnomaly {
    /// Stable anomaly identifier.
    pub anomaly_id: String,
    /// Short category code (for example `A1`).
    pub category_code: String,
    /// Short human-readable title.
    pub title: String,
    /// Severity of the anomaly.
    pub severity: AnomalySeverity,
    /// Current lifecycle state.
    pub status: AnomalyStatus,
    /// Whether this anomaly forces `GO`, `HOLD`, or `ROLLBACK`.
    pub blocking_decision: PromotionDecision,
    /// Current triage owner.
    pub triage_owner: String,
    /// Current remediation owner.
    pub remediation_owner: String,
    /// When the anomaly was opened.
    pub opened_at_ms: u64,
    /// When the anomaly was last updated.
    pub last_updated_at_ms: u64,
    /// Freeform summary of the issue.
    pub summary: String,
    /// Linked qualitative feedback ids.
    pub linked_feedback_ids: Vec<String>,
    /// Linked evidence artifacts.
    pub linked_artifacts: Vec<String>,
    /// Current close-loop status string.
    pub close_loop_status: String,
    /// Evidence supporting the current close-loop status.
    pub close_loop_evidence: Vec<String>,
    /// Tracking issue ids for cross-linking into beads/docs.
    pub tracking_issue_ids: Vec<String>,
}

impl BetaAnomaly {
    /// Whether the anomaly is still active for rollout decisions.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status != AnomalyStatus::Closed
    }
}

/// Reason for a promotion decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionReason {
    /// Short code for programmatic consumption.
    pub code: String,
    /// Human-readable explanation.
    pub explanation: String,
}

/// Per-cohort evaluation summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CohortEvaluation {
    /// Cohort name.
    pub cohort_name: String,
    /// Number of smoothness observations.
    pub observation_count: usize,
    /// Number of feedback entries.
    pub feedback_count: usize,
    /// Smoothness at configured percentile.
    pub smoothness_at_percentile: Option<f64>,
    /// Mean NPS score.
    pub mean_nps: Option<f64>,
    /// Count of critical friction points.
    pub critical_friction_count: usize,
    /// Whether this cohort meets promotion criteria.
    pub meets_criteria: bool,
}

/// Full stage evaluation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageEvaluation {
    /// Current stage.
    pub stage: BetaStage,
    /// Decision.
    pub decision: PromotionDecision,
    /// Reasons supporting the decision.
    pub reasons: Vec<DecisionReason>,
    /// Per-cohort evaluations.
    pub cohort_evaluations: Vec<CohortEvaluation>,
    /// Timestamp of evaluation.
    pub evaluated_at_ms: u64,
}

/// Serde-roundtrippable snapshot of the entire beta loop state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BetaLoopSnapshot {
    /// Current stage.
    pub stage: BetaStage,
    /// Total feedback entries.
    pub total_feedback: u64,
    /// Total smoothness observations.
    pub total_observations: u64,
    /// Number of stage transitions.
    pub transition_count: u32,
    /// Number of evaluations performed.
    pub evaluation_count: u32,
    /// Last decision, if any.
    pub last_decision: Option<PromotionDecision>,
    /// Per-cohort observation counts.
    pub cohort_observation_counts: HashMap<String, u64>,
    /// Per-cohort feedback counts.
    pub cohort_feedback_counts: HashMap<String, u64>,
    /// Number of currently active anomalies.
    #[serde(default)]
    pub active_anomaly_count: u64,
    /// Full anomaly ledger snapshot.
    #[serde(default)]
    pub anomalies: Vec<BetaAnomaly>,
}

// ── Controller ─────────────────────────────────────────────────────────

/// The main beta feedback loop controller.
///
/// Manages cohorts, collects feedback and telemetry observations,
/// evaluates promotion/hold/rollback decisions, and tracks stage transitions.
pub struct BetaLoopController {
    config: BetaLoopConfig,
    stage: BetaStage,
    cohorts: Vec<BetaCohort>,
    /// member_id → cohort index
    member_cohort: HashMap<String, usize>,
    /// cohort index → feedback entries
    feedback: HashMap<usize, Vec<QualitativeFeedback>>,
    /// cohort index → smoothness observations
    observations: HashMap<usize, Vec<SmoothnessObservation>>,
    /// Structured anomaly ledger for HOLD/ROLLBACK blockers.
    anomalies: Vec<BetaAnomaly>,
    transition_count: u32,
    evaluation_count: u32,
    last_decision: Option<PromotionDecision>,
}

impl BetaLoopController {
    /// Create a new controller with given config and cohorts.
    #[must_use]
    pub fn new(config: BetaLoopConfig, cohorts: Vec<BetaCohort>) -> Self {
        let mut member_cohort = HashMap::new();
        for (idx, cohort) in cohorts.iter().enumerate() {
            for member in &cohort.members {
                member_cohort.insert(member.clone(), idx);
            }
        }
        Self {
            config,
            stage: BetaStage::Baseline,
            cohorts,
            member_cohort,
            feedback: HashMap::new(),
            observations: HashMap::new(),
            anomalies: Vec::new(),
            transition_count: 0,
            evaluation_count: 0,
            last_decision: None,
        }
    }

    /// Current stage.
    #[must_use]
    pub fn stage(&self) -> BetaStage {
        self.stage
    }

    /// Number of registered cohorts.
    #[must_use]
    pub fn cohort_count(&self) -> usize {
        self.cohorts.len()
    }

    /// Number of active cohorts at the current stage.
    #[must_use]
    pub fn active_cohort_count(&self) -> usize {
        self.cohorts
            .iter()
            .filter(|c| c.is_active_at(self.stage))
            .count()
    }

    /// Record qualitative feedback from a beta participant.
    pub fn record_feedback(&mut self, feedback: QualitativeFeedback) {
        let cohort_idx = self.member_cohort.get(&feedback.member_id).copied();
        if let Some(idx) = cohort_idx {
            let entries = self.feedback.entry(idx).or_default();
            if entries.len() < self.config.max_feedback_per_cohort {
                entries.push(feedback);
            }
        }
    }

    /// Record a smoothness telemetry observation.
    pub fn record_observation(&mut self, observation: SmoothnessObservation) {
        let cohort_idx = self.member_cohort.get(&observation.member_id).copied();
        if let Some(idx) = cohort_idx {
            let entries = self.observations.entry(idx).or_default();
            if entries.len() < self.config.max_observations_per_cohort {
                entries.push(observation);
            }
        }
    }

    /// Upsert an anomaly in the rollout ledger.
    pub fn record_anomaly(&mut self, anomaly: BetaAnomaly) {
        if let Some(existing) = self
            .anomalies
            .iter_mut()
            .find(|existing| existing.anomaly_id == anomaly.anomaly_id)
        {
            *existing = anomaly;
        } else {
            self.anomalies.push(anomaly);
            self.anomalies
                .sort_by(|left, right| left.anomaly_id.cmp(&right.anomaly_id));
        }
    }

    /// Mark an anomaly as closed and append any new close-loop evidence.
    pub fn resolve_anomaly(
        &mut self,
        anomaly_id: &str,
        resolved_at_ms: u64,
        close_loop_evidence: Vec<String>,
    ) -> bool {
        let Some(anomaly) = self
            .anomalies
            .iter_mut()
            .find(|anomaly| anomaly.anomaly_id == anomaly_id)
        else {
            return false;
        };

        anomaly.status = AnomalyStatus::Closed;
        anomaly.last_updated_at_ms = resolved_at_ms;
        anomaly.close_loop_status = "closed".into();
        for evidence in close_loop_evidence {
            if !anomaly
                .close_loop_evidence
                .iter()
                .any(|entry| entry == &evidence)
            {
                anomaly.close_loop_evidence.push(evidence);
            }
        }
        true
    }

    /// Total feedback entries across all cohorts.
    #[must_use]
    pub fn total_feedback(&self) -> u64 {
        self.feedback.values().map(|v| v.len() as u64).sum()
    }

    /// Total observations across all cohorts.
    #[must_use]
    pub fn total_observations(&self) -> u64 {
        self.observations.values().map(|v| v.len() as u64).sum()
    }

    /// Number of active anomalies currently affecting rollout decisions.
    #[must_use]
    pub fn active_anomaly_count(&self) -> usize {
        self.anomalies
            .iter()
            .filter(|anomaly| anomaly.is_active())
            .count()
    }

    /// Active anomalies currently affecting rollout decisions.
    #[must_use]
    pub fn active_anomalies(&self) -> Vec<&BetaAnomaly> {
        self.anomalies
            .iter()
            .filter(|anomaly| anomaly.is_active())
            .collect()
    }

    /// Evaluate the current stage and produce a promotion decision.
    #[must_use]
    pub fn evaluate(&mut self, now_ms: u64) -> StageEvaluation {
        self.evaluation_count += 1;
        let mut cohort_evals = Vec::new();
        let mut reasons = Vec::new();
        let mut any_rollback = false;
        let mut all_meet_criteria = true;
        let mut any_data = false;
        let mut hold_anomalies = Vec::new();

        for anomaly in self.active_anomalies() {
            match anomaly.blocking_decision {
                PromotionDecision::Rollback => {
                    any_rollback = true;
                    reasons.push(DecisionReason {
                        code: "anomaly_forces_rollback".into(),
                        explanation: format!(
                            "Anomaly '{}' ({}) forces rollback; triage owner '{}'",
                            anomaly.anomaly_id, anomaly.title, anomaly.triage_owner,
                        ),
                    });
                }
                PromotionDecision::Hold => hold_anomalies.push(anomaly),
                PromotionDecision::Promote => {}
            }
        }

        for (idx, cohort) in self.cohorts.iter().enumerate() {
            if !cohort.is_active_at(self.stage) {
                continue;
            }

            let obs = self
                .observations
                .get(&idx)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let fb = self.feedback.get(&idx).map(Vec::as_slice).unwrap_or(&[]);

            let smoothness_at_percentile =
                percentile_smoothness(obs, self.config.smoothness_percentile);
            let mean_nps = if fb.is_empty() {
                None
            } else {
                let sum: i64 = fb.iter().map(|f| f.nps_score as i64).sum();
                Some(sum as f64 / fb.len() as f64)
            };
            let critical_friction_count = fb
                .iter()
                .filter(|f| f.severity == FeedbackSeverity::Critical)
                .count();

            let has_enough_obs = obs.len() >= self.config.min_observations_per_cohort;
            let has_enough_feedback = fb.len() >= self.config.min_feedback_per_cohort;
            any_data = any_data || !obs.is_empty() || !fb.is_empty();

            // Check promotion criteria
            let smoothness_ok = smoothness_at_percentile
                .map(|s| s >= self.config.smoothness_target)
                .unwrap_or(false);
            let nps_ok = mean_nps
                .map(|n| n >= self.config.promotion_nps_threshold as f64)
                .unwrap_or(true); // No feedback = neutral
            let friction_ok = critical_friction_count <= self.config.max_critical_friction;
            let meets =
                has_enough_obs && has_enough_feedback && smoothness_ok && nps_ok && friction_ok;

            if !meets {
                all_meet_criteria = false;
            }

            // Check rollback triggers
            if let Some(s) = smoothness_at_percentile {
                let shortfall = self.config.smoothness_target - s;
                let budget = 1.0 - self.config.smoothness_target;
                if budget > 0.0 && shortfall > budget * self.config.rollback_budget_multiplier {
                    any_rollback = true;
                    reasons.push(DecisionReason {
                        code: "smoothness_budget_exceeded".into(),
                        explanation: format!(
                            "Cohort '{}': smoothness {:.3} breaches budget by {:.1}× (target {:.2})",
                            cohort.name,
                            s,
                            shortfall / budget,
                            self.config.smoothness_target,
                        ),
                    });
                }
            }

            if let Some(nps) = mean_nps {
                if nps < self.config.rollback_nps_threshold as f64 {
                    any_rollback = true;
                    reasons.push(DecisionReason {
                        code: "nps_below_rollback".into(),
                        explanation: format!(
                            "Cohort '{}': NPS {:.1} below rollback threshold {}",
                            cohort.name, nps, self.config.rollback_nps_threshold,
                        ),
                    });
                }
            }

            if critical_friction_count > self.config.max_critical_friction {
                any_rollback = true;
                reasons.push(DecisionReason {
                    code: "critical_friction_exceeded".into(),
                    explanation: format!(
                        "Cohort '{}': {} critical friction points (max {})",
                        cohort.name, critical_friction_count, self.config.max_critical_friction,
                    ),
                });
            }

            cohort_evals.push(CohortEvaluation {
                cohort_name: cohort.name.clone(),
                observation_count: obs.len(),
                feedback_count: fb.len(),
                smoothness_at_percentile,
                mean_nps,
                critical_friction_count,
                meets_criteria: meets,
            });
        }

        let decision = if any_rollback {
            PromotionDecision::Rollback
        } else if all_meet_criteria && any_data && hold_anomalies.is_empty() {
            PromotionDecision::Promote
        } else {
            for anomaly in &hold_anomalies {
                reasons.push(DecisionReason {
                    code: "anomaly_forces_hold".into(),
                    explanation: format!(
                        "Anomaly '{}' ({}) forces hold; remediation owner '{}'",
                        anomaly.anomaly_id, anomaly.title, anomaly.remediation_owner,
                    ),
                });
            }
            if !any_data {
                reasons.push(DecisionReason {
                    code: "insufficient_data".into(),
                    explanation: "No active cohorts have data yet".into(),
                });
            } else {
                for eval in &cohort_evals {
                    if !eval.meets_criteria {
                        if eval.observation_count < self.config.min_observations_per_cohort {
                            reasons.push(DecisionReason {
                                code: "insufficient_observations".into(),
                                explanation: format!(
                                    "Cohort '{}': {}/{} observations",
                                    eval.cohort_name,
                                    eval.observation_count,
                                    self.config.min_observations_per_cohort,
                                ),
                            });
                        }
                        if eval.feedback_count < self.config.min_feedback_per_cohort {
                            reasons.push(DecisionReason {
                                code: "insufficient_feedback".into(),
                                explanation: format!(
                                    "Cohort '{}': {}/{} feedback items",
                                    eval.cohort_name,
                                    eval.feedback_count,
                                    self.config.min_feedback_per_cohort,
                                ),
                            });
                        }
                    }
                }
            }
            PromotionDecision::Hold
        };

        self.last_decision = Some(decision);

        StageEvaluation {
            stage: self.stage,
            decision,
            reasons,
            cohort_evaluations: cohort_evals,
            evaluated_at_ms: now_ms,
        }
    }

    /// Attempt to advance to the next stage.
    ///
    /// Returns `true` if the transition succeeded, `false` if already at GA
    /// or if the last evaluation was not `Promote`.
    pub fn advance_stage(&mut self) -> bool {
        if self.last_decision != Some(PromotionDecision::Promote) {
            return false;
        }
        match self.stage.next() {
            Some(next) => {
                self.stage = next;
                self.transition_count += 1;
                self.last_decision = None; // Require fresh evaluation before next advance
                true
            }
            None => false,
        }
    }

    /// Roll back to the previous stage.
    ///
    /// Returns `true` if rollback succeeded, `false` if already at Baseline.
    pub fn rollback_stage(&mut self) -> bool {
        match self.stage.prev() {
            Some(prev) => {
                self.stage = prev;
                self.transition_count += 1;
                self.last_decision = None; // Require fresh evaluation before next advance
                true
            }
            None => false,
        }
    }

    /// Reset to Baseline, clearing all collected data.
    pub fn reset(&mut self) {
        self.stage = BetaStage::Baseline;
        self.feedback.clear();
        self.observations.clear();
        self.anomalies.clear();
        self.transition_count = 0;
        self.evaluation_count = 0;
        self.last_decision = None;
    }

    /// Produce a serde-roundtrippable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> BetaLoopSnapshot {
        let mut cohort_observation_counts = HashMap::new();
        let mut cohort_feedback_counts = HashMap::new();
        for (idx, cohort) in self.cohorts.iter().enumerate() {
            let obs_count = self
                .observations
                .get(&idx)
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            let fb_count = self.feedback.get(&idx).map(|v| v.len() as u64).unwrap_or(0);
            cohort_observation_counts.insert(cohort.name.clone(), obs_count);
            cohort_feedback_counts.insert(cohort.name.clone(), fb_count);
        }
        BetaLoopSnapshot {
            stage: self.stage,
            total_feedback: self.total_feedback(),
            total_observations: self.total_observations(),
            transition_count: self.transition_count,
            evaluation_count: self.evaluation_count,
            last_decision: self.last_decision,
            cohort_observation_counts,
            cohort_feedback_counts,
            active_anomaly_count: self.active_anomaly_count() as u64,
            anomalies: self.anomalies.clone(),
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Compute the smoothness value at a given percentile from observations.
fn percentile_smoothness(observations: &[SmoothnessObservation], percentile: f64) -> Option<f64> {
    if observations.is_empty() {
        return None;
    }
    let mut values: Vec<f64> = observations.iter().map(|o| o.smoothness).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((percentile * (values.len() as f64 - 1.0)).round() as usize).min(values.len() - 1);
    Some(values[idx])
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> BetaLoopConfig {
        BetaLoopConfig {
            min_observations_per_cohort: 5,
            min_feedback_per_cohort: 3,
            max_feedback_per_cohort: 100,
            max_observations_per_cohort: 1000,
            ..Default::default()
        }
    }

    fn make_cohorts() -> Vec<BetaCohort> {
        let mut internal = BetaCohort::new("internal", BetaStage::InternalBeta);
        internal.add_member("alice");
        internal.add_member("bob");

        let mut closed = BetaCohort::new("early-adopters", BetaStage::ClosedBeta);
        closed.add_member("carol");
        closed.add_member("dave");

        vec![internal, closed]
    }

    fn good_observation(member: &str, ts: u64) -> SmoothnessObservation {
        SmoothnessObservation {
            member_id: member.into(),
            smoothness: 0.95,
            input_to_paint_us: Some(5000),
            frame_jitter_us: Some(1000),
            keystroke_echo_us: Some(3000),
            timestamp_ms: ts,
        }
    }

    fn bad_observation(member: &str, ts: u64) -> SmoothnessObservation {
        SmoothnessObservation {
            member_id: member.into(),
            smoothness: 0.50,
            input_to_paint_us: Some(50000),
            frame_jitter_us: Some(20000),
            keystroke_echo_us: Some(30000),
            timestamp_ms: ts,
        }
    }

    fn positive_feedback(member: &str, ts: u64) -> QualitativeFeedback {
        QualitativeFeedback {
            member_id: member.into(),
            category: FeedbackCategory::Positive,
            severity: FeedbackSeverity::Info,
            description: "Feels smooth and responsive".into(),
            timestamp_ms: ts,
            nps_score: 40,
        }
    }

    fn negative_feedback(member: &str, ts: u64) -> QualitativeFeedback {
        QualitativeFeedback {
            member_id: member.into(),
            category: FeedbackCategory::PerformanceRegression,
            severity: FeedbackSeverity::Critical,
            description: "Terminal freezes during resize".into(),
            timestamp_ms: ts,
            nps_score: -50,
        }
    }

    fn hold_anomaly(anomaly_id: &str, ts: u64) -> BetaAnomaly {
        BetaAnomaly {
            anomaly_id: anomaly_id.into(),
            category_code: "A1".into(),
            title: "Insufficient sample coverage".into(),
            severity: AnomalySeverity::High,
            status: AnomalyStatus::Open,
            blocking_decision: PromotionDecision::Hold,
            triage_owner: "resize-rollout-ops".into(),
            remediation_owner: "beta-program".into(),
            opened_at_ms: ts,
            last_updated_at_ms: ts,
            summary: "Awaiting enough real-user samples.".into(),
            linked_feedback_ids: vec!["checkpoint-fixture-only".into()],
            linked_artifacts: vec!["evidence/wa-1u90p.8.7/cohort_daily_summary.json".into()],
            close_loop_status: "awaiting_real_user_cohort_ingest".into(),
            close_loop_evidence: vec![
                "evidence/wa-1u90p.8.7/decision_checkpoint_20260314.md".into(),
            ],
            tracking_issue_ids: vec!["ft-1u90p.8.7".into()],
        }
    }

    fn rollback_anomaly(anomaly_id: &str, ts: u64) -> BetaAnomaly {
        BetaAnomaly {
            anomaly_id: anomaly_id.into(),
            category_code: "A4".into(),
            title: "Repeated resize regression".into(),
            severity: AnomalySeverity::Critical,
            status: AnomalyStatus::Investigating,
            blocking_decision: PromotionDecision::Rollback,
            triage_owner: "resize-rollout-ops".into(),
            remediation_owner: "rendering-team".into(),
            opened_at_ms: ts,
            last_updated_at_ms: ts,
            summary: "Critical resize regression remains unresolved.".into(),
            linked_feedback_ids: vec!["feedback-critical-1".into()],
            linked_artifacts: vec!["evidence/wa-1u90p.8.7/decision_checkpoint_20260314.md".into()],
            close_loop_status: "investigating".into(),
            close_loop_evidence: vec!["tests/e2e/logs/ft_1u90p_8_7_20260314_002836.jsonl".into()],
            tracking_issue_ids: vec!["ft-1u90p.8.7".into()],
        }
    }

    // ── Stage tests ────────────────────────────────────────────────────

    #[test]
    fn stage_ordering() {
        assert!(BetaStage::Baseline.rank() < BetaStage::InternalBeta.rank());
        assert!(BetaStage::InternalBeta.rank() < BetaStage::ClosedBeta.rank());
        assert!(BetaStage::ClosedBeta.rank() < BetaStage::OpenBeta.rank());
        assert!(BetaStage::OpenBeta.rank() < BetaStage::GeneralAvailability.rank());
    }

    #[test]
    fn stage_next_prev_roundtrip() {
        let mut stage = BetaStage::Baseline;
        let mut count = 0;
        while let Some(next) = stage.next() {
            assert_eq!(next.prev(), Some(stage));
            stage = next;
            count += 1;
        }
        assert_eq!(count, 4);
        assert_eq!(stage, BetaStage::GeneralAvailability);
    }

    #[test]
    fn stage_serde_roundtrip() {
        for stage in [
            BetaStage::Baseline,
            BetaStage::InternalBeta,
            BetaStage::ClosedBeta,
            BetaStage::OpenBeta,
            BetaStage::GeneralAvailability,
        ] {
            let json = serde_json::to_string(&stage).unwrap();
            let rt: BetaStage = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, stage);
        }
    }

    #[test]
    fn stage_snake_case_names() {
        let json = serde_json::to_string(&BetaStage::InternalBeta).unwrap();
        assert_eq!(json, "\"internal_beta\"");
        let json = serde_json::to_string(&BetaStage::GeneralAvailability).unwrap();
        assert_eq!(json, "\"general_availability\"");
    }

    // ── Cohort tests ───────────────────────────────────────────────────

    #[test]
    fn cohort_activation() {
        let cohort = BetaCohort::new("internal", BetaStage::InternalBeta);
        assert!(!cohort.is_active_at(BetaStage::Baseline));
        assert!(cohort.is_active_at(BetaStage::InternalBeta));
        assert!(cohort.is_active_at(BetaStage::ClosedBeta));
        assert!(cohort.is_active_at(BetaStage::GeneralAvailability));
    }

    #[test]
    fn cohort_member_tracking() {
        let mut cohort = BetaCohort::new("test", BetaStage::Baseline);
        assert_eq!(cohort.member_count(), 0);
        cohort.add_member("alice");
        cohort.add_member("bob");
        assert_eq!(cohort.member_count(), 2);
    }

    // ── Controller basics ──────────────────────────────────────────────

    #[test]
    fn controller_initial_state() {
        let ctrl = BetaLoopController::new(test_config(), make_cohorts());
        assert_eq!(ctrl.stage(), BetaStage::Baseline);
        assert_eq!(ctrl.cohort_count(), 2);
        assert_eq!(ctrl.active_cohort_count(), 0); // No cohorts active at Baseline
        assert_eq!(ctrl.total_feedback(), 0);
        assert_eq!(ctrl.total_observations(), 0);
    }

    #[test]
    fn unknown_member_feedback_ignored() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.record_feedback(positive_feedback("unknown-user", 1000));
        assert_eq!(ctrl.total_feedback(), 0);
    }

    #[test]
    fn unknown_member_observation_ignored() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.record_observation(good_observation("unknown-user", 1000));
        assert_eq!(ctrl.total_observations(), 0);
    }

    #[test]
    fn feedback_and_observation_counting() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        assert_eq!(ctrl.total_observations(), 10);
        assert_eq!(ctrl.total_feedback(), 10);
    }

    #[test]
    fn feedback_capped_per_cohort() {
        let mut config = test_config();
        config.max_feedback_per_cohort = 3;
        let mut ctrl = BetaLoopController::new(config, make_cohorts());
        for i in 0..10 {
            ctrl.record_feedback(positive_feedback("alice", i * 100));
        }
        assert_eq!(ctrl.total_feedback(), 3);
    }

    #[test]
    fn observations_capped_per_cohort() {
        let mut config = test_config();
        config.max_observations_per_cohort = 5;
        let mut ctrl = BetaLoopController::new(config, make_cohorts());
        for i in 0..20 {
            ctrl.record_observation(good_observation("alice", i * 100));
        }
        assert_eq!(ctrl.total_observations(), 5);
    }

    // ── Evaluation logic ───────────────────────────────────────────────

    #[test]
    fn evaluate_hold_when_no_data() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        // Move to InternalBeta so there's an active cohort
        ctrl.last_decision = Some(PromotionDecision::Promote);
        ctrl.stage = BetaStage::InternalBeta;
        let eval = ctrl.evaluate(1000);
        assert_eq!(eval.decision, PromotionDecision::Hold);
        assert!(eval.reasons.iter().any(|r| r.code == "insufficient_data"));
    }

    #[test]
    fn evaluate_hold_when_insufficient_observations() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        // Only 2 observations, need 5
        ctrl.record_observation(good_observation("alice", 100));
        ctrl.record_observation(good_observation("alice", 200));
        let eval = ctrl.evaluate(1000);
        assert_eq!(eval.decision, PromotionDecision::Hold);
    }

    #[test]
    fn evaluate_hold_when_insufficient_feedback() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
        }
        ctrl.record_feedback(positive_feedback("bob", 100));
        ctrl.record_feedback(positive_feedback("bob", 200));

        let eval = ctrl.evaluate(1_000);
        assert_eq!(eval.decision, PromotionDecision::Hold);
        assert!(
            eval.reasons
                .iter()
                .any(|reason| reason.code == "insufficient_feedback")
        );
    }

    #[test]
    fn evaluate_promote_when_all_criteria_met() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        // Enough good observations for the internal cohort
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        let eval = ctrl.evaluate(2000);
        assert_eq!(eval.decision, PromotionDecision::Promote);
        assert!(eval.cohort_evaluations[0].meets_criteria);
    }

    #[test]
    fn evaluate_rollback_on_bad_smoothness() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        // All bad smoothness observations
        for i in 0..10 {
            ctrl.record_observation(bad_observation("alice", i * 100));
        }
        let eval = ctrl.evaluate(2000);
        assert_eq!(eval.decision, PromotionDecision::Rollback);
        assert!(
            eval.reasons
                .iter()
                .any(|r| r.code == "smoothness_budget_exceeded")
        );
    }

    #[test]
    fn evaluate_rollback_on_low_nps() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(negative_feedback("bob", i * 100));
        }
        let eval = ctrl.evaluate(2000);
        assert_eq!(eval.decision, PromotionDecision::Rollback);
        assert!(eval.reasons.iter().any(|r| r.code == "nps_below_rollback"));
    }

    #[test]
    fn evaluate_rollback_on_critical_friction() {
        let mut config = test_config();
        config.max_critical_friction = 2;
        let mut ctrl = BetaLoopController::new(config, make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
        }
        // 3 critical feedbacks > max of 2
        for i in 0..3 {
            let mut fb = negative_feedback("alice", i * 100);
            fb.nps_score = 10; // NPS is fine, but severity is Critical
            ctrl.record_feedback(fb);
        }
        let eval = ctrl.evaluate(2000);
        assert_eq!(eval.decision, PromotionDecision::Rollback);
        assert!(
            eval.reasons
                .iter()
                .any(|r| r.code == "critical_friction_exceeded")
        );
    }

    #[test]
    fn evaluate_hold_when_active_anomaly_blocks_promotion() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        ctrl.record_anomaly(hold_anomaly("sample-gap", 1_000));

        let eval = ctrl.evaluate(2_000);
        assert_eq!(eval.decision, PromotionDecision::Hold);
        assert!(
            eval.reasons
                .iter()
                .any(|reason| reason.code == "anomaly_forces_hold")
        );
    }

    #[test]
    fn evaluate_rollback_when_active_anomaly_demands_it() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        ctrl.record_anomaly(rollback_anomaly("regression-open", 1_000));

        let eval = ctrl.evaluate(2_000);
        assert_eq!(eval.decision, PromotionDecision::Rollback);
        assert!(
            eval.reasons
                .iter()
                .any(|reason| reason.code == "anomaly_forces_rollback")
        );
    }

    #[test]
    fn resolved_anomaly_no_longer_blocks_promotion() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        ctrl.record_anomaly(hold_anomaly("sample-gap", 1_000));
        assert_eq!(ctrl.active_anomaly_count(), 1);
        assert!(ctrl.resolve_anomaly(
            "sample-gap",
            2_000,
            vec!["evidence/wa-1u90p.8.7/decision_checkpoint_20260315.md".into()],
        ));

        let eval = ctrl.evaluate(3_000);
        assert_eq!(eval.decision, PromotionDecision::Promote);
        let snapshot = ctrl.snapshot();
        assert_eq!(snapshot.active_anomaly_count, 0);
        assert_eq!(snapshot.anomalies.len(), 1);
        assert_eq!(snapshot.anomalies[0].status, AnomalyStatus::Closed);
        assert!(
            snapshot.anomalies[0]
                .close_loop_evidence
                .iter()
                .any(|entry| entry.ends_with("decision_checkpoint_20260315.md"))
        );
    }

    #[test]
    fn record_anomaly_upserts_existing_entry() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        let mut anomaly = hold_anomaly("sample-gap", 1_000);
        ctrl.record_anomaly(anomaly.clone());
        anomaly.status = AnomalyStatus::Mitigated;
        anomaly.last_updated_at_ms = 2_000;
        ctrl.record_anomaly(anomaly.clone());

        let snapshot = ctrl.snapshot();
        assert_eq!(snapshot.anomalies.len(), 1);
        assert_eq!(snapshot.anomalies[0].status, AnomalyStatus::Mitigated);
        assert_eq!(snapshot.active_anomaly_count, 1);
    }

    // ── Stage transitions ──────────────────────────────────────────────

    #[test]
    fn advance_requires_promote_decision() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        assert!(!ctrl.advance_stage()); // No decision yet
        ctrl.last_decision = Some(PromotionDecision::Hold);
        assert!(!ctrl.advance_stage());
        ctrl.last_decision = Some(PromotionDecision::Rollback);
        assert!(!ctrl.advance_stage());
    }

    #[test]
    fn advance_through_all_stages() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        for expected in [
            BetaStage::InternalBeta,
            BetaStage::ClosedBeta,
            BetaStage::OpenBeta,
            BetaStage::GeneralAvailability,
        ] {
            ctrl.last_decision = Some(PromotionDecision::Promote);
            assert!(ctrl.advance_stage());
            assert_eq!(ctrl.stage(), expected);
        }
        // Can't go past GA
        ctrl.last_decision = Some(PromotionDecision::Promote);
        assert!(!ctrl.advance_stage());
    }

    #[test]
    fn rollback_decrements_stage() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::ClosedBeta;
        assert!(ctrl.rollback_stage());
        assert_eq!(ctrl.stage(), BetaStage::InternalBeta);
        assert!(ctrl.rollback_stage());
        assert_eq!(ctrl.stage(), BetaStage::Baseline);
        assert!(!ctrl.rollback_stage()); // Can't go below Baseline
    }

    #[test]
    fn advance_blocked_after_rollback() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        // Promote to InternalBeta
        ctrl.last_decision = Some(PromotionDecision::Promote);
        assert!(ctrl.advance_stage());
        assert_eq!(ctrl.stage(), BetaStage::InternalBeta);
        // Collect good data and get a Promote decision
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        let eval = ctrl.evaluate(2000);
        assert_eq!(eval.decision, PromotionDecision::Promote);
        // Roll back — should invalidate the Promote decision
        assert!(ctrl.rollback_stage());
        assert_eq!(ctrl.stage(), BetaStage::Baseline);
        // advance_stage must fail: rollback cleared last_decision
        assert!(!ctrl.advance_stage());
        assert_eq!(ctrl.stage(), BetaStage::Baseline);
    }

    #[test]
    fn transition_count_tracked() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.last_decision = Some(PromotionDecision::Promote);
        ctrl.advance_stage();
        ctrl.advance_stage(); // Fails — decision cleared
        assert_eq!(ctrl.snapshot().transition_count, 1);
        ctrl.rollback_stage();
        assert_eq!(ctrl.snapshot().transition_count, 2);
    }

    // ── Snapshot ───────────────────────────────────────────────────────

    #[test]
    fn snapshot_serde_roundtrip() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..5 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        let _ = ctrl.evaluate(1000);
        let snap = ctrl.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let rt: BetaLoopSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, snap);
    }

    #[test]
    fn snapshot_reflects_state() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
        }
        let _ = ctrl.evaluate(1000);
        let snap = ctrl.snapshot();
        assert_eq!(snap.stage, BetaStage::InternalBeta);
        assert_eq!(snap.total_observations, 10);
        assert_eq!(snap.evaluation_count, 1);
        assert_eq!(*snap.cohort_observation_counts.get("internal").unwrap(), 10);
    }

    // ── Reset ──────────────────────────────────────────────────────────

    #[test]
    fn reset_clears_all_state() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::ClosedBeta;
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        let _ = ctrl.evaluate(1000);
        ctrl.reset();
        let snap = ctrl.snapshot();
        assert_eq!(snap.stage, BetaStage::Baseline);
        assert_eq!(snap.total_feedback, 0);
        assert_eq!(snap.total_observations, 0);
        assert_eq!(snap.evaluation_count, 0);
        assert_eq!(snap.transition_count, 0);
        assert!(snap.last_decision.is_none());
    }

    // ── Config serde ───────────────────────────────────────────────────

    #[test]
    fn config_serde_roundtrip() {
        let config = BetaLoopConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let rt: BetaLoopConfig = serde_json::from_str(&json).unwrap();
        assert!((rt.smoothness_target - config.smoothness_target).abs() < f64::EPSILON);
        assert_eq!(
            rt.min_observations_per_cohort,
            config.min_observations_per_cohort
        );
        assert_eq!(rt.min_feedback_per_cohort, config.min_feedback_per_cohort);
        assert_eq!(rt.promotion_nps_threshold, config.promotion_nps_threshold);
    }

    #[test]
    fn decision_serde_roundtrip() {
        for d in [
            PromotionDecision::Promote,
            PromotionDecision::Hold,
            PromotionDecision::Rollback,
        ] {
            let json = serde_json::to_string(&d).unwrap();
            let rt: PromotionDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, d);
        }
    }

    #[test]
    fn feedback_category_serde_roundtrip() {
        for cat in [
            FeedbackCategory::Smoothness,
            FeedbackCategory::VisualGlitch,
            FeedbackCategory::Confusion,
            FeedbackCategory::WorkflowDisruption,
            FeedbackCategory::PerformanceRegression,
            FeedbackCategory::Positive,
            FeedbackCategory::General,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let rt: FeedbackCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, cat);
        }
    }

    // ── Percentile helper ──────────────────────────────────────────────

    #[test]
    fn percentile_empty_is_none() {
        assert_eq!(percentile_smoothness(&[], 0.5), None);
    }

    #[test]
    fn percentile_single_value() {
        let obs = [SmoothnessObservation {
            member_id: "x".into(),
            smoothness: 0.85,
            input_to_paint_us: None,
            frame_jitter_us: None,
            keystroke_echo_us: None,
            timestamp_ms: 0,
        }];
        assert!((percentile_smoothness(&obs, 0.5).unwrap() - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_median_of_sorted() {
        let obs: Vec<SmoothnessObservation> = (0..11)
            .map(|i| SmoothnessObservation {
                member_id: "x".into(),
                smoothness: i as f64 / 10.0,
                input_to_paint_us: None,
                frame_jitter_us: None,
                keystroke_echo_us: None,
                timestamp_ms: i * 100,
            })
            .collect();
        // p50 of [0.0, 0.1, ..., 1.0] = 0.5
        let p50 = percentile_smoothness(&obs, 0.5).unwrap();
        assert!((p50 - 0.5).abs() < f64::EPSILON);
    }

    // ── Full lifecycle integration ─────────────────────────────────────

    #[test]
    fn full_promotion_lifecycle() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());

        // Baseline → InternalBeta: no active cohorts at baseline, can't evaluate meaningfully
        // Force first transition
        ctrl.last_decision = Some(PromotionDecision::Promote);
        assert!(ctrl.advance_stage());
        assert_eq!(ctrl.stage(), BetaStage::InternalBeta);

        // Collect good data
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
            ctrl.record_feedback(positive_feedback("bob", i * 100));
        }
        let eval = ctrl.evaluate(2000);
        assert_eq!(eval.decision, PromotionDecision::Promote);
        assert!(ctrl.advance_stage());
        assert_eq!(ctrl.stage(), BetaStage::ClosedBeta);

        // Now both cohorts active; early-adopters need data too
        for i in 0..10 {
            ctrl.record_observation(good_observation("carol", i * 100));
            ctrl.record_feedback(positive_feedback("dave", i * 100));
        }
        let eval = ctrl.evaluate(3000);
        assert_eq!(eval.decision, PromotionDecision::Promote);
        assert!(ctrl.advance_stage());
        assert_eq!(ctrl.stage(), BetaStage::OpenBeta);

        let snap = ctrl.snapshot();
        assert_eq!(snap.transition_count, 3);
        assert!(snap.total_observations >= 20);
    }

    #[test]
    fn rollback_on_regression_mid_loop() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::ClosedBeta;

        // Good internal data
        for i in 0..10 {
            ctrl.record_observation(good_observation("alice", i * 100));
        }
        // Bad early-adopter data triggers rollback
        for i in 0..10 {
            ctrl.record_observation(bad_observation("carol", i * 100));
        }

        let eval = ctrl.evaluate(2000);
        assert_eq!(eval.decision, PromotionDecision::Rollback);
        assert!(ctrl.rollback_stage());
        assert_eq!(ctrl.stage(), BetaStage::InternalBeta);
    }

    #[test]
    fn evaluation_count_increments() {
        let mut ctrl = BetaLoopController::new(test_config(), make_cohorts());
        ctrl.stage = BetaStage::InternalBeta;
        let _ = ctrl.evaluate(1000);
        let _ = ctrl.evaluate(2000);
        let _ = ctrl.evaluate(3000);
        assert_eq!(ctrl.snapshot().evaluation_count, 3);
    }
}
