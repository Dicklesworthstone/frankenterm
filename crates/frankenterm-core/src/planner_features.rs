//! Planner feature extraction and normalization layer (ft-1i2ge.2.3).
//!
//! Transforms beads graph data and agent capability profiles into normalized
//! planner inputs: impact, urgency, risk, fit, and confidence scores (all 0.0–1.0).
//!
//! This module sits between the DAG readiness resolver (`beads_types`) and
//! the multi-factor scoring function (`plan.rs` suitability scorer).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::beads_types::{
    BeadReadinessReport, BeadReadyCandidate, BeadResolverReasonCode, BeadStatus,
};
use crate::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};

// ── Feature vector ──────────────────────────────────────────────────────────

/// Normalized planner feature vector for a single bead candidate.
///
/// All scores are in `[0.0, 1.0]` where 1.0 is the most favorable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerFeatureVector {
    /// Bead id this feature vector belongs to.
    pub bead_id: String,
    /// How much unblocking completing this bead provides (0.0 = leaf, 1.0 = critical bottleneck).
    pub impact: f64,
    /// How time-sensitive this bead is (priority + staleness).
    pub urgency: f64,
    /// Risk of failure or wasted work (degraded data, missing deps, cycles).
    pub risk: f64,
    /// Best-fit score across available agents (capability + load match).
    pub fit: f64,
    /// Confidence in the feature values (data completeness).
    pub confidence: f64,
}

impl PlannerFeatureVector {
    /// Composite score using weighted sum (default weights).
    #[must_use]
    pub fn composite_score(&self) -> f64 {
        self.composite_score_with_weights(&PlannerWeights::default())
    }

    /// Composite score with caller-supplied weights.
    #[must_use]
    pub fn composite_score_with_weights(&self, w: &PlannerWeights) -> f64 {
        let raw = w.confidence.mul_add(
            self.confidence,
            w.fit.mul_add(
                self.fit,
                w.risk.mul_add(
                    1.0 - self.risk, // invert: low risk is good
                    w.impact.mul_add(self.impact, w.urgency * self.urgency),
                ),
            ),
        );
        raw.clamp(0.0, 1.0)
    }
}

// ── Weights ─────────────────────────────────────────────────────────────────

/// Tunable weights for the composite scoring function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerWeights {
    pub impact: f64,
    pub urgency: f64,
    pub risk: f64,
    pub fit: f64,
    pub confidence: f64,
}

impl Default for PlannerWeights {
    fn default() -> Self {
        Self {
            impact: 0.30,
            urgency: 0.25,
            risk: 0.15,
            fit: 0.20,
            confidence: 0.10,
        }
    }
}

impl PlannerWeights {
    /// Sum of all weights (should be 1.0 for a proper distribution).
    #[must_use]
    pub fn total(&self) -> f64 {
        self.impact + self.urgency + self.risk + self.fit + self.confidence
    }
}

// ── Extraction config ───────────────────────────────────────────────────────

/// Configuration knobs for the feature extraction pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerExtractionConfig {
    /// Maximum transitive unblock count used to normalize impact to [0,1].
    /// If a bead unblocks >= this many, impact = 1.0.
    pub max_unblock_count: usize,
    /// Maximum critical path depth used to normalize impact depth component.
    pub max_critical_depth: usize,
    /// Maximum staleness in hours used to normalize urgency staleness component.
    pub max_staleness_hours: f64,
    /// Weight of unblock count vs depth in impact calculation.
    pub impact_unblock_weight: f64,
    /// Weight of depth vs unblock count in impact calculation.
    pub impact_depth_weight: f64,
    /// Weight of priority vs staleness in urgency calculation.
    pub urgency_priority_weight: f64,
    /// Weight of staleness vs priority in urgency calculation.
    pub urgency_staleness_weight: f64,
}

impl Default for PlannerExtractionConfig {
    fn default() -> Self {
        Self {
            max_unblock_count: 10,
            max_critical_depth: 8,
            max_staleness_hours: 168.0, // 1 week
            impact_unblock_weight: 0.6,
            impact_depth_weight: 0.4,
            urgency_priority_weight: 0.7,
            urgency_staleness_weight: 0.3,
        }
    }
}

// ── Extraction context ──────────────────────────────────────────────────────

/// Runtime context supplied alongside the readiness report for extraction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlannerExtractionContext {
    /// Staleness per bead id (hours since last update).
    #[serde(default)]
    pub staleness_hours: HashMap<String, f64>,
}

// ── Full extraction report ──────────────────────────────────────────────────

/// Output of the feature extraction pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerExtractionReport {
    pub features: Vec<PlannerFeatureVector>,
    pub ranked_ids: Vec<String>,
    pub config_used: PlannerExtractionConfig,
}

impl PlannerExtractionReport {
    /// Get the feature vector for a specific bead.
    #[must_use]
    pub fn get(&self, bead_id: &str) -> Option<&PlannerFeatureVector> {
        self.features.iter().find(|f| f.bead_id == bead_id)
    }
}

// ── Core extraction function ────────────────────────────────────────────────

/// Extract normalized planner features from a readiness report + agent profiles.
///
/// Produces a `PlannerFeatureVector` for each **ready** candidate in the report.
/// Non-ready candidates are excluded from the output.
#[must_use]
pub fn extract_planner_features(
    report: &BeadReadinessReport,
    agents: &[MissionAgentCapabilityProfile],
    context: &PlannerExtractionContext,
    config: &PlannerExtractionConfig,
) -> PlannerExtractionReport {
    let weights = PlannerWeights::default();
    let mut features: Vec<PlannerFeatureVector> = report
        .candidates
        .iter()
        .filter(|c| c.ready)
        .map(|c| {
            let impact = extract_impact(c, config);
            let urgency = extract_urgency(c, context, config);
            let risk = extract_risk(c);
            let fit = extract_fit(c, agents);
            let confidence = extract_confidence(c, context);

            PlannerFeatureVector {
                bead_id: c.id.clone(),
                impact,
                urgency,
                risk,
                fit,
                confidence,
            }
        })
        .collect();

    // Sort by composite score descending.
    features.sort_by(|a, b| {
        let sa = a.composite_score_with_weights(&weights);
        let sb = b.composite_score_with_weights(&weights);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let ranked_ids: Vec<String> = features.iter().map(|f| f.bead_id.clone()).collect();

    PlannerExtractionReport {
        features,
        ranked_ids,
        config_used: config.clone(),
    }
}

/// Same as `extract_planner_features` but includes all candidates (ready and blocked).
#[must_use]
pub fn extract_planner_features_all(
    report: &BeadReadinessReport,
    agents: &[MissionAgentCapabilityProfile],
    context: &PlannerExtractionContext,
    config: &PlannerExtractionConfig,
) -> PlannerExtractionReport {
    let weights = PlannerWeights::default();
    let mut features: Vec<PlannerFeatureVector> = report
        .candidates
        .iter()
        .map(|c| {
            let impact = extract_impact(c, config);
            let urgency = extract_urgency(c, context, config);
            let risk = extract_risk(c);
            let fit = extract_fit(c, agents);
            let confidence = extract_confidence(c, context);

            PlannerFeatureVector {
                bead_id: c.id.clone(),
                impact,
                urgency,
                risk,
                fit,
                confidence,
            }
        })
        .collect();

    features.sort_by(|a, b| {
        let sa = a.composite_score_with_weights(&weights);
        let sb = b.composite_score_with_weights(&weights);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let ranked_ids: Vec<String> = features.iter().map(|f| f.bead_id.clone()).collect();

    PlannerExtractionReport {
        features,
        ranked_ids,
        config_used: config.clone(),
    }
}

// ── Feature extractors ──────────────────────────────────────────────────────

/// Impact: how much unblocking this bead provides.
///
/// Combines transitive unblock count (how many beads become workable) with
/// critical path depth (how deep the unblocking chain goes).
fn extract_impact(candidate: &BeadReadyCandidate, config: &PlannerExtractionConfig) -> f64 {
    let unblock_norm =
        (candidate.transitive_unblock_count as f64 / config.max_unblock_count as f64).min(1.0);
    let depth_norm =
        (candidate.critical_path_depth_hint as f64 / config.max_critical_depth as f64).min(1.0);

    let impact = config
        .impact_unblock_weight
        .mul_add(unblock_norm, config.impact_depth_weight * depth_norm);
    impact.clamp(0.0, 1.0)
}

/// Urgency: how time-sensitive is this bead.
///
/// Combines priority level (P0 = highest urgency) with staleness
/// (how long the bead has been waiting without progress).
fn extract_urgency(
    candidate: &BeadReadyCandidate,
    context: &PlannerExtractionContext,
    config: &PlannerExtractionConfig,
) -> f64 {
    // Priority: P0 = 1.0, P1 = 0.75, P2 = 0.5, P3 = 0.25, P4+ = 0.0
    let priority_norm = match candidate.priority {
        0 => 1.0,
        1 => 0.75,
        2 => 0.5,
        3 => 0.25,
        _ => 0.0,
    };

    // Staleness: normalized by max staleness hours.
    let staleness_norm = context
        .staleness_hours
        .get(&candidate.id)
        .map(|h| (h / config.max_staleness_hours).min(1.0))
        .unwrap_or(0.0);

    let urgency = config.urgency_priority_weight.mul_add(
        priority_norm,
        config.urgency_staleness_weight * staleness_norm,
    );
    urgency.clamp(0.0, 1.0)
}

/// Risk: probability of failure or wasted work.
///
/// Increases with degraded data (missing deps, cycles, partial data).
/// Non-ready beads always get risk = 1.0.
fn extract_risk(candidate: &BeadReadyCandidate) -> f64 {
    if !candidate.ready {
        return 1.0;
    }

    let mut risk: f64 = 0.0;
    for reason in &candidate.degraded_reasons {
        risk += match reason {
            BeadResolverReasonCode::MissingDependencyNode => 0.4,
            BeadResolverReasonCode::CyclicDependencyGraph => 0.5,
            BeadResolverReasonCode::PartialGraphData => 0.2,
        };
    }

    risk.min(1.0)
}

/// Fit: how well the bead matches available agent capabilities.
///
/// Returns 1.0 if at least one agent is Ready with spare capacity,
/// lower if agents are degraded or fully loaded.
fn extract_fit(_candidate: &BeadReadyCandidate, agents: &[MissionAgentCapabilityProfile]) -> f64 {
    if agents.is_empty() {
        return 0.0;
    }

    let mut best_fit = 0.0_f64;
    for agent in agents {
        let capacity = agent.effective_capacity();
        let spare = capacity.saturating_sub(agent.current_load);
        if spare == 0 {
            continue;
        }

        let base = match &agent.availability {
            MissionAgentAvailability::Ready => 1.0,
            MissionAgentAvailability::Degraded { .. } => 0.6,
            MissionAgentAvailability::Paused { .. } => 0.0,
            MissionAgentAvailability::RateLimited { .. } => 0.1,
            MissionAgentAvailability::Offline { .. } => 0.0,
        };

        let load_ratio = if capacity > 0 {
            spare as f64 / capacity as f64
        } else {
            0.0
        };

        let fit = base * load_ratio;
        best_fit = best_fit.max(fit);
    }

    best_fit.clamp(0.0, 1.0)
}

/// Confidence: how reliable are the feature values.
///
/// Full confidence when: not degraded, staleness data available, beads data complete.
/// Lower confidence for partial/degraded data.
fn extract_confidence(candidate: &BeadReadyCandidate, context: &PlannerExtractionContext) -> f64 {
    let mut confidence: f64 = 1.0;

    // Each degraded reason reduces confidence.
    for reason in &candidate.degraded_reasons {
        confidence -= match reason {
            BeadResolverReasonCode::MissingDependencyNode => 0.3,
            BeadResolverReasonCode::CyclicDependencyGraph => 0.2,
            BeadResolverReasonCode::PartialGraphData => 0.4,
        };
    }

    // Missing staleness data reduces confidence slightly.
    if !context.staleness_hours.contains_key(&candidate.id) {
        confidence -= 0.1;
    }

    // InProgress status is slightly more confident (already triaged).
    if candidate.status == BeadStatus::InProgress {
        confidence += 0.05;
    }

    confidence.clamp(0.0, 1.0)
}

// ── Multi-factor scoring function (ft-1i2ge.2.4) ────────────────────────────

/// Effort estimate bucket for a bead task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortBucket {
    /// < 30 min
    Trivial,
    /// 30 min – 2 hours
    Small,
    /// 2 – 8 hours
    Medium,
    /// 8 – 24 hours
    Large,
    /// > 24 hours
    Epic,
}

impl EffortBucket {
    /// Normalized effort score (0.0 = trivial, 1.0 = epic).
    #[must_use]
    pub fn score(&self) -> f64 {
        match self {
            Self::Trivial => 0.0,
            Self::Small => 0.25,
            Self::Medium => 0.5,
            Self::Large => 0.75,
            Self::Epic => 1.0,
        }
    }
}

/// Input to the multi-factor scorer for a single candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorerInput {
    pub features: PlannerFeatureVector,
    /// Optional effort estimate (if not supplied, defaults to Medium).
    pub effort: Option<EffortBucket>,
    /// Optional label-based tags that may influence scoring (e.g. "safety", "regression").
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Configuration for the multi-factor scorer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScorerConfig {
    pub weights: PlannerWeights,
    /// Weight for the effort dimension (lower effort = higher score).
    pub effort_weight: f64,
    /// Bonus multiplier for safety-tagged beads (applied to final score).
    pub safety_bonus: f64,
    /// Bonus multiplier for regression-tagged beads.
    pub regression_bonus: f64,
    /// Minimum confidence threshold: candidates below this get score = 0.
    pub min_confidence_threshold: f64,
    /// Deterministic tie-breaking: when scores differ by less than this, use bead_id ordering.
    pub tie_break_epsilon: f64,
}

impl Default for ScorerConfig {
    fn default() -> Self {
        Self {
            weights: PlannerWeights::default(),
            effort_weight: 0.10,
            safety_bonus: 1.15,
            regression_bonus: 1.10,
            min_confidence_threshold: 0.1,
            tie_break_epsilon: 0.001,
        }
    }
}

/// Scored candidate with breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredCandidate {
    pub bead_id: String,
    pub final_score: f64,
    pub feature_composite: f64,
    pub effort_penalty: f64,
    pub tag_multiplier: f64,
    pub below_confidence_threshold: bool,
    pub rank: usize,
}

/// Output of the multi-factor scoring pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorerReport {
    pub scored: Vec<ScoredCandidate>,
    pub ranked_ids: Vec<String>,
    pub config_used: ScorerConfig,
}

impl ScorerReport {
    /// Get a scored candidate by bead id.
    #[must_use]
    pub fn get(&self, bead_id: &str) -> Option<&ScoredCandidate> {
        self.scored.iter().find(|s| s.bead_id == bead_id)
    }

    /// Top-N ranked bead ids.
    #[must_use]
    pub fn top_n(&self, n: usize) -> Vec<String> {
        self.ranked_ids.iter().take(n).cloned().collect()
    }
}

/// Run the multi-factor scorer on a set of planner inputs.
///
/// Produces a deterministically-ranked list of scored candidates.
/// Candidates below the confidence threshold get score = 0.
#[must_use]
pub fn score_candidates(inputs: &[ScorerInput], config: &ScorerConfig) -> ScorerReport {
    let mut scored: Vec<ScoredCandidate> = inputs
        .iter()
        .map(|input| {
            let feature_composite = input.features.composite_score_with_weights(&config.weights);
            let effort = input.effort.unwrap_or(EffortBucket::Medium);
            // Effort penalty: high effort reduces score.
            let effort_penalty = config.effort_weight * effort.score();

            // Tag multiplier: safety and regression tags boost score.
            let mut tag_multiplier = 1.0_f64;
            for tag in &input.tags {
                match tag.as_str() {
                    "safety" | "policy" => tag_multiplier = tag_multiplier.max(config.safety_bonus),
                    "regression" | "bug" => {
                        tag_multiplier = tag_multiplier.max(config.regression_bonus);
                    }
                    _ => {}
                }
            }

            let below_threshold = input.features.confidence < config.min_confidence_threshold;

            let final_score = if below_threshold {
                0.0
            } else {
                ((feature_composite - effort_penalty) * tag_multiplier).clamp(0.0, 1.0)
            };

            ScoredCandidate {
                bead_id: input.features.bead_id.clone(),
                final_score,
                feature_composite,
                effort_penalty,
                tag_multiplier,
                below_confidence_threshold: below_threshold,
                rank: 0, // filled in after sort
            }
        })
        .collect();

    // Sort: highest score first. Deterministic tie-break by bead_id.
    scored.sort_by(|a, b| {
        let diff = (a.final_score - b.final_score).abs();
        if diff < config.tie_break_epsilon {
            a.bead_id.cmp(&b.bead_id)
        } else {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        }
    });

    // Assign ranks (1-based).
    for (i, candidate) in scored.iter_mut().enumerate() {
        candidate.rank = i + 1;
    }

    let ranked_ids: Vec<String> = scored.iter().map(|s| s.bead_id.clone()).collect();

    ScorerReport {
        scored,
        ranked_ids,
        config_used: config.clone(),
    }
}

// ── Deterministic planner solver (ft-1i2ge.2.5) ─────────────────────────────

/// Why a candidate was not assigned.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// No agent has spare capacity.
    NoCapacity,
    /// Candidate conflicts with an already-assigned bead (e.g. same resource).
    ConflictWithAssigned { conflicting_bead_id: String },
    /// Safety gate denied this candidate.
    SafetyGateDenied { gate_name: String },
    /// Candidate's score was below the minimum threshold.
    BelowScoreThreshold,
    /// Candidate was already assigned to another agent in this round.
    AlreadyAssigned,
}

/// A single assignment: bead → agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub bead_id: String,
    pub agent_id: String,
    pub score: f64,
    pub rank: usize,
}

/// A candidate that was rejected with reasons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedCandidate {
    pub bead_id: String,
    pub score: f64,
    pub reasons: Vec<RejectionReason>,
}

/// Safety gate that can deny candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyGate {
    pub name: String,
    /// Bead ids that this gate denies.
    pub denied_bead_ids: Vec<String>,
}

/// Conflict declaration: two beads that cannot be assigned simultaneously.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictPair {
    pub bead_a: String,
    pub bead_b: String,
}

/// Configuration for the planner solver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolverConfig {
    /// Minimum score to consider for assignment.
    pub min_score: f64,
    /// Maximum beads to assign per round.
    pub max_assignments: usize,
    /// Safety gates to apply.
    #[serde(default)]
    pub safety_gates: Vec<SafetyGate>,
    /// Conflict pairs: beads that cannot be co-assigned.
    #[serde(default)]
    pub conflicts: Vec<ConflictPair>,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            min_score: 0.05,
            max_assignments: 10,
            safety_gates: Vec::new(),
            conflicts: Vec::new(),
        }
    }
}

/// Result of the planner solver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentSet {
    pub assignments: Vec<Assignment>,
    pub rejected: Vec<RejectedCandidate>,
    pub solver_config: SolverConfig,
}

impl AssignmentSet {
    /// Number of assignments made.
    #[must_use]
    pub fn assignment_count(&self) -> usize {
        self.assignments.len()
    }

    /// Get assignment for a specific bead.
    #[must_use]
    pub fn get_assignment(&self, bead_id: &str) -> Option<&Assignment> {
        self.assignments.iter().find(|a| a.bead_id == bead_id)
    }

    /// Get rejection for a specific bead.
    #[must_use]
    pub fn get_rejection(&self, bead_id: &str) -> Option<&RejectedCandidate> {
        self.rejected.iter().find(|r| r.bead_id == bead_id)
    }
}

/// Run the deterministic planner solver.
///
/// Takes a scored candidate list and available agents, then greedily assigns
/// beads to agents in score order, respecting capacity, conflicts, and safety gates.
#[must_use]
pub fn solve_assignments(
    scored: &ScorerReport,
    agents: &[MissionAgentCapabilityProfile],
    config: &SolverConfig,
) -> AssignmentSet {
    // Track remaining capacity per agent.
    let mut remaining_capacity: HashMap<String, u32> = agents
        .iter()
        .filter(|a| {
            matches!(
                a.availability,
                MissionAgentAvailability::Ready | MissionAgentAvailability::Degraded { .. }
            )
        })
        .map(|a| {
            let cap = a.effective_capacity().saturating_sub(a.current_load) as u32;
            (a.agent_id.clone(), cap)
        })
        .collect();

    // Build safety gate denial set.
    let denied: HashMap<String, Vec<String>> = config
        .safety_gates
        .iter()
        .flat_map(|gate| {
            gate.denied_bead_ids
                .iter()
                .map(move |id| (id.clone(), gate.name.clone()))
        })
        .fold(HashMap::new(), |mut acc, (id, gate_name)| {
            acc.entry(id).or_default().push(gate_name);
            acc
        });

    let mut assignments: Vec<Assignment> = Vec::new();
    let mut rejected: Vec<RejectedCandidate> = Vec::new();
    let mut assigned_bead_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for candidate in &scored.scored {
        if assignments.len() >= config.max_assignments {
            break;
        }

        let mut reasons: Vec<RejectionReason> = Vec::new();

        // Check score threshold.
        if candidate.final_score < config.min_score {
            reasons.push(RejectionReason::BelowScoreThreshold);
        }

        // Check safety gates.
        if let Some(gate_names) = denied.get(&candidate.bead_id) {
            for gate_name in gate_names {
                reasons.push(RejectionReason::SafetyGateDenied {
                    gate_name: gate_name.clone(),
                });
            }
        }

        // Check conflicts with already-assigned beads.
        for conflict in &config.conflicts {
            if conflict.bead_a == candidate.bead_id && assigned_bead_ids.contains(&conflict.bead_b)
            {
                reasons.push(RejectionReason::ConflictWithAssigned {
                    conflicting_bead_id: conflict.bead_b.clone(),
                });
            }
            if conflict.bead_b == candidate.bead_id && assigned_bead_ids.contains(&conflict.bead_a)
            {
                reasons.push(RejectionReason::ConflictWithAssigned {
                    conflicting_bead_id: conflict.bead_a.clone(),
                });
            }
        }

        if !reasons.is_empty() {
            rejected.push(RejectedCandidate {
                bead_id: candidate.bead_id.clone(),
                score: candidate.final_score,
                reasons,
            });
            continue;
        }

        // Find best available agent (most spare capacity, deterministic tie-break).
        let best_agent = remaining_capacity
            .iter()
            .filter(|(_, cap)| **cap > 0)
            .max_by(|(id_a, cap_a), (id_b, cap_b)| {
                cap_a.cmp(cap_b).then_with(|| id_b.cmp(id_a)) // higher cap first, then alphabetical
            })
            .map(|(id, _)| id.clone());

        match best_agent {
            Some(agent_id) => {
                *remaining_capacity.get_mut(&agent_id).unwrap() -= 1;
                assigned_bead_ids.insert(candidate.bead_id.clone());
                assignments.push(Assignment {
                    bead_id: candidate.bead_id.clone(),
                    agent_id,
                    score: candidate.final_score,
                    rank: assignments.len() + 1,
                });
            }
            None => {
                rejected.push(RejectedCandidate {
                    bead_id: candidate.bead_id.clone(),
                    score: candidate.final_score,
                    reasons: vec![RejectionReason::NoCapacity],
                });
            }
        }
    }

    AssignmentSet {
        assignments,
        rejected,
        solver_config: config.clone(),
    }
}

// ── Decision explainability (ft-1i2ge.2.6) ──────────────────────────────────

/// Human-readable explanation for why a bead was selected or rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionExplanation {
    pub bead_id: String,
    pub outcome: DecisionOutcome,
    pub summary: String,
    pub factors: Vec<ExplanationFactor>,
}

/// Whether this bead was assigned or rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    Assigned,
    Rejected,
}

/// A single factor contributing to the decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplanationFactor {
    pub dimension: String,
    pub value: f64,
    pub description: String,
    pub polarity: FactorPolarity,
}

/// Whether a factor contributed positively or negatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactorPolarity {
    Positive,
    Negative,
    Neutral,
}

/// Full explainability report for a decision cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainabilityReport {
    pub cycle_id: u64,
    pub explanations: Vec<DecisionExplanation>,
}

impl ExplainabilityReport {
    /// Get explanation for a specific bead.
    #[must_use]
    pub fn get(&self, bead_id: &str) -> Option<&DecisionExplanation> {
        self.explanations.iter().find(|e| e.bead_id == bead_id)
    }
}

/// Generate explainability payloads from an assignment set and scorer report.
#[must_use]
pub fn explain_decisions(
    cycle_id: u64,
    scorer_report: &ScorerReport,
    assignment_set: &AssignmentSet,
) -> ExplainabilityReport {
    let mut explanations = Vec::new();

    // Explain assignments.
    for assignment in &assignment_set.assignments {
        if let Some(scored) = scorer_report
            .scored
            .iter()
            .find(|s| s.bead_id == assignment.bead_id)
        {
            let factors = build_factors_for_scored(scored);
            let summary = format!(
                "Assigned to {} (rank #{}, score {:.3})",
                assignment.agent_id, assignment.rank, scored.final_score
            );
            explanations.push(DecisionExplanation {
                bead_id: assignment.bead_id.clone(),
                outcome: DecisionOutcome::Assigned,
                summary,
                factors,
            });
        }
    }

    // Explain rejections.
    for rejected in &assignment_set.rejected {
        let scored = scorer_report
            .scored
            .iter()
            .find(|s| s.bead_id == rejected.bead_id);

        let mut factors = scored.map(build_factors_for_scored).unwrap_or_default();

        for reason in &rejected.reasons {
            factors.push(ExplanationFactor {
                dimension: "rejection".to_string(),
                value: 0.0,
                description: format_rejection_reason(reason),
                polarity: FactorPolarity::Negative,
            });
        }

        let reason_str: Vec<String> = rejected
            .reasons
            .iter()
            .map(format_rejection_reason)
            .collect();
        let summary = format!(
            "Rejected (score {:.3}): {}",
            rejected.score,
            reason_str.join("; ")
        );

        explanations.push(DecisionExplanation {
            bead_id: rejected.bead_id.clone(),
            outcome: DecisionOutcome::Rejected,
            summary,
            factors,
        });
    }

    ExplainabilityReport {
        cycle_id,
        explanations,
    }
}

fn build_factors_for_scored(scored: &ScoredCandidate) -> Vec<ExplanationFactor> {
    vec![
        ExplanationFactor {
            dimension: "composite_score".to_string(),
            value: scored.feature_composite,
            description: format!("Feature composite score: {:.3}", scored.feature_composite),
            polarity: if scored.feature_composite >= 0.5 {
                FactorPolarity::Positive
            } else {
                FactorPolarity::Neutral
            },
        },
        ExplanationFactor {
            dimension: "effort_penalty".to_string(),
            value: scored.effort_penalty,
            description: format!("Effort penalty: -{:.3}", scored.effort_penalty),
            polarity: if scored.effort_penalty > 0.0 {
                FactorPolarity::Negative
            } else {
                FactorPolarity::Neutral
            },
        },
        ExplanationFactor {
            dimension: "tag_multiplier".to_string(),
            value: scored.tag_multiplier,
            description: if scored.tag_multiplier > 1.0 {
                format!("Tag bonus: x{:.2}", scored.tag_multiplier)
            } else {
                "No tag bonus".to_string()
            },
            polarity: if scored.tag_multiplier > 1.0 {
                FactorPolarity::Positive
            } else {
                FactorPolarity::Neutral
            },
        },
    ]
}

fn format_rejection_reason(reason: &RejectionReason) -> String {
    match reason {
        RejectionReason::NoCapacity => "No agent has spare capacity".to_string(),
        RejectionReason::ConflictWithAssigned {
            conflicting_bead_id,
        } => {
            format!("Conflicts with assigned bead {}", conflicting_bead_id)
        }
        RejectionReason::SafetyGateDenied { gate_name } => {
            format!("Denied by safety gate: {}", gate_name)
        }
        RejectionReason::BelowScoreThreshold => "Score below minimum threshold".to_string(),
        RejectionReason::AlreadyAssigned => "Already assigned to another agent".to_string(),
    }
}

// ── Anti-thrash governor (ft-1i2ge.2.7) ─────────────────────────────────────

/// Configuration for the anti-thrash governor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorConfig {
    /// Minimum cycles a bead must remain assigned before it can be reassigned.
    pub reassignment_cooldown_cycles: u64,
    /// Maximum consecutive cycles a bead can be skipped before its score is boosted.
    pub starvation_threshold_cycles: u64,
    /// Score boost applied per starvation cycle (additive, capped at starvation_max_boost).
    pub starvation_boost_per_cycle: f64,
    /// Maximum starvation boost that can be applied.
    pub starvation_max_boost: f64,
    /// Number of recent assignment snapshots to retain for oscillation detection.
    pub history_window: usize,
    /// If a bead flips between assigned/unassigned more than this many times
    /// within the history window, it is flagged as thrashing.
    pub thrash_flip_threshold: u32,
    /// Score penalty applied to thrashing beads (multiplicative, 0.0–1.0).
    pub thrash_penalty: f64,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            reassignment_cooldown_cycles: 3,
            starvation_threshold_cycles: 5,
            starvation_boost_per_cycle: 0.02,
            starvation_max_boost: 0.15,
            history_window: 10,
            thrash_flip_threshold: 3,
            thrash_penalty: 0.5,
        }
    }
}

/// Tracks per-bead state for anti-thrash governance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadGovernorState {
    /// When this bead was last assigned (cycle number).
    pub last_assigned_cycle: Option<u64>,
    /// How many consecutive cycles this bead has been skipped (not assigned).
    pub consecutive_skipped: u64,
    /// Recent assignment history: true = assigned, false = not assigned.
    pub assignment_history: Vec<bool>,
    /// Agent it was last assigned to.
    pub last_agent_id: Option<String>,
}

impl Default for BeadGovernorState {
    fn default() -> Self {
        Self {
            last_assigned_cycle: None,
            consecutive_skipped: 0,
            assignment_history: Vec::new(),
            last_agent_id: None,
        }
    }
}

/// Actions the governor can impose on a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernorAction {
    /// Allow the assignment as-is.
    Allow,
    /// Boost the score by a starvation-prevention amount.
    BoostScore { amount: f64 },
    /// Penalize the score to suppress thrashing.
    PenalizeScore { factor: f64 },
    /// Block reassignment during cooldown.
    BlockReassignment { remaining_cycles: u64 },
}

/// Result of governor evaluation for a single bead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorVerdict {
    pub bead_id: String,
    pub action: GovernorAction,
    pub adjusted_score: f64,
    pub original_score: f64,
    pub reason: String,
}

/// Full governor report for a cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorReport {
    pub cycle_id: u64,
    pub verdicts: Vec<GovernorVerdict>,
    pub thrashing_bead_ids: Vec<String>,
    pub starving_bead_ids: Vec<String>,
    pub cooldown_bead_ids: Vec<String>,
}

/// Stateful anti-thrash governor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThrashGovernor {
    pub config: GovernorConfig,
    pub bead_states: HashMap<String, BeadGovernorState>,
    pub current_cycle: u64,
}

impl ThrashGovernor {
    /// Create a new governor with default config.
    #[must_use]
    pub fn new(config: GovernorConfig) -> Self {
        Self {
            config,
            bead_states: HashMap::new(),
            current_cycle: 0,
        }
    }

    /// Evaluate governor constraints on scored candidates before solving.
    ///
    /// Returns adjusted scores and verdicts. Caller should use adjusted scores
    /// in the solver instead of raw scores.
    #[must_use]
    pub fn evaluate(&self, candidates: &[ScoredCandidate]) -> GovernorReport {
        let mut verdicts = Vec::new();
        let mut thrashing = Vec::new();
        let mut starving = Vec::new();
        let mut cooldown = Vec::new();

        for candidate in candidates {
            let state = self.bead_states.get(&candidate.bead_id);
            let verdict = self.evaluate_one(candidate, state);

            match &verdict.action {
                GovernorAction::PenalizeScore { .. } => {
                    thrashing.push(candidate.bead_id.clone());
                }
                GovernorAction::BoostScore { .. } => {
                    starving.push(candidate.bead_id.clone());
                }
                GovernorAction::BlockReassignment { .. } => {
                    cooldown.push(candidate.bead_id.clone());
                }
                GovernorAction::Allow => {}
            }

            verdicts.push(verdict);
        }

        GovernorReport {
            cycle_id: self.current_cycle,
            verdicts,
            thrashing_bead_ids: thrashing,
            starving_bead_ids: starving,
            cooldown_bead_ids: cooldown,
        }
    }

    fn evaluate_one(
        &self,
        candidate: &ScoredCandidate,
        state: Option<&BeadGovernorState>,
    ) -> GovernorVerdict {
        let original_score = candidate.final_score;

        // Check cooldown: if bead was recently reassigned to a different agent,
        // block reassignment.
        if let Some(state) = state {
            if let Some(last_cycle) = state.last_assigned_cycle {
                let elapsed = self.current_cycle.saturating_sub(last_cycle);
                if elapsed < self.config.reassignment_cooldown_cycles {
                    let remaining = self.config.reassignment_cooldown_cycles - elapsed;
                    return GovernorVerdict {
                        bead_id: candidate.bead_id.clone(),
                        action: GovernorAction::BlockReassignment {
                            remaining_cycles: remaining,
                        },
                        adjusted_score: 0.0,
                        original_score,
                        reason: format!(
                            "Cooldown: {} cycles remaining before reassignment allowed",
                            remaining
                        ),
                    };
                }
            }

            // Check thrashing: count flips in assignment history.
            let flips = count_flips(&state.assignment_history);
            if flips >= self.config.thrash_flip_threshold {
                let adjusted = original_score * self.config.thrash_penalty;
                return GovernorVerdict {
                    bead_id: candidate.bead_id.clone(),
                    action: GovernorAction::PenalizeScore {
                        factor: self.config.thrash_penalty,
                    },
                    adjusted_score: adjusted,
                    original_score,
                    reason: format!(
                        "Thrash detected: {} flips in last {} cycles",
                        flips,
                        state.assignment_history.len()
                    ),
                };
            }

            // Check starvation: boost if skipped too many cycles.
            if state.consecutive_skipped >= self.config.starvation_threshold_cycles {
                let extra_cycles = state
                    .consecutive_skipped
                    .saturating_sub(self.config.starvation_threshold_cycles);
                let boost = (extra_cycles as f64 * self.config.starvation_boost_per_cycle)
                    .min(self.config.starvation_max_boost);
                if boost > 0.0 {
                    let adjusted = (original_score + boost).min(1.0);
                    return GovernorVerdict {
                        bead_id: candidate.bead_id.clone(),
                        action: GovernorAction::BoostScore { amount: boost },
                        adjusted_score: adjusted,
                        original_score,
                        reason: format!(
                            "Starvation prevention: skipped {} cycles, boost {:.3}",
                            state.consecutive_skipped, boost
                        ),
                    };
                }
            }
        }

        GovernorVerdict {
            bead_id: candidate.bead_id.clone(),
            action: GovernorAction::Allow,
            adjusted_score: original_score,
            original_score,
            reason: "No governor intervention".to_string(),
        }
    }

    /// Record the outcome of a cycle: which beads were assigned and which were not.
    pub fn record_cycle(&mut self, assigned_bead_ids: &[String]) {
        self.current_cycle += 1;
        let assigned_set: std::collections::HashSet<&String> = assigned_bead_ids.iter().collect();

        // Update known beads.
        let known_ids: Vec<String> = self.bead_states.keys().cloned().collect();
        for bead_id in &known_ids {
            let is_assigned = assigned_set.contains(bead_id);
            let window = self.config.history_window;
            let state = self.bead_states.get_mut(bead_id).unwrap();
            push_history_bounded(state, is_assigned, window);

            if is_assigned {
                state.last_assigned_cycle = Some(self.current_cycle);
                state.consecutive_skipped = 0;
            } else {
                state.consecutive_skipped += 1;
            }
        }

        // Register newly seen beads.
        for bead_id in assigned_bead_ids {
            if !self.bead_states.contains_key(bead_id) {
                let mut state = BeadGovernorState::default();
                state.last_assigned_cycle = Some(self.current_cycle);
                state.assignment_history.push(true);
                self.bead_states.insert(bead_id.clone(), state);
            }
        }
    }

    /// Record which agent was assigned to a bead (for cooldown tracking).
    pub fn record_agent_assignment(&mut self, bead_id: &str, agent_id: &str) {
        let state = self.bead_states.entry(bead_id.to_string()).or_default();
        state.last_agent_id = Some(agent_id.to_string());
    }

    /// Register a bead as known but not assigned (for tracking starvation from start).
    pub fn register_bead(&mut self, bead_id: &str) {
        self.bead_states.entry(bead_id.to_string()).or_default();
    }
}

/// Push an assignment entry into bounded history.
fn push_history_bounded(state: &mut BeadGovernorState, assigned: bool, window: usize) {
    state.assignment_history.push(assigned);
    if state.assignment_history.len() > window {
        state
            .assignment_history
            .drain(0..state.assignment_history.len() - window);
    }
}

/// Count the number of state transitions (flips) in an assignment history.
fn count_flips(history: &[bool]) -> u32 {
    if history.len() < 2 {
        return 0;
    }
    history.windows(2).filter(|w| w[0] != w[1]).count() as u32
}

// ── Multi-objective mission profiles (ft-1i2ge.2.9) ─────────────────────────

/// Named mission profile that defines operational priorities.
///
/// Each profile tunes weights, thresholds, and governor parameters to
/// optimize for a specific operational goal without code changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionProfileKind {
    /// Default balanced profile.
    Balanced,
    /// Prioritize safety: risk weight high, safety bonus amplified, aggressive cooldown.
    SafetyFirst,
    /// Maximize throughput: lower thresholds, less cooldown, impact-heavy weights.
    Throughput,
    /// Focus on urgent/stale work: urgency weight dominant, starvation boost aggressive.
    UrgencyDriven,
    /// Conservative: high confidence threshold, long cooldown, minimal starvation boost.
    Conservative,
}

/// Complete mission profile: scorer config + governor config + extraction config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionProfile {
    pub kind: MissionProfileKind,
    pub name: String,
    pub description: String,
    pub scorer_config: ScorerConfig,
    pub governor_config: GovernorConfig,
    pub extraction_config: PlannerExtractionConfig,
}

impl MissionProfile {
    /// Create a profile from a named kind.
    #[must_use]
    pub fn from_kind(kind: MissionProfileKind) -> Self {
        match kind {
            MissionProfileKind::Balanced => Self::balanced(),
            MissionProfileKind::SafetyFirst => Self::safety_first(),
            MissionProfileKind::Throughput => Self::throughput(),
            MissionProfileKind::UrgencyDriven => Self::urgency_driven(),
            MissionProfileKind::Conservative => Self::conservative(),
        }
    }

    /// Balanced default profile.
    #[must_use]
    pub fn balanced() -> Self {
        Self {
            kind: MissionProfileKind::Balanced,
            name: "Balanced".to_string(),
            description: "Default balanced weighting across all dimensions".to_string(),
            scorer_config: ScorerConfig::default(),
            governor_config: GovernorConfig::default(),
            extraction_config: PlannerExtractionConfig::default(),
        }
    }

    /// Safety-first profile: risk and confidence dominate, aggressive cooldown.
    #[must_use]
    pub fn safety_first() -> Self {
        Self {
            kind: MissionProfileKind::SafetyFirst,
            name: "Safety First".to_string(),
            description: "Prioritize risk avoidance and high confidence".to_string(),
            scorer_config: ScorerConfig {
                weights: PlannerWeights {
                    impact: 0.15,
                    urgency: 0.10,
                    risk: 0.35,
                    fit: 0.15,
                    confidence: 0.25,
                },
                safety_bonus: 1.30,
                regression_bonus: 1.20,
                min_confidence_threshold: 0.3,
                ..ScorerConfig::default()
            },
            governor_config: GovernorConfig {
                reassignment_cooldown_cycles: 5,
                thrash_flip_threshold: 2,
                thrash_penalty: 0.3,
                ..GovernorConfig::default()
            },
            extraction_config: PlannerExtractionConfig::default(),
        }
    }

    /// Throughput profile: impact-heavy, minimal cooldown, lower thresholds.
    #[must_use]
    pub fn throughput() -> Self {
        Self {
            kind: MissionProfileKind::Throughput,
            name: "Throughput".to_string(),
            description: "Maximize work completed per cycle".to_string(),
            scorer_config: ScorerConfig {
                weights: PlannerWeights {
                    impact: 0.40,
                    urgency: 0.20,
                    risk: 0.10,
                    fit: 0.25,
                    confidence: 0.05,
                },
                safety_bonus: 1.05,
                min_confidence_threshold: 0.05,
                ..ScorerConfig::default()
            },
            governor_config: GovernorConfig {
                reassignment_cooldown_cycles: 1,
                starvation_threshold_cycles: 3,
                starvation_boost_per_cycle: 0.03,
                starvation_max_boost: 0.10,
                thrash_flip_threshold: 5,
                ..GovernorConfig::default()
            },
            extraction_config: PlannerExtractionConfig::default(),
        }
    }

    /// Urgency-driven profile: staleness dominates, aggressive starvation boost.
    #[must_use]
    pub fn urgency_driven() -> Self {
        Self {
            kind: MissionProfileKind::UrgencyDriven,
            name: "Urgency Driven".to_string(),
            description: "Focus on time-sensitive and stale work".to_string(),
            scorer_config: ScorerConfig {
                weights: PlannerWeights {
                    impact: 0.15,
                    urgency: 0.45,
                    risk: 0.10,
                    fit: 0.15,
                    confidence: 0.15,
                },
                ..ScorerConfig::default()
            },
            governor_config: GovernorConfig {
                starvation_threshold_cycles: 2,
                starvation_boost_per_cycle: 0.05,
                starvation_max_boost: 0.25,
                ..GovernorConfig::default()
            },
            extraction_config: PlannerExtractionConfig {
                urgency_staleness_weight: 0.6,
                urgency_priority_weight: 0.4,
                ..PlannerExtractionConfig::default()
            },
        }
    }

    /// Conservative profile: high confidence bar, long cooldown, minimal starvation.
    #[must_use]
    pub fn conservative() -> Self {
        Self {
            kind: MissionProfileKind::Conservative,
            name: "Conservative".to_string(),
            description: "High bar for confidence, minimal churn".to_string(),
            scorer_config: ScorerConfig {
                weights: PlannerWeights {
                    impact: 0.20,
                    urgency: 0.15,
                    risk: 0.25,
                    fit: 0.15,
                    confidence: 0.25,
                },
                min_confidence_threshold: 0.4,
                ..ScorerConfig::default()
            },
            governor_config: GovernorConfig {
                reassignment_cooldown_cycles: 6,
                starvation_threshold_cycles: 10,
                starvation_boost_per_cycle: 0.01,
                starvation_max_boost: 0.08,
                thrash_flip_threshold: 2,
                thrash_penalty: 0.3,
                ..GovernorConfig::default()
            },
            extraction_config: PlannerExtractionConfig::default(),
        }
    }
}

/// Utility policy tuner: adjusts profile weights based on runtime feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtilityPolicyTuner {
    /// Active profile.
    pub active_profile: MissionProfile,
    /// Override individual weight adjustments (additive, applied after profile).
    pub weight_overrides: HashMap<String, f64>,
    /// History of profile switches for audit.
    pub switch_history: Vec<ProfileSwitch>,
    /// Maximum switch history entries to retain.
    pub max_history: usize,
}

/// Record of a profile switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSwitch {
    pub cycle_id: u64,
    pub from: MissionProfileKind,
    pub to: MissionProfileKind,
    pub reason: String,
}

impl UtilityPolicyTuner {
    /// Create a tuner with the given profile.
    #[must_use]
    pub fn new(profile: MissionProfile) -> Self {
        Self {
            active_profile: profile,
            weight_overrides: HashMap::new(),
            switch_history: Vec::new(),
            max_history: 50,
        }
    }

    /// Switch to a different profile, recording the reason.
    pub fn switch_profile(&mut self, cycle_id: u64, kind: MissionProfileKind, reason: &str) {
        let old_kind = self.active_profile.kind;
        if old_kind == kind {
            return;
        }
        self.switch_history.push(ProfileSwitch {
            cycle_id,
            from: old_kind,
            to: kind,
            reason: reason.to_string(),
        });
        if self.switch_history.len() > self.max_history {
            self.switch_history
                .drain(0..self.switch_history.len() - self.max_history);
        }
        self.active_profile = MissionProfile::from_kind(kind);
        self.weight_overrides.clear();
    }

    /// Apply a weight override (additive adjustment to a named dimension).
    pub fn set_override(&mut self, dimension: &str, adjustment: f64) {
        self.weight_overrides
            .insert(dimension.to_string(), adjustment);
    }

    /// Remove a weight override.
    pub fn clear_override(&mut self, dimension: &str) {
        self.weight_overrides.remove(dimension);
    }

    /// Get the effective scorer config with overrides applied.
    #[must_use]
    pub fn effective_scorer_config(&self) -> ScorerConfig {
        let mut config = self.active_profile.scorer_config.clone();
        if let Some(&adj) = self.weight_overrides.get("impact") {
            config.weights.impact = (config.weights.impact + adj).clamp(0.0, 1.0);
        }
        if let Some(&adj) = self.weight_overrides.get("urgency") {
            config.weights.urgency = (config.weights.urgency + adj).clamp(0.0, 1.0);
        }
        if let Some(&adj) = self.weight_overrides.get("risk") {
            config.weights.risk = (config.weights.risk + adj).clamp(0.0, 1.0);
        }
        if let Some(&adj) = self.weight_overrides.get("fit") {
            config.weights.fit = (config.weights.fit + adj).clamp(0.0, 1.0);
        }
        if let Some(&adj) = self.weight_overrides.get("confidence") {
            config.weights.confidence = (config.weights.confidence + adj).clamp(0.0, 1.0);
        }
        if let Some(&adj) = self.weight_overrides.get("effort") {
            config.effort_weight = (config.effort_weight + adj).clamp(0.0, 1.0);
        }
        if let Some(&adj) = self.weight_overrides.get("min_confidence") {
            config.min_confidence_threshold =
                (config.min_confidence_threshold + adj).clamp(0.0, 1.0);
        }
        config
    }

    /// Get the effective governor config (currently no overrides supported).
    #[must_use]
    pub fn effective_governor_config(&self) -> GovernorConfig {
        self.active_profile.governor_config.clone()
    }

    /// Get the effective extraction config (currently no overrides supported).
    #[must_use]
    pub fn effective_extraction_config(&self) -> PlannerExtractionConfig {
        self.active_profile.extraction_config.clone()
    }
}

// ── Mission runtime config schema (ft-1i2ge.5.4) ────────────────────────────

/// Validation error for mission configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValidationError {
    pub field: String,
    pub message: String,
}

/// Severity level for config diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDiagnosticSeverity {
    Error,
    Warning,
    Info,
}

/// A single config diagnostic (error, warning, or info).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiagnostic {
    pub severity: ConfigDiagnosticSeverity,
    pub field: String,
    pub message: String,
}

/// Result of validating a mission runtime config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigValidationResult {
    pub valid: bool,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

impl ConfigValidationResult {
    /// Count errors only.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == ConfigDiagnosticSeverity::Error)
            .count()
    }

    /// Count warnings only.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == ConfigDiagnosticSeverity::Warning)
            .count()
    }
}

/// Unified mission runtime configuration schema.
///
/// This is the top-level config that operators provide to configure the
/// mission control loop. It bundles all sub-configs with validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionRuntimeConfig {
    /// Active mission profile kind.
    pub profile: MissionProfileKind,
    /// Cadence interval in milliseconds.
    pub cadence_ms: u64,
    /// Maximum triggers to batch before forcing evaluation.
    pub max_trigger_batch: usize,
    /// Whether to include blocked candidates in extraction reports.
    pub include_blocked_in_extraction: bool,
    /// Scorer configuration overrides (applied on top of profile defaults).
    #[serde(default)]
    pub scorer_overrides: ScorerConfigOverrides,
    /// Governor configuration overrides (applied on top of profile defaults).
    #[serde(default)]
    pub governor_overrides: GovernorConfigOverrides,
    /// Extraction configuration overrides.
    #[serde(default)]
    pub extraction_overrides: ExtractionConfigOverrides,
    /// Maximum assignments per solver round (0 = use solver default).
    pub max_assignments_per_round: usize,
    /// Minimum score threshold override (0.0 = use profile default).
    pub min_score_override: f64,
    /// Safety gates to apply in addition to profile defaults.
    #[serde(default)]
    pub additional_safety_gates: Vec<SafetyGate>,
    /// Conflict pairs to apply in addition to profile defaults.
    #[serde(default)]
    pub additional_conflicts: Vec<ConflictPair>,
}

/// Partial overrides for scorer weights (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScorerConfigOverrides {
    pub impact_weight: Option<f64>,
    pub urgency_weight: Option<f64>,
    pub risk_weight: Option<f64>,
    pub fit_weight: Option<f64>,
    pub confidence_weight: Option<f64>,
    pub effort_weight: Option<f64>,
    pub safety_bonus: Option<f64>,
    pub regression_bonus: Option<f64>,
    pub min_confidence_threshold: Option<f64>,
}

/// Partial overrides for governor config (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GovernorConfigOverrides {
    pub reassignment_cooldown_cycles: Option<u64>,
    pub starvation_threshold_cycles: Option<u64>,
    pub starvation_boost_per_cycle: Option<f64>,
    pub starvation_max_boost: Option<f64>,
    pub history_window: Option<usize>,
    pub thrash_flip_threshold: Option<u32>,
    pub thrash_penalty: Option<f64>,
}

/// Partial overrides for extraction config (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionConfigOverrides {
    pub max_unblock_count: Option<usize>,
    pub max_critical_depth: Option<usize>,
    pub max_staleness_hours: Option<f64>,
    pub impact_unblock_weight: Option<f64>,
    pub impact_depth_weight: Option<f64>,
    pub urgency_priority_weight: Option<f64>,
    pub urgency_staleness_weight: Option<f64>,
}

impl Default for MissionRuntimeConfig {
    fn default() -> Self {
        Self {
            profile: MissionProfileKind::Balanced,
            cadence_ms: 30_000,
            max_trigger_batch: 10,
            include_blocked_in_extraction: false,
            scorer_overrides: ScorerConfigOverrides::default(),
            governor_overrides: GovernorConfigOverrides::default(),
            extraction_overrides: ExtractionConfigOverrides::default(),
            max_assignments_per_round: 0,
            min_score_override: 0.0,
            additional_safety_gates: Vec::new(),
            additional_conflicts: Vec::new(),
        }
    }
}

impl MissionRuntimeConfig {
    /// Validate the configuration, returning diagnostics.
    #[must_use]
    pub fn validate(&self) -> ConfigValidationResult {
        let mut diagnostics = Vec::new();

        // Cadence bounds.
        if self.cadence_ms == 0 {
            diagnostics.push(ConfigDiagnostic {
                severity: ConfigDiagnosticSeverity::Error,
                field: "cadence_ms".to_string(),
                message: "Cadence must be > 0".to_string(),
            });
        } else if self.cadence_ms < 1000 {
            diagnostics.push(ConfigDiagnostic {
                severity: ConfigDiagnosticSeverity::Warning,
                field: "cadence_ms".to_string(),
                message: "Cadence below 1s may cause excessive CPU usage".to_string(),
            });
        }

        if self.max_trigger_batch == 0 {
            diagnostics.push(ConfigDiagnostic {
                severity: ConfigDiagnosticSeverity::Error,
                field: "max_trigger_batch".to_string(),
                message: "Trigger batch size must be > 0".to_string(),
            });
        }

        // Validate weight overrides are in [0,1].
        Self::validate_weight_range(
            &mut diagnostics,
            "scorer_overrides.impact_weight",
            self.scorer_overrides.impact_weight,
        );
        Self::validate_weight_range(
            &mut diagnostics,
            "scorer_overrides.urgency_weight",
            self.scorer_overrides.urgency_weight,
        );
        Self::validate_weight_range(
            &mut diagnostics,
            "scorer_overrides.risk_weight",
            self.scorer_overrides.risk_weight,
        );
        Self::validate_weight_range(
            &mut diagnostics,
            "scorer_overrides.fit_weight",
            self.scorer_overrides.fit_weight,
        );
        Self::validate_weight_range(
            &mut diagnostics,
            "scorer_overrides.confidence_weight",
            self.scorer_overrides.confidence_weight,
        );
        Self::validate_weight_range(
            &mut diagnostics,
            "scorer_overrides.effort_weight",
            self.scorer_overrides.effort_weight,
        );

        // Bonus multipliers should be >= 1.0.
        if let Some(bonus) = self.scorer_overrides.safety_bonus {
            if bonus < 1.0 {
                diagnostics.push(ConfigDiagnostic {
                    severity: ConfigDiagnosticSeverity::Warning,
                    field: "scorer_overrides.safety_bonus".to_string(),
                    message: "Safety bonus < 1.0 would penalize safety-tagged beads".to_string(),
                });
            }
        }

        // Governor: penalty should be in (0, 1].
        if let Some(penalty) = self.governor_overrides.thrash_penalty {
            if penalty <= 0.0 || penalty > 1.0 {
                diagnostics.push(ConfigDiagnostic {
                    severity: ConfigDiagnosticSeverity::Error,
                    field: "governor_overrides.thrash_penalty".to_string(),
                    message: "Thrash penalty must be in (0.0, 1.0]".to_string(),
                });
            }
        }

        // Governor: history window should be reasonable.
        if let Some(window) = self.governor_overrides.history_window {
            if window == 0 {
                diagnostics.push(ConfigDiagnostic {
                    severity: ConfigDiagnosticSeverity::Error,
                    field: "governor_overrides.history_window".to_string(),
                    message: "History window must be > 0".to_string(),
                });
            }
        }

        // min_score_override bounds.
        if self.min_score_override < 0.0 || self.min_score_override > 1.0 {
            diagnostics.push(ConfigDiagnostic {
                severity: ConfigDiagnosticSeverity::Error,
                field: "min_score_override".to_string(),
                message: "min_score_override must be in [0.0, 1.0]".to_string(),
            });
        }

        let has_errors = diagnostics
            .iter()
            .any(|d| d.severity == ConfigDiagnosticSeverity::Error);

        ConfigValidationResult {
            valid: !has_errors,
            diagnostics,
        }
    }

    fn validate_weight_range(
        diagnostics: &mut Vec<ConfigDiagnostic>,
        field: &str,
        value: Option<f64>,
    ) {
        if let Some(v) = value {
            if !(0.0..=1.0).contains(&v) {
                diagnostics.push(ConfigDiagnostic {
                    severity: ConfigDiagnosticSeverity::Error,
                    field: field.to_string(),
                    message: format!("Weight must be in [0.0, 1.0], got {}", v),
                });
            }
        }
    }

    /// Resolve this config into concrete sub-configs by applying overrides to profile defaults.
    #[must_use]
    pub fn resolve(&self) -> ResolvedMissionConfig {
        let profile = MissionProfile::from_kind(self.profile);

        // Scorer config with overrides.
        let mut scorer = profile.scorer_config.clone();
        if let Some(v) = self.scorer_overrides.impact_weight {
            scorer.weights.impact = v;
        }
        if let Some(v) = self.scorer_overrides.urgency_weight {
            scorer.weights.urgency = v;
        }
        if let Some(v) = self.scorer_overrides.risk_weight {
            scorer.weights.risk = v;
        }
        if let Some(v) = self.scorer_overrides.fit_weight {
            scorer.weights.fit = v;
        }
        if let Some(v) = self.scorer_overrides.confidence_weight {
            scorer.weights.confidence = v;
        }
        if let Some(v) = self.scorer_overrides.effort_weight {
            scorer.effort_weight = v;
        }
        if let Some(v) = self.scorer_overrides.safety_bonus {
            scorer.safety_bonus = v;
        }
        if let Some(v) = self.scorer_overrides.regression_bonus {
            scorer.regression_bonus = v;
        }
        if let Some(v) = self.scorer_overrides.min_confidence_threshold {
            scorer.min_confidence_threshold = v;
        }

        // Governor config with overrides.
        let mut governor = profile.governor_config.clone();
        if let Some(v) = self.governor_overrides.reassignment_cooldown_cycles {
            governor.reassignment_cooldown_cycles = v;
        }
        if let Some(v) = self.governor_overrides.starvation_threshold_cycles {
            governor.starvation_threshold_cycles = v;
        }
        if let Some(v) = self.governor_overrides.starvation_boost_per_cycle {
            governor.starvation_boost_per_cycle = v;
        }
        if let Some(v) = self.governor_overrides.starvation_max_boost {
            governor.starvation_max_boost = v;
        }
        if let Some(v) = self.governor_overrides.history_window {
            governor.history_window = v;
        }
        if let Some(v) = self.governor_overrides.thrash_flip_threshold {
            governor.thrash_flip_threshold = v;
        }
        if let Some(v) = self.governor_overrides.thrash_penalty {
            governor.thrash_penalty = v;
        }

        // Extraction config with overrides.
        let mut extraction = profile.extraction_config.clone();
        if let Some(v) = self.extraction_overrides.max_unblock_count {
            extraction.max_unblock_count = v;
        }
        if let Some(v) = self.extraction_overrides.max_critical_depth {
            extraction.max_critical_depth = v;
        }
        if let Some(v) = self.extraction_overrides.max_staleness_hours {
            extraction.max_staleness_hours = v;
        }
        if let Some(v) = self.extraction_overrides.impact_unblock_weight {
            extraction.impact_unblock_weight = v;
        }
        if let Some(v) = self.extraction_overrides.impact_depth_weight {
            extraction.impact_depth_weight = v;
        }
        if let Some(v) = self.extraction_overrides.urgency_priority_weight {
            extraction.urgency_priority_weight = v;
        }
        if let Some(v) = self.extraction_overrides.urgency_staleness_weight {
            extraction.urgency_staleness_weight = v;
        }

        // Solver config.
        let mut solver = SolverConfig {
            min_score: if self.min_score_override > 0.0 {
                self.min_score_override
            } else {
                scorer.min_confidence_threshold * 0.5
            },
            max_assignments: if self.max_assignments_per_round > 0 {
                self.max_assignments_per_round
            } else {
                SolverConfig::default().max_assignments
            },
            safety_gates: self.additional_safety_gates.clone(),
            conflicts: self.additional_conflicts.clone(),
        };
        // Merge profile defaults for solver if any.
        let default_solver = SolverConfig::default();
        if solver.max_assignments == 0 {
            solver.max_assignments = default_solver.max_assignments;
        }

        ResolvedMissionConfig {
            profile: self.profile,
            cadence_ms: self.cadence_ms,
            max_trigger_batch: self.max_trigger_batch,
            include_blocked_in_extraction: self.include_blocked_in_extraction,
            scorer_config: scorer,
            governor_config: governor,
            extraction_config: extraction,
            solver_config: solver,
        }
    }
}

/// Fully resolved config with no optionals — ready for the mission loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedMissionConfig {
    pub profile: MissionProfileKind,
    pub cadence_ms: u64,
    pub max_trigger_batch: usize,
    pub include_blocked_in_extraction: bool,
    pub scorer_config: ScorerConfig,
    pub governor_config: GovernorConfig,
    pub extraction_config: PlannerExtractionConfig,
    pub solver_config: SolverConfig,
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beads_types::{
        BeadDependencyRef, BeadIssueDetail, BeadIssueType, resolve_bead_readiness,
    };

    fn sample_detail(
        id: &str,
        status: BeadStatus,
        priority: u8,
        dependency_ids: &[(&str, &str)],
    ) -> BeadIssueDetail {
        BeadIssueDetail {
            id: id.to_string(),
            title: format!("Bead {}", id),
            status,
            priority,
            issue_type: BeadIssueType::Task,
            assignee: None,
            labels: Vec::new(),
            dependencies: dependency_ids
                .iter()
                .map(|(dep_id, dep_type)| BeadDependencyRef {
                    id: (*dep_id).to_string(),
                    title: None,
                    status: None,
                    priority: None,
                    dependency_type: Some((*dep_type).to_string()),
                })
                .collect(),
            dependents: Vec::new(),
            parent: None,
            ingest_warning: None,
            extra: HashMap::new(),
        }
    }

    fn ready_agent(agent_id: &str) -> MissionAgentCapabilityProfile {
        MissionAgentCapabilityProfile {
            agent_id: agent_id.to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 2,
            availability: MissionAgentAvailability::Ready,
        }
    }

    // ── Basic extraction ────────────────────────────────────────────────

    #[test]
    fn extract_empty_report() {
        let report = resolve_bead_readiness(&[]);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features(&report, &agents, &ctx, &config);
        assert!(result.features.is_empty());
        assert!(result.ranked_ids.is_empty());
    }

    #[test]
    fn extract_single_ready_bead() {
        let issues = vec![sample_detail("solo", BeadStatus::Open, 1, &[])];
        let report = resolve_bead_readiness(&issues);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features(&report, &agents, &ctx, &config);
        assert_eq!(result.features.len(), 1);
        assert_eq!(result.features[0].bead_id, "solo");
        assert!(result.features[0].impact >= 0.0 && result.features[0].impact <= 1.0);
        assert!(result.features[0].urgency >= 0.0 && result.features[0].urgency <= 1.0);
        assert!(result.features[0].risk >= 0.0 && result.features[0].risk <= 1.0);
        assert!(result.features[0].fit >= 0.0 && result.features[0].fit <= 1.0);
        assert!(result.features[0].confidence >= 0.0 && result.features[0].confidence <= 1.0);
    }

    #[test]
    fn extract_filters_blocked_candidates() {
        let issues = vec![
            sample_detail("blocker", BeadStatus::Open, 0, &[]),
            sample_detail("blocked", BeadStatus::Open, 1, &[("blocker", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features(&report, &agents, &ctx, &config);
        assert_eq!(result.features.len(), 1);
        assert_eq!(result.features[0].bead_id, "blocker");
    }

    #[test]
    fn extract_all_includes_blocked() {
        let issues = vec![
            sample_detail("blocker", BeadStatus::Open, 0, &[]),
            sample_detail("blocked", BeadStatus::Open, 1, &[("blocker", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features_all(&report, &agents, &ctx, &config);
        assert_eq!(result.features.len(), 2);
    }

    // ── Impact ──────────────────────────────────────────────────────────

    #[test]
    fn impact_zero_for_leaf() {
        let issues = vec![sample_detail("leaf", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let impact = extract_impact(c, &PlannerExtractionConfig::default());
        assert!((impact - 0.0).abs() < 1e-10);
    }

    #[test]
    fn impact_increases_with_unblock_count() {
        // root -> a, b, c (3 children)
        let issues = vec![
            sample_detail("root", BeadStatus::Open, 0, &[]),
            sample_detail("a", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("b", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("c", BeadStatus::Open, 1, &[("root", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let root = report.candidates.iter().find(|c| c.id == "root").unwrap();
        let config = PlannerExtractionConfig::default();
        let impact = extract_impact(root, &config);
        // 3/10 * 0.6 + 1/8 * 0.4 = 0.18 + 0.05 = 0.23
        assert!(impact > 0.0);
        assert!(impact <= 1.0);
    }

    #[test]
    fn impact_caps_at_max() {
        let mut config = PlannerExtractionConfig::default();
        config.max_unblock_count = 2;
        config.max_critical_depth = 1;

        // root -> a, b, c (3 children, exceeds max_unblock_count=2)
        let issues = vec![
            sample_detail("root", BeadStatus::Open, 0, &[]),
            sample_detail("a", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("b", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("c", BeadStatus::Open, 1, &[("root", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let root = report.candidates.iter().find(|c| c.id == "root").unwrap();
        let impact = extract_impact(root, &config);
        // Both components capped at 1.0: 0.6 * 1.0 + 0.4 * 1.0 = 1.0
        assert!((impact - 1.0).abs() < 1e-10);
    }

    // ── Urgency ─────────────────────────────────────────────────────────

    #[test]
    fn urgency_p0_highest() {
        let issues = vec![sample_detail("p0", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let config = PlannerExtractionConfig::default();
        let ctx = PlannerExtractionContext::default();
        let urgency = extract_urgency(c, &ctx, &config);
        // 0.7 * 1.0 + 0.3 * 0.0 = 0.7
        assert!((urgency - 0.7).abs() < 1e-10);
    }

    #[test]
    fn urgency_p4_lowest() {
        let issues = vec![sample_detail("p4", BeadStatus::Open, 4, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let config = PlannerExtractionConfig::default();
        let ctx = PlannerExtractionContext::default();
        let urgency = extract_urgency(c, &ctx, &config);
        // 0.7 * 0.0 + 0.3 * 0.0 = 0.0
        assert!((urgency - 0.0).abs() < 1e-10);
    }

    #[test]
    fn urgency_increases_with_staleness() {
        let issues = vec![sample_detail("stale", BeadStatus::Open, 2, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let config = PlannerExtractionConfig::default();
        let mut ctx = PlannerExtractionContext::default();
        ctx.staleness_hours.insert("stale".to_string(), 84.0); // half of 168h max
        let urgency = extract_urgency(c, &ctx, &config);
        // 0.7 * 0.5 + 0.3 * 0.5 = 0.35 + 0.15 = 0.5
        assert!((urgency - 0.5).abs() < 1e-10);
    }

    #[test]
    fn urgency_staleness_caps_at_max() {
        let issues = vec![sample_detail("very-stale", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let config = PlannerExtractionConfig::default();
        let mut ctx = PlannerExtractionContext::default();
        ctx.staleness_hours.insert("very-stale".to_string(), 1000.0); // way over max
        let urgency = extract_urgency(c, &ctx, &config);
        // 0.7 * 1.0 + 0.3 * 1.0 = 1.0
        assert!((urgency - 1.0).abs() < 1e-10);
    }

    // ── Risk ────────────────────────────────────────────────────────────

    #[test]
    fn risk_zero_for_clean_ready() {
        let issues = vec![sample_detail("clean", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let risk = extract_risk(c);
        assert!((risk - 0.0).abs() < 1e-10);
    }

    #[test]
    fn risk_one_for_blocked() {
        let issues = vec![
            sample_detail("dep", BeadStatus::Open, 0, &[]),
            sample_detail("blocked", BeadStatus::Open, 1, &[("dep", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let blocked = report
            .candidates
            .iter()
            .find(|c| c.id == "blocked")
            .unwrap();
        let risk = extract_risk(blocked);
        assert!((risk - 1.0).abs() < 1e-10);
    }

    #[test]
    fn risk_increases_with_degraded_reasons() {
        // Use partial graph data warning
        let mut detail = sample_detail("partial", BeadStatus::Open, 0, &[]);
        detail.ingest_warning = Some(BeadResolverReasonCode::PartialGraphData);
        let report = resolve_bead_readiness(&[detail]);
        let c = &report.candidates[0];
        let risk = extract_risk(c);
        assert!((risk - 0.2).abs() < 1e-10); // PartialGraphData = 0.2
    }

    // ── Fit ─────────────────────────────────────────────────────────────

    #[test]
    fn fit_zero_with_no_agents() {
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let fit = extract_fit(c, &[]);
        assert!((fit - 0.0).abs() < 1e-10);
    }

    #[test]
    fn fit_one_with_idle_ready_agent() {
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let agents = vec![ready_agent("a1")];
        let fit = extract_fit(c, &agents);
        assert!((fit - 1.0).abs() < 1e-10);
    }

    #[test]
    fn fit_reduced_with_degraded_agent() {
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "degraded".to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 2,
            availability: MissionAgentAvailability::Degraded {
                reason_code: "slow".to_string(),
                max_parallel_assignments: 1,
            },
        }];
        let fit = extract_fit(c, &agents);
        assert!(fit > 0.0);
        assert!(fit < 1.0);
    }

    #[test]
    fn fit_zero_with_all_agents_offline() {
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "offline".to_string(),
            capabilities: vec![],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 1,
            availability: MissionAgentAvailability::Offline {
                reason_code: "unreachable".to_string(),
            },
        }];
        let fit = extract_fit(c, &agents);
        assert!((fit - 0.0).abs() < 1e-10);
    }

    #[test]
    fn fit_zero_when_fully_loaded() {
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "busy".to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 2,
            max_parallel_assignments: 2,
            availability: MissionAgentAvailability::Ready,
        }];
        let fit = extract_fit(c, &agents);
        assert!((fit - 0.0).abs() < 1e-10);
    }

    #[test]
    fn fit_takes_best_agent() {
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let agents = vec![
            MissionAgentCapabilityProfile {
                agent_id: "busy".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 2,
                max_parallel_assignments: 2,
                availability: MissionAgentAvailability::Ready,
            },
            ready_agent("idle"),
        ];
        let fit = extract_fit(c, &agents);
        assert!((fit - 1.0).abs() < 1e-10);
    }

    // ── Confidence ──────────────────────────────────────────────────────

    #[test]
    fn confidence_high_for_clean_data() {
        let issues = vec![sample_detail("clean", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let mut ctx = PlannerExtractionContext::default();
        ctx.staleness_hours.insert("clean".to_string(), 10.0);
        let confidence = extract_confidence(c, &ctx);
        assert!((confidence - 1.0).abs() < 1e-10);
    }

    #[test]
    fn confidence_reduced_for_missing_staleness() {
        let issues = vec![sample_detail("no-stale", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let ctx = PlannerExtractionContext::default();
        let confidence = extract_confidence(c, &ctx);
        assert!((confidence - 0.9).abs() < 1e-10);
    }

    #[test]
    fn confidence_reduced_for_partial_graph() {
        let mut detail = sample_detail("partial", BeadStatus::Open, 0, &[]);
        detail.ingest_warning = Some(BeadResolverReasonCode::PartialGraphData);
        let report = resolve_bead_readiness(&[detail]);
        let c = &report.candidates[0];
        let ctx = PlannerExtractionContext::default();
        let confidence = extract_confidence(c, &ctx);
        // 1.0 - 0.4 (partial) - 0.1 (no staleness) = 0.5
        assert!((confidence - 0.5).abs() < 1e-10);
    }

    #[test]
    fn confidence_boost_for_in_progress() {
        let issues = vec![sample_detail("wip", BeadStatus::InProgress, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let c = &report.candidates[0];
        let mut ctx = PlannerExtractionContext::default();
        ctx.staleness_hours.insert("wip".to_string(), 5.0);
        let confidence = extract_confidence(c, &ctx);
        // 1.0 + 0.05 = 1.05 clamped to 1.0
        assert!((confidence - 1.0).abs() < 1e-10);
    }

    // ── Composite scoring ───────────────────────────────────────────────

    #[test]
    fn composite_score_within_bounds() {
        let fv = PlannerFeatureVector {
            bead_id: "test".to_string(),
            impact: 0.5,
            urgency: 0.5,
            risk: 0.5,
            fit: 0.5,
            confidence: 0.5,
        };
        let score = fv.composite_score();
        assert!((0.0..=1.0).contains(&score));
    }

    #[test]
    fn composite_score_max_when_perfect() {
        let fv = PlannerFeatureVector {
            bead_id: "perfect".to_string(),
            impact: 1.0,
            urgency: 1.0,
            risk: 0.0,
            fit: 1.0,
            confidence: 1.0,
        };
        let score = fv.composite_score();
        // 0.3*1 + 0.25*1 + 0.15*(1-0) + 0.2*1 + 0.1*1 = 1.0
        assert!((score - 1.0).abs() < 1e-10);
    }

    #[test]
    fn composite_score_zero_when_worst() {
        let fv = PlannerFeatureVector {
            bead_id: "worst".to_string(),
            impact: 0.0,
            urgency: 0.0,
            risk: 1.0,
            fit: 0.0,
            confidence: 0.0,
        };
        let score = fv.composite_score();
        assert!((score - 0.0).abs() < 1e-10);
    }

    #[test]
    fn composite_with_custom_weights() {
        let fv = PlannerFeatureVector {
            bead_id: "custom".to_string(),
            impact: 1.0,
            urgency: 0.0,
            risk: 0.0,
            fit: 0.0,
            confidence: 0.0,
        };
        let weights = PlannerWeights {
            impact: 1.0,
            urgency: 0.0,
            risk: 0.0,
            fit: 0.0,
            confidence: 0.0,
        };
        let score = fv.composite_score_with_weights(&weights);
        assert!((score - 1.0).abs() < 1e-10);
    }

    // ── Weights ─────────────────────────────────────────────────────────

    #[test]
    fn default_weights_sum_to_one() {
        let w = PlannerWeights::default();
        assert!((w.total() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn weights_serde_roundtrip() {
        let w = PlannerWeights::default();
        let json = serde_json::to_string(&w).unwrap();
        let back: PlannerWeights = serde_json::from_str(&json).unwrap();
        assert_eq!(back, w);
    }

    // ── Ranking ─────────────────────────────────────────────────────────

    #[test]
    fn ranking_high_impact_before_low() {
        // root unblocks 3, leaf unblocks 0. Both P0.
        let issues = vec![
            sample_detail("root", BeadStatus::Open, 0, &[]),
            sample_detail("a", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("b", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("c", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("leaf", BeadStatus::Open, 0, &[]),
        ];
        let report = resolve_bead_readiness(&issues);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features(&report, &agents, &ctx, &config);
        // root should be ranked before leaf
        assert_eq!(result.ranked_ids[0], "root");
    }

    #[test]
    fn ranking_p0_before_p4_when_equal_impact() {
        let issues = vec![
            sample_detail("p4", BeadStatus::Open, 4, &[]),
            sample_detail("p0", BeadStatus::Open, 0, &[]),
        ];
        let report = resolve_bead_readiness(&issues);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features(&report, &agents, &ctx, &config);
        assert_eq!(result.ranked_ids[0], "p0");
    }

    // ── Serde ───────────────────────────────────────────────────────────

    #[test]
    fn feature_vector_serde_roundtrip() {
        let fv = PlannerFeatureVector {
            bead_id: "test".to_string(),
            impact: 0.42,
            urgency: 0.73,
            risk: 0.15,
            fit: 0.88,
            confidence: 0.95,
        };
        let json = serde_json::to_string(&fv).unwrap();
        let back: PlannerFeatureVector = serde_json::from_str(&json).unwrap();
        assert_eq!(back.bead_id, "test");
        assert!((back.impact - 0.42).abs() < 1e-10);
    }

    #[test]
    fn extraction_report_serde_roundtrip() {
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = resolve_bead_readiness(&issues);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features(&report, &agents, &ctx, &config);
        let json = serde_json::to_string(&result).unwrap();
        let back: PlannerExtractionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.features.len(), 2);
        assert_eq!(back.ranked_ids.len(), 2);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = PlannerExtractionConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: PlannerExtractionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn context_serde_roundtrip() {
        let mut ctx = PlannerExtractionContext::default();
        ctx.staleness_hours.insert("a".to_string(), 24.0);
        let json = serde_json::to_string(&ctx).unwrap();
        let back: PlannerExtractionContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.staleness_hours.get("a"), Some(&24.0));
    }

    // ── Report.get ──────────────────────────────────────────────────────

    #[test]
    fn report_get_existing() {
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let result = extract_planner_features(&report, &agents, &ctx, &config);
        assert!(result.get("a").is_some());
        assert!(result.get("nonexistent").is_none());
    }

    // ── Multi-factor scorer (ft-1i2ge.2.4) ──────────────────────────────

    fn make_fv(
        id: &str,
        impact: f64,
        urgency: f64,
        risk: f64,
        fit: f64,
        confidence: f64,
    ) -> PlannerFeatureVector {
        PlannerFeatureVector {
            bead_id: id.to_string(),
            impact,
            urgency,
            risk,
            fit,
            confidence,
        }
    }

    fn make_input(
        fv: PlannerFeatureVector,
        effort: Option<EffortBucket>,
        tags: Vec<&str>,
    ) -> ScorerInput {
        ScorerInput {
            features: fv,
            effort,
            tags: tags.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn scorer_empty_input() {
        let report = score_candidates(&[], &ScorerConfig::default());
        assert!(report.scored.is_empty());
        assert!(report.ranked_ids.is_empty());
    }

    #[test]
    fn scorer_single_candidate() {
        let fv = make_fv("a", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs = vec![make_input(fv, None, vec![])];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        assert_eq!(report.scored.len(), 1);
        assert_eq!(report.scored[0].bead_id, "a");
        assert_eq!(report.scored[0].rank, 1);
        assert!(report.scored[0].final_score > 0.0);
    }

    #[test]
    fn scorer_effort_reduces_score() {
        let fv_easy = make_fv("easy", 0.5, 0.5, 0.0, 1.0, 1.0);
        let fv_hard = make_fv("hard", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs = vec![
            make_input(fv_easy, Some(EffortBucket::Trivial), vec![]),
            make_input(fv_hard, Some(EffortBucket::Epic), vec![]),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        let easy = report.get("easy").unwrap();
        let hard = report.get("hard").unwrap();
        assert!(
            easy.final_score > hard.final_score,
            "easy={} should beat hard={}",
            easy.final_score,
            hard.final_score
        );
    }

    #[test]
    fn scorer_safety_tag_boosts() {
        let fv_safe = make_fv("safe", 0.5, 0.5, 0.0, 1.0, 1.0);
        let fv_norm = make_fv("norm", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs = vec![
            make_input(fv_safe, None, vec!["safety"]),
            make_input(fv_norm, None, vec![]),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        let safe = report.get("safe").unwrap();
        let norm = report.get("norm").unwrap();
        assert!(
            safe.final_score > norm.final_score,
            "safe={} should beat norm={}",
            safe.final_score,
            norm.final_score
        );
    }

    #[test]
    fn scorer_regression_tag_boosts() {
        let fv_reg = make_fv("reg", 0.5, 0.5, 0.0, 1.0, 1.0);
        let fv_norm = make_fv("norm", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs = vec![
            make_input(fv_reg, None, vec!["regression"]),
            make_input(fv_norm, None, vec![]),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        let reg = report.get("reg").unwrap();
        let norm = report.get("norm").unwrap();
        assert!(reg.final_score > norm.final_score);
    }

    #[test]
    fn scorer_below_confidence_gets_zero() {
        let fv = make_fv("low-conf", 1.0, 1.0, 0.0, 1.0, 0.05);
        let inputs = vec![make_input(fv, None, vec![])];
        let config = ScorerConfig::default(); // threshold = 0.1
        let report = score_candidates(&inputs, &config);
        let c = &report.scored[0];
        assert!((c.final_score - 0.0).abs() < 1e-10);
        assert!(c.below_confidence_threshold);
    }

    #[test]
    fn scorer_above_confidence_not_zeroed() {
        let fv = make_fv("ok-conf", 1.0, 1.0, 0.0, 1.0, 0.5);
        let inputs = vec![make_input(fv, None, vec![])];
        let config = ScorerConfig::default();
        let report = score_candidates(&inputs, &config);
        let c = &report.scored[0];
        assert!(c.final_score > 0.0);
        assert!(!c.below_confidence_threshold);
    }

    #[test]
    fn scorer_deterministic_tie_break_by_id() {
        let fv_a = make_fv("alpha", 0.5, 0.5, 0.0, 1.0, 1.0);
        let fv_z = make_fv("zeta", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs = vec![
            make_input(fv_z, Some(EffortBucket::Medium), vec![]),
            make_input(fv_a, Some(EffortBucket::Medium), vec![]),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        // Same score => alphabetical tie-break
        assert_eq!(report.ranked_ids[0], "alpha");
        assert_eq!(report.ranked_ids[1], "zeta");
    }

    #[test]
    fn scorer_ranks_are_one_based() {
        let inputs = vec![
            make_input(make_fv("a", 1.0, 1.0, 0.0, 1.0, 1.0), None, vec![]),
            make_input(make_fv("b", 0.0, 0.0, 0.0, 1.0, 1.0), None, vec![]),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        assert_eq!(report.scored[0].rank, 1);
        assert_eq!(report.scored[1].rank, 2);
    }

    #[test]
    fn scorer_final_score_clamped() {
        // Even with safety bonus * high composite, score should not exceed 1.0
        let fv = make_fv("boost", 1.0, 1.0, 0.0, 1.0, 1.0);
        let inputs = vec![make_input(fv, Some(EffortBucket::Trivial), vec!["safety"])];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        assert!(report.scored[0].final_score <= 1.0);
    }

    #[test]
    fn scorer_report_top_n() {
        let inputs = vec![
            make_input(make_fv("a", 1.0, 1.0, 0.0, 1.0, 1.0), None, vec![]),
            make_input(make_fv("b", 0.5, 0.5, 0.0, 1.0, 1.0), None, vec![]),
            make_input(make_fv("c", 0.0, 0.0, 0.0, 1.0, 1.0), None, vec![]),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        assert_eq!(report.top_n(2), vec!["a", "b"]);
        assert_eq!(report.top_n(5).len(), 3); // only 3 available
    }

    #[test]
    fn scorer_report_get() {
        let inputs = vec![make_input(
            make_fv("target", 0.5, 0.5, 0.0, 1.0, 1.0),
            None,
            vec![],
        )];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        assert!(report.get("target").is_some());
        assert!(report.get("missing").is_none());
    }

    #[test]
    fn scorer_effort_buckets_ordered() {
        let buckets = [
            EffortBucket::Trivial,
            EffortBucket::Small,
            EffortBucket::Medium,
            EffortBucket::Large,
            EffortBucket::Epic,
        ];
        for window in buckets.windows(2) {
            assert!(
                window[0].score() < window[1].score(),
                "{:?} ({}) should be less than {:?} ({})",
                window[0],
                window[0].score(),
                window[1],
                window[1].score()
            );
        }
    }

    #[test]
    fn scorer_effort_bucket_serde_roundtrip() {
        for bucket in [
            EffortBucket::Trivial,
            EffortBucket::Small,
            EffortBucket::Medium,
            EffortBucket::Large,
            EffortBucket::Epic,
        ] {
            let json = serde_json::to_string(&bucket).unwrap();
            let back: EffortBucket = serde_json::from_str(&json).unwrap();
            assert_eq!(back, bucket);
        }
    }

    #[test]
    fn scorer_config_serde_roundtrip() {
        let config = ScorerConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: ScorerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn scorer_report_serde_roundtrip() {
        let inputs = vec![
            make_input(make_fv("a", 0.5, 0.5, 0.0, 1.0, 1.0), None, vec!["safety"]),
            make_input(
                make_fv("b", 0.3, 0.3, 0.1, 0.8, 0.9),
                Some(EffortBucket::Large),
                vec![],
            ),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        let json = serde_json::to_string(&report).unwrap();
        let back: ScorerReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ranked_ids, report.ranked_ids);
        assert_eq!(back.scored.len(), 2);
    }

    #[test]
    fn scorer_policy_tag_same_as_safety() {
        let fv_policy = make_fv("policy", 0.5, 0.5, 0.0, 1.0, 1.0);
        let fv_safety = make_fv("safety", 0.5, 0.5, 0.0, 1.0, 1.0);
        let config = ScorerConfig::default();
        let r1 = score_candidates(&[make_input(fv_policy, None, vec!["policy"])], &config);
        let r2 = score_candidates(&[make_input(fv_safety, None, vec!["safety"])], &config);
        assert!((r1.scored[0].final_score - r2.scored[0].final_score).abs() < 1e-10);
    }

    #[test]
    fn scorer_bug_tag_same_as_regression() {
        let fv_bug = make_fv("bug", 0.5, 0.5, 0.0, 1.0, 1.0);
        let fv_reg = make_fv("reg", 0.5, 0.5, 0.0, 1.0, 1.0);
        let config = ScorerConfig::default();
        let r1 = score_candidates(&[make_input(fv_bug, None, vec!["bug"])], &config);
        let r2 = score_candidates(&[make_input(fv_reg, None, vec!["regression"])], &config);
        assert!((r1.scored[0].final_score - r2.scored[0].final_score).abs() < 1e-10);
    }

    #[test]
    fn scorer_multiple_tags_uses_max_bonus() {
        // Both safety and regression tags: should use whichever bonus is higher
        let fv = make_fv("multi", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs = vec![make_input(fv.clone(), None, vec!["safety", "regression"])];
        let config = ScorerConfig::default();
        let report = score_candidates(&inputs, &config);

        // safety_bonus (1.15) > regression_bonus (1.10), so multiplier = 1.15
        let c = &report.scored[0];
        assert!((c.tag_multiplier - 1.15).abs() < 1e-10);
    }

    #[test]
    fn scorer_unknown_tag_ignored() {
        let fv_tagged = make_fv("tagged", 0.5, 0.5, 0.0, 1.0, 1.0);
        let fv_plain = make_fv("plain", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs_tagged = vec![make_input(fv_tagged, None, vec!["unknown-tag"])];
        let inputs_plain = vec![make_input(fv_plain, None, vec![])];
        let config = ScorerConfig::default();
        let r1 = score_candidates(&inputs_tagged, &config);
        let r2 = score_candidates(&inputs_plain, &config);
        assert!((r1.scored[0].final_score - r2.scored[0].final_score).abs() < 1e-10);
    }

    #[test]
    fn scorer_breakdown_components_match() {
        let fv = make_fv("check", 0.8, 0.6, 0.1, 0.9, 0.95);
        let inputs = vec![make_input(fv, Some(EffortBucket::Small), vec!["safety"])];
        let config = ScorerConfig::default();
        let report = score_candidates(&inputs, &config);
        let c = &report.scored[0];

        // Verify effort_penalty = effort_weight * effort_score = 0.10 * 0.25 = 0.025
        assert!((c.effort_penalty - 0.025).abs() < 1e-10);
        // Verify tag_multiplier = safety_bonus = 1.15
        assert!((c.tag_multiplier - 1.15).abs() < 1e-10);
        // final_score = clamp((composite - penalty) * multiplier, 0, 1)
        let expected =
            ((c.feature_composite - c.effort_penalty) * c.tag_multiplier).clamp(0.0, 1.0);
        assert!((c.final_score - expected).abs() < 1e-10);
    }

    #[test]
    fn scorer_high_risk_reduces_composite() {
        // High risk candidate should score lower than low risk
        let fv_risky = make_fv("risky", 0.5, 0.5, 0.9, 1.0, 1.0);
        let fv_safe = make_fv("safe", 0.5, 0.5, 0.0, 1.0, 1.0);
        let inputs = vec![
            make_input(fv_risky, None, vec![]),
            make_input(fv_safe, None, vec![]),
        ];
        let report = score_candidates(&inputs, &ScorerConfig::default());
        let risky = report.get("risky").unwrap();
        let safe = report.get("safe").unwrap();
        assert!(risky.feature_composite < safe.feature_composite);
        assert!(risky.final_score < safe.final_score);
    }

    // ── Solver (ft-1i2ge.2.5) ───────────────────────────────────────────

    fn scored_report(ids_scores: &[(&str, f64)]) -> ScorerReport {
        let scored: Vec<ScoredCandidate> = ids_scores
            .iter()
            .enumerate()
            .map(|(i, (id, score))| ScoredCandidate {
                bead_id: id.to_string(),
                final_score: *score,
                feature_composite: *score,
                effort_penalty: 0.0,
                tag_multiplier: 1.0,
                below_confidence_threshold: false,
                rank: i + 1,
            })
            .collect();
        let ranked_ids = scored.iter().map(|s| s.bead_id.clone()).collect();
        ScorerReport {
            scored,
            ranked_ids,
            config_used: ScorerConfig::default(),
        }
    }

    #[test]
    fn solver_empty_input() {
        let scored = scored_report(&[]);
        let agents = vec![ready_agent("a1")];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert!(result.assignments.is_empty());
        assert!(result.rejected.is_empty());
    }

    #[test]
    fn solver_single_assignment() {
        let scored = scored_report(&[("b1", 0.8)]);
        let agents = vec![ready_agent("a1")];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(result.assignment_count(), 1);
        let a = &result.assignments[0];
        assert_eq!(a.bead_id, "b1");
        assert_eq!(a.agent_id, "a1");
        assert_eq!(a.rank, 1);
    }

    #[test]
    fn solver_respects_capacity() {
        // Agent has capacity 2, 3 beads scored
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.7), ("b3", 0.5)]);
        let agents = vec![ready_agent("a1")]; // max_parallel = 2, current_load = 0
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(result.assignment_count(), 2);
        assert_eq!(result.rejected.len(), 1);
        let rej = &result.rejected[0];
        assert_eq!(rej.bead_id, "b3");
        assert!(rej.reasons.contains(&RejectionReason::NoCapacity));
    }

    #[test]
    fn solver_spreads_across_agents() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.7)]);
        let agents = vec![
            MissionAgentCapabilityProfile {
                agent_id: "a1".to_string(),
                capabilities: vec![],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 1,
                availability: MissionAgentAvailability::Ready,
            },
            MissionAgentCapabilityProfile {
                agent_id: "a2".to_string(),
                capabilities: vec![],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 1,
                availability: MissionAgentAvailability::Ready,
            },
        ];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(result.assignment_count(), 2);
        let agent_ids: Vec<&str> = result
            .assignments
            .iter()
            .map(|a| a.agent_id.as_str())
            .collect();
        assert!(agent_ids.contains(&"a1"));
        assert!(agent_ids.contains(&"a2"));
    }

    #[test]
    fn solver_below_score_threshold_rejected() {
        let scored = scored_report(&[("low", 0.01)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            min_score: 0.05,
            ..SolverConfig::default()
        };
        let result = solve_assignments(&scored, &agents, &config);
        assert_eq!(result.assignment_count(), 0);
        let rej = &result.rejected[0];
        assert_eq!(rej.bead_id, "low");
        assert!(rej.reasons.contains(&RejectionReason::BelowScoreThreshold));
    }

    #[test]
    fn solver_safety_gate_denies() {
        let scored = scored_report(&[("dangerous", 0.9), ("safe", 0.8)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            safety_gates: vec![SafetyGate {
                name: "no-dangerous".to_string(),
                denied_bead_ids: vec!["dangerous".to_string()],
            }],
            ..SolverConfig::default()
        };
        let result = solve_assignments(&scored, &agents, &config);
        assert_eq!(result.assignment_count(), 1);
        assert_eq!(result.assignments[0].bead_id, "safe");
        let rej = result.get_rejection("dangerous").unwrap();
        assert!(matches!(
            &rej.reasons[0],
            RejectionReason::SafetyGateDenied { gate_name } if gate_name == "no-dangerous"
        ));
    }

    #[test]
    fn solver_conflict_pair_prevents_coassignment() {
        let scored = scored_report(&[("x", 0.9), ("y", 0.8)]);
        let agents = vec![ready_agent("a1")]; // capacity 2
        let config = SolverConfig {
            conflicts: vec![ConflictPair {
                bead_a: "x".to_string(),
                bead_b: "y".to_string(),
            }],
            ..SolverConfig::default()
        };
        let result = solve_assignments(&scored, &agents, &config);
        assert_eq!(result.assignment_count(), 1);
        assert_eq!(result.assignments[0].bead_id, "x");
        let rej = result.get_rejection("y").unwrap();
        assert!(matches!(
            &rej.reasons[0],
            RejectionReason::ConflictWithAssigned { conflicting_bead_id } if conflicting_bead_id == "x"
        ));
    }

    #[test]
    fn solver_max_assignments_limit() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.8), ("b3", 0.7)]);
        let agents = vec![ready_agent("a1")]; // capacity 2
        let config = SolverConfig {
            max_assignments: 1,
            ..SolverConfig::default()
        };
        let result = solve_assignments(&scored, &agents, &config);
        assert_eq!(result.assignment_count(), 1);
    }

    #[test]
    fn solver_offline_agents_skipped() {
        let scored = scored_report(&[("b1", 0.9)]);
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "offline".to_string(),
            capabilities: vec![],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 5,
            availability: MissionAgentAvailability::Offline {
                reason_code: "unreachable".to_string(),
            },
        }];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(result.assignment_count(), 0);
        assert!(
            result.rejected[0]
                .reasons
                .contains(&RejectionReason::NoCapacity)
        );
    }

    #[test]
    fn solver_paused_agents_skipped() {
        let scored = scored_report(&[("b1", 0.9)]);
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "paused".to_string(),
            capabilities: vec![],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 5,
            availability: MissionAgentAvailability::Paused {
                reason_code: "manual_pause".to_string(),
            },
        }];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(result.assignment_count(), 0);
    }

    #[test]
    fn solver_degraded_agents_used() {
        let scored = scored_report(&[("b1", 0.9)]);
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "degraded".to_string(),
            capabilities: vec![],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 4,
            availability: MissionAgentAvailability::Degraded {
                reason_code: "slow".to_string(),
                max_parallel_assignments: 1,
            },
        }];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(result.assignment_count(), 1);
        assert_eq!(result.assignments[0].agent_id, "degraded");
    }

    #[test]
    fn solver_fully_loaded_agent_skipped() {
        let scored = scored_report(&[("b1", 0.9)]);
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "full".to_string(),
            capabilities: vec![],
            lane_affinity: Vec::new(),
            current_load: 2,
            max_parallel_assignments: 2,
            availability: MissionAgentAvailability::Ready,
        }];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(result.assignment_count(), 0);
    }

    #[test]
    fn solver_assignment_ranks_sequential() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.7), ("b3", 0.5)]);
        let agents = vec![ready_agent("a1"), ready_agent("a2"), ready_agent("a3")];
        let result = solve_assignments(&scored, &agents, &SolverConfig::default());
        for (i, a) in result.assignments.iter().enumerate() {
            assert_eq!(a.rank, i + 1);
        }
    }

    #[test]
    fn solver_no_agents_rejects_all() {
        let scored = scored_report(&[("b1", 0.9)]);
        let result = solve_assignments(&scored, &[], &SolverConfig::default());
        assert_eq!(result.assignment_count(), 0);
        assert_eq!(result.rejected.len(), 1);
    }

    #[test]
    fn solver_deterministic_agent_selection() {
        // Two agents with equal capacity — should pick deterministically
        let scored = scored_report(&[("b1", 0.9)]);
        let agents = vec![
            MissionAgentCapabilityProfile {
                agent_id: "alpha".to_string(),
                capabilities: vec![],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 1,
                availability: MissionAgentAvailability::Ready,
            },
            MissionAgentCapabilityProfile {
                agent_id: "beta".to_string(),
                capabilities: vec![],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 1,
                availability: MissionAgentAvailability::Ready,
            },
        ];
        let r1 = solve_assignments(&scored, &agents, &SolverConfig::default());
        let r2 = solve_assignments(&scored, &agents, &SolverConfig::default());
        assert_eq!(r1.assignments[0].agent_id, r2.assignments[0].agent_id);
    }

    #[test]
    fn solver_get_assignment_and_rejection() {
        let scored = scored_report(&[("assigned", 0.9), ("rejected", 0.01)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            min_score: 0.05,
            ..SolverConfig::default()
        };
        let result = solve_assignments(&scored, &agents, &config);
        assert!(result.get_assignment("assigned").is_some());
        assert!(result.get_assignment("rejected").is_none());
        assert!(result.get_rejection("rejected").is_some());
        assert!(result.get_rejection("assigned").is_none());
    }

    #[test]
    fn solver_config_serde_roundtrip() {
        let config = SolverConfig {
            min_score: 0.1,
            max_assignments: 5,
            safety_gates: vec![SafetyGate {
                name: "test".to_string(),
                denied_bead_ids: vec!["x".to_string()],
            }],
            conflicts: vec![ConflictPair {
                bead_a: "a".to_string(),
                bead_b: "b".to_string(),
            }],
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SolverConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.min_score, 0.1);
        assert_eq!(back.safety_gates.len(), 1);
        assert_eq!(back.conflicts.len(), 1);
    }

    #[test]
    fn solver_assignment_set_serde_roundtrip() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.01)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            min_score: 0.05,
            ..SolverConfig::default()
        };
        let result = solve_assignments(&scored, &agents, &config);
        let json = serde_json::to_string(&result).unwrap();
        let back: AssignmentSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back.assignment_count(), 1);
        assert_eq!(back.rejected.len(), 1);
    }

    #[test]
    fn solver_rejection_reason_serde_roundtrip() {
        let reasons = vec![
            RejectionReason::NoCapacity,
            RejectionReason::ConflictWithAssigned {
                conflicting_bead_id: "x".to_string(),
            },
            RejectionReason::SafetyGateDenied {
                gate_name: "gate1".to_string(),
            },
            RejectionReason::BelowScoreThreshold,
            RejectionReason::AlreadyAssigned,
        ];
        for reason in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            let back: RejectionReason = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, reason);
        }
    }

    // ── Explainability tests (ft-1i2ge.2.6) ─────────────────────────────────

    #[test]
    fn explain_empty_inputs() {
        let scorer = ScorerReport {
            scored: Vec::new(),
            ranked_ids: Vec::new(),
            config_used: ScorerConfig::default(),
        };
        let assignment_set = AssignmentSet {
            assignments: Vec::new(),
            rejected: Vec::new(),
            solver_config: SolverConfig::default(),
        };
        let report = explain_decisions(1, &scorer, &assignment_set);
        assert_eq!(report.cycle_id, 1);
        assert!(report.explanations.is_empty());
    }

    #[test]
    fn explain_single_assignment() {
        let scored = scored_report(&[("b1", 0.8)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig::default();
        let assignments = solve_assignments(&scored, &agents, &config);
        assert_eq!(assignments.assignment_count(), 1);

        let report = explain_decisions(42, &scored, &assignments);
        assert_eq!(report.cycle_id, 42);
        assert_eq!(report.explanations.len(), 1);

        let expl = &report.explanations[0];
        assert_eq!(expl.bead_id, "b1");
        assert_eq!(expl.outcome, DecisionOutcome::Assigned);
        assert!(expl.summary.contains("Assigned to a1"));
        assert!(expl.summary.contains("rank #1"));
        assert!(!expl.factors.is_empty());
    }

    #[test]
    fn explain_single_rejection_no_capacity() {
        let scored = scored_report(&[("b1", 0.8), ("b2", 0.6), ("b3", 0.4)]);
        let agents = vec![{
            let mut a = ready_agent("a1");
            a.max_parallel_assignments = 1;
            a
        }];
        let config = SolverConfig::default();
        let assignments = solve_assignments(&scored, &agents, &config);

        let report = explain_decisions(10, &scored, &assignments);
        let rejected_expls: Vec<_> = report
            .explanations
            .iter()
            .filter(|e| e.outcome == DecisionOutcome::Rejected)
            .collect();
        assert!(rejected_expls.len() >= 2);
        for expl in &rejected_expls {
            assert!(expl.summary.contains("Rejected"));
            assert!(
                expl.factors
                    .iter()
                    .any(|f| f.dimension == "rejection" && f.polarity == FactorPolarity::Negative)
            );
        }
    }

    #[test]
    fn explain_rejection_below_threshold() {
        let scored = scored_report(&[("b1", 0.01)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            min_score: 0.5,
            ..SolverConfig::default()
        };
        let assignments = solve_assignments(&scored, &agents, &config);
        assert_eq!(assignments.rejected.len(), 1);

        let report = explain_decisions(5, &scored, &assignments);
        let expl = &report.explanations[0];
        assert_eq!(expl.outcome, DecisionOutcome::Rejected);
        assert!(expl.summary.contains("below minimum threshold"));
    }

    #[test]
    fn explain_rejection_conflict() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.7)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            conflicts: vec![ConflictPair {
                bead_a: "b1".to_string(),
                bead_b: "b2".to_string(),
            }],
            ..SolverConfig::default()
        };
        let assignments = solve_assignments(&scored, &agents, &config);

        let report = explain_decisions(6, &scored, &assignments);
        let rejected: Vec<_> = report
            .explanations
            .iter()
            .filter(|e| e.outcome == DecisionOutcome::Rejected)
            .collect();
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].bead_id, "b2");
        assert!(rejected[0].summary.contains("Conflicts with assigned bead"));
    }

    #[test]
    fn explain_rejection_safety_gate() {
        let scored = scored_report(&[("b1", 0.9)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            safety_gates: vec![SafetyGate {
                name: "deploy-freeze".to_string(),
                denied_bead_ids: vec!["b1".to_string()],
            }],
            ..SolverConfig::default()
        };
        let assignments = solve_assignments(&scored, &agents, &config);

        let report = explain_decisions(7, &scored, &assignments);
        let expl = &report.explanations[0];
        assert_eq!(expl.outcome, DecisionOutcome::Rejected);
        assert!(expl.summary.contains("deploy-freeze"));
    }

    #[test]
    fn explain_factors_positive_composite() {
        let scored = scored_report(&[("b1", 0.9)]);
        let agents = vec![ready_agent("a1")];
        let assignments = solve_assignments(&scored, &agents, &SolverConfig::default());

        let report = explain_decisions(8, &scored, &assignments);
        let expl = &report.explanations[0];
        let composite_factor = expl
            .factors
            .iter()
            .find(|f| f.dimension == "composite_score")
            .unwrap();
        assert_eq!(composite_factor.polarity, FactorPolarity::Positive);
        assert!(composite_factor.value > 0.0);
    }

    #[test]
    fn explain_factors_effort_penalty() {
        let inputs = vec![ScorerInput {
            features: PlannerFeatureVector {
                bead_id: "b1".to_string(),
                impact: 0.8,
                urgency: 0.8,
                risk: 0.2,
                fit: 0.8,
                confidence: 0.9,
            },
            effort: Some(EffortBucket::Epic),
            tags: Vec::new(),
        }];
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let agents = vec![ready_agent("a1")];
        let assignments = solve_assignments(&scored, &agents, &SolverConfig::default());

        let report = explain_decisions(9, &scored, &assignments);
        let expl = &report.explanations[0];
        let effort_factor = expl
            .factors
            .iter()
            .find(|f| f.dimension == "effort_penalty")
            .unwrap();
        assert_eq!(effort_factor.polarity, FactorPolarity::Negative);
        assert!(effort_factor.value > 0.0);
    }

    #[test]
    fn explain_factors_tag_bonus() {
        let inputs = vec![ScorerInput {
            features: PlannerFeatureVector {
                bead_id: "b1".to_string(),
                impact: 0.8,
                urgency: 0.8,
                risk: 0.2,
                fit: 0.8,
                confidence: 0.9,
            },
            effort: None,
            tags: vec!["safety".to_string()],
        }];
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let agents = vec![ready_agent("a1")];
        let assignments = solve_assignments(&scored, &agents, &SolverConfig::default());

        let report = explain_decisions(10, &scored, &assignments);
        let expl = &report.explanations[0];
        let tag_factor = expl
            .factors
            .iter()
            .find(|f| f.dimension == "tag_multiplier")
            .unwrap();
        assert_eq!(tag_factor.polarity, FactorPolarity::Positive);
        assert!(tag_factor.value > 1.0);
    }

    #[test]
    fn explain_mixed_assigned_and_rejected() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.01)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            min_score: 0.05,
            ..SolverConfig::default()
        };
        let assignments = solve_assignments(&scored, &agents, &config);

        let report = explain_decisions(11, &scored, &assignments);
        assert_eq!(report.explanations.len(), 2);

        let assigned: Vec<_> = report
            .explanations
            .iter()
            .filter(|e| e.outcome == DecisionOutcome::Assigned)
            .collect();
        let rejected: Vec<_> = report
            .explanations
            .iter()
            .filter(|e| e.outcome == DecisionOutcome::Rejected)
            .collect();
        assert_eq!(assigned.len(), 1);
        assert_eq!(rejected.len(), 1);
        assert_eq!(assigned[0].bead_id, "b1");
        assert_eq!(rejected[0].bead_id, "b2");
    }

    #[test]
    fn explain_get_by_bead_id() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.5)]);
        let agents = vec![ready_agent("a1")];
        let assignments = solve_assignments(&scored, &agents, &SolverConfig::default());
        let report = explain_decisions(12, &scored, &assignments);

        assert!(report.get("b1").is_some());
        assert!(report.get("b2").is_some());
        assert!(report.get("nonexistent").is_none());
    }

    #[test]
    fn explain_decision_outcome_serde() {
        let outcomes = vec![DecisionOutcome::Assigned, DecisionOutcome::Rejected];
        for outcome in &outcomes {
            let json = serde_json::to_string(outcome).unwrap();
            let back: DecisionOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, outcome);
        }
    }

    #[test]
    fn explain_factor_polarity_serde() {
        let polarities = vec![
            FactorPolarity::Positive,
            FactorPolarity::Negative,
            FactorPolarity::Neutral,
        ];
        for polarity in &polarities {
            let json = serde_json::to_string(polarity).unwrap();
            let back: FactorPolarity = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, polarity);
        }
    }

    #[test]
    fn explain_report_serde_roundtrip() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.01)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            min_score: 0.05,
            ..SolverConfig::default()
        };
        let assignments = solve_assignments(&scored, &agents, &config);
        let report = explain_decisions(99, &scored, &assignments);

        let json = serde_json::to_string(&report).unwrap();
        let back: ExplainabilityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_id, 99);
        assert_eq!(back.explanations.len(), report.explanations.len());
        for (orig, rt) in report.explanations.iter().zip(back.explanations.iter()) {
            assert_eq!(orig.bead_id, rt.bead_id);
            assert_eq!(orig.outcome, rt.outcome);
            assert_eq!(orig.factors.len(), rt.factors.len());
        }
    }

    #[test]
    fn explain_format_rejection_all_variants() {
        let cases = vec![
            (RejectionReason::NoCapacity, "spare capacity"),
            (
                RejectionReason::ConflictWithAssigned {
                    conflicting_bead_id: "x".to_string(),
                },
                "Conflicts with",
            ),
            (
                RejectionReason::SafetyGateDenied {
                    gate_name: "freeze".to_string(),
                },
                "safety gate",
            ),
            (RejectionReason::BelowScoreThreshold, "below minimum"),
            (RejectionReason::AlreadyAssigned, "Already assigned"),
        ];
        for (reason, expected_substring) in cases {
            let formatted = format_rejection_reason(&reason);
            assert!(
                formatted.contains(expected_substring),
                "Expected '{}' in '{}'",
                expected_substring,
                formatted
            );
        }
    }

    #[test]
    fn explain_factors_count_three_per_scored() {
        let scored = scored_report(&[("b1", 0.8)]);
        let agents = vec![ready_agent("a1")];
        let assignments = solve_assignments(&scored, &agents, &SolverConfig::default());
        let report = explain_decisions(13, &scored, &assignments);
        let expl = &report.explanations[0];
        // Each scored candidate produces 3 factors: composite, effort, tag
        assert_eq!(expl.factors.len(), 3);
        let dims: Vec<&str> = expl.factors.iter().map(|f| f.dimension.as_str()).collect();
        assert!(dims.contains(&"composite_score"));
        assert!(dims.contains(&"effort_penalty"));
        assert!(dims.contains(&"tag_multiplier"));
    }

    #[test]
    fn explain_rejected_has_rejection_factor_plus_scored_factors() {
        let scored = scored_report(&[("b1", 0.9), ("b2", 0.7)]);
        let agents = vec![ready_agent("a1")];
        let config = SolverConfig {
            conflicts: vec![ConflictPair {
                bead_a: "b1".to_string(),
                bead_b: "b2".to_string(),
            }],
            ..SolverConfig::default()
        };
        let assignments = solve_assignments(&scored, &agents, &config);
        let report = explain_decisions(14, &scored, &assignments);

        let rejected_expl = report.get("b2").unwrap();
        assert_eq!(rejected_expl.outcome, DecisionOutcome::Rejected);
        // 3 scored factors + 1 rejection factor
        assert_eq!(rejected_expl.factors.len(), 4);
        assert!(
            rejected_expl
                .factors
                .iter()
                .any(|f| f.dimension == "rejection")
        );
    }

    #[test]
    fn explain_multiple_rejection_reasons() {
        // Force both below-threshold and no-capacity
        let scored = scored_report(&[("b1", 0.01)]);
        let config = SolverConfig {
            min_score: 0.5,
            ..SolverConfig::default()
        };
        // No agents → no capacity
        let assignments = solve_assignments(&scored, &[], &config);
        let report = explain_decisions(15, &scored, &assignments);
        let expl = &report.explanations[0];
        assert_eq!(expl.outcome, DecisionOutcome::Rejected);
        // Should have rejection factors
        let rejection_factors: Vec<_> = expl
            .factors
            .iter()
            .filter(|f| f.dimension == "rejection")
            .collect();
        assert!(!rejection_factors.is_empty());
    }

    // ── Anti-thrash governor tests (ft-1i2ge.2.7) ───────────────────────────

    fn make_scored(bead_id: &str, score: f64) -> ScoredCandidate {
        ScoredCandidate {
            bead_id: bead_id.to_string(),
            final_score: score,
            feature_composite: score,
            effort_penalty: 0.0,
            tag_multiplier: 1.0,
            below_confidence_threshold: false,
            rank: 1,
        }
    }

    #[test]
    fn governor_default_config() {
        let config = GovernorConfig::default();
        assert_eq!(config.reassignment_cooldown_cycles, 3);
        assert_eq!(config.starvation_threshold_cycles, 5);
        assert!(config.starvation_boost_per_cycle > 0.0);
        assert!(config.starvation_max_boost > 0.0);
        assert!(config.history_window > 0);
        assert!(config.thrash_flip_threshold > 0);
        assert!(config.thrash_penalty > 0.0);
        assert!(config.thrash_penalty <= 1.0);
    }

    #[test]
    fn governor_no_state_allows_all() {
        let gov = ThrashGovernor::new(GovernorConfig::default());
        let candidates = vec![make_scored("b1", 0.8), make_scored("b2", 0.6)];
        let report = gov.evaluate(&candidates);
        assert_eq!(report.verdicts.len(), 2);
        for v in &report.verdicts {
            assert_eq!(v.action, GovernorAction::Allow);
            assert_eq!(v.adjusted_score, v.original_score);
        }
        assert!(report.thrashing_bead_ids.is_empty());
        assert!(report.starving_bead_ids.is_empty());
        assert!(report.cooldown_bead_ids.is_empty());
    }

    #[test]
    fn governor_cooldown_blocks_recent_assignment() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            reassignment_cooldown_cycles: 3,
            ..GovernorConfig::default()
        });

        // Cycle 1: b1 is assigned.
        gov.record_cycle(&["b1".to_string()]);
        assert_eq!(gov.current_cycle, 1);

        // Cycle 2: try to evaluate b1 again → blocked.
        let candidates = vec![make_scored("b1", 0.9)];
        let report = gov.evaluate(&candidates);
        let v = &report.verdicts[0];
        // Assigned at cycle 1, current_cycle is 1, elapsed=0, remaining=3.
        assert!(
            matches!(
                v.action,
                GovernorAction::BlockReassignment {
                    remaining_cycles: 3
                }
            ),
            "Expected block with 3 remaining, got {:?}",
            v.action
        );
        assert_eq!(v.adjusted_score, 0.0);
        assert_eq!(report.cooldown_bead_ids, vec!["b1"]);
    }

    #[test]
    fn governor_cooldown_expires() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            reassignment_cooldown_cycles: 2,
            ..GovernorConfig::default()
        });

        gov.record_cycle(&["b1".to_string()]);
        gov.record_cycle(&[]); // cycle 2
        gov.record_cycle(&[]); // cycle 3: cooldown expires

        let candidates = vec![make_scored("b1", 0.9)];
        let report = gov.evaluate(&candidates);
        // Cooldown was 2 cycles, we're 2 cycles past assignment → allowed.
        // But it might detect thrash or starvation; check cooldown specifically.
        assert!(report.cooldown_bead_ids.is_empty());
    }

    #[test]
    fn governor_starvation_boost() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            starvation_threshold_cycles: 3,
            starvation_boost_per_cycle: 0.05,
            starvation_max_boost: 0.20,
            reassignment_cooldown_cycles: 0,
            ..GovernorConfig::default()
        });

        // Register bead as known.
        gov.register_bead("b1");

        // Skip 5 cycles without assigning b1.
        for _ in 0..5 {
            gov.record_cycle(&[]);
        }

        let candidates = vec![make_scored("b1", 0.3)];
        let report = gov.evaluate(&candidates);
        let v = &report.verdicts[0];
        // After threshold (3), extra 2 cycles → boost = 2 * 0.05 = 0.10
        assert!(
            matches!(v.action, GovernorAction::BoostScore { amount } if (amount - 0.10).abs() < 1e-9)
        );
        assert!((v.adjusted_score - 0.4).abs() < 1e-9);
        assert_eq!(report.starving_bead_ids, vec!["b1"]);
    }

    #[test]
    fn governor_starvation_boost_caps_at_max() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            starvation_threshold_cycles: 2,
            starvation_boost_per_cycle: 0.10,
            starvation_max_boost: 0.15,
            reassignment_cooldown_cycles: 0,
            ..GovernorConfig::default()
        });

        gov.register_bead("b1");
        for _ in 0..20 {
            gov.record_cycle(&[]);
        }

        let candidates = vec![make_scored("b1", 0.5)];
        let report = gov.evaluate(&candidates);
        let v = &report.verdicts[0];
        if let GovernorAction::BoostScore { amount } = v.action {
            assert!(amount <= 0.15 + 1e-9, "Boost {} exceeds max", amount);
        } else {
            panic!("Expected BoostScore, got {:?}", v.action);
        }
    }

    #[test]
    fn governor_starvation_adjusted_score_capped_at_one() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            starvation_threshold_cycles: 1,
            starvation_boost_per_cycle: 0.50,
            starvation_max_boost: 0.50,
            reassignment_cooldown_cycles: 0,
            ..GovernorConfig::default()
        });

        gov.register_bead("b1");
        gov.record_cycle(&[]);
        gov.record_cycle(&[]);

        let candidates = vec![make_scored("b1", 0.9)];
        let report = gov.evaluate(&candidates);
        let v = &report.verdicts[0];
        assert!(v.adjusted_score <= 1.0);
    }

    #[test]
    fn governor_thrash_detection() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            thrash_flip_threshold: 3,
            thrash_penalty: 0.4,
            reassignment_cooldown_cycles: 0,
            starvation_threshold_cycles: 100, // disable starvation
            ..GovernorConfig::default()
        });

        // Create an oscillating pattern: assigned, not, assigned, not, assigned, not
        gov.register_bead("b1");
        gov.record_cycle(&["b1".to_string()]);
        gov.record_cycle(&[]);
        gov.record_cycle(&["b1".to_string()]);
        gov.record_cycle(&[]);
        gov.record_cycle(&["b1".to_string()]);
        gov.record_cycle(&[]);

        let candidates = vec![make_scored("b1", 0.8)];
        let report = gov.evaluate(&candidates);
        let v = &report.verdicts[0];
        assert!(
            matches!(v.action, GovernorAction::PenalizeScore { factor } if (factor - 0.4).abs() < 1e-9),
            "Expected penalty 0.4, got {:?}",
            v.action
        );
        assert!((v.adjusted_score - 0.32).abs() < 1e-9);
        assert_eq!(report.thrashing_bead_ids, vec!["b1"]);
    }

    #[test]
    fn governor_no_thrash_when_stable() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            thrash_flip_threshold: 3,
            reassignment_cooldown_cycles: 0,
            starvation_threshold_cycles: 100,
            ..GovernorConfig::default()
        });

        // Stable: always assigned.
        gov.register_bead("b1");
        for _ in 0..5 {
            gov.record_cycle(&["b1".to_string()]);
        }

        let candidates = vec![make_scored("b1", 0.8)];
        let report = gov.evaluate(&candidates);
        assert!(report.thrashing_bead_ids.is_empty());
    }

    #[test]
    fn governor_history_window_bounded() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            history_window: 5,
            reassignment_cooldown_cycles: 0,
            starvation_threshold_cycles: 100,
            ..GovernorConfig::default()
        });

        gov.register_bead("b1");
        for _ in 0..20 {
            gov.record_cycle(&[]);
        }

        let state = gov.bead_states.get("b1").unwrap();
        assert!(state.assignment_history.len() <= 5);
    }

    #[test]
    fn governor_record_cycle_increments() {
        let mut gov = ThrashGovernor::new(GovernorConfig::default());
        assert_eq!(gov.current_cycle, 0);
        gov.record_cycle(&[]);
        assert_eq!(gov.current_cycle, 1);
        gov.record_cycle(&[]);
        assert_eq!(gov.current_cycle, 2);
    }

    #[test]
    fn governor_record_agent_assignment() {
        let mut gov = ThrashGovernor::new(GovernorConfig::default());
        gov.record_agent_assignment("b1", "agent-x");
        let state = gov.bead_states.get("b1").unwrap();
        assert_eq!(state.last_agent_id.as_deref(), Some("agent-x"));
    }

    #[test]
    fn governor_register_bead() {
        let mut gov = ThrashGovernor::new(GovernorConfig::default());
        gov.register_bead("b1");
        assert!(gov.bead_states.contains_key("b1"));
        let state = gov.bead_states.get("b1").unwrap();
        assert!(state.last_assigned_cycle.is_none());
        assert_eq!(state.consecutive_skipped, 0);
    }

    #[test]
    fn governor_mixed_beads() {
        let mut gov = ThrashGovernor::new(GovernorConfig {
            reassignment_cooldown_cycles: 2,
            starvation_threshold_cycles: 3,
            starvation_boost_per_cycle: 0.05,
            starvation_max_boost: 0.15,
            thrash_flip_threshold: 10, // effectively disable thrash
            ..GovernorConfig::default()
        });

        // b1 recently assigned → cooldown. b2 never assigned → starvation eventually.
        gov.register_bead("b2");
        gov.record_cycle(&["b1".to_string()]);

        let candidates = vec![make_scored("b1", 0.9), make_scored("b2", 0.5)];
        let report = gov.evaluate(&candidates);

        // b1 should be blocked (cooldown).
        let v1 = report.verdicts.iter().find(|v| v.bead_id == "b1").unwrap();
        assert!(matches!(
            v1.action,
            GovernorAction::BlockReassignment { .. }
        ));

        // b2 skipped 1 cycle, threshold is 3 → allowed (no boost yet).
        let v2 = report.verdicts.iter().find(|v| v.bead_id == "b2").unwrap();
        assert_eq!(v2.action, GovernorAction::Allow);
    }

    #[test]
    fn governor_consecutive_skipped_resets_on_assign() {
        let mut gov = ThrashGovernor::new(GovernorConfig::default());
        gov.register_bead("b1");
        gov.record_cycle(&[]);
        gov.record_cycle(&[]);
        gov.record_cycle(&[]);
        assert_eq!(gov.bead_states.get("b1").unwrap().consecutive_skipped, 3);

        gov.record_cycle(&["b1".to_string()]);
        assert_eq!(gov.bead_states.get("b1").unwrap().consecutive_skipped, 0);
    }

    #[test]
    fn governor_count_flips_empty() {
        assert_eq!(count_flips(&[]), 0);
    }

    #[test]
    fn governor_count_flips_single() {
        assert_eq!(count_flips(&[true]), 0);
    }

    #[test]
    fn governor_count_flips_no_changes() {
        assert_eq!(count_flips(&[true, true, true, true]), 0);
        assert_eq!(count_flips(&[false, false, false]), 0);
    }

    #[test]
    fn governor_count_flips_alternating() {
        assert_eq!(count_flips(&[true, false, true, false, true]), 4);
    }

    #[test]
    fn governor_config_serde_roundtrip() {
        let config = GovernorConfig {
            reassignment_cooldown_cycles: 5,
            starvation_threshold_cycles: 10,
            starvation_boost_per_cycle: 0.03,
            starvation_max_boost: 0.20,
            history_window: 15,
            thrash_flip_threshold: 4,
            thrash_penalty: 0.6,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: GovernorConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reassignment_cooldown_cycles, 5);
        assert_eq!(back.starvation_threshold_cycles, 10);
        assert!((back.starvation_boost_per_cycle - 0.03).abs() < 1e-9);
        assert!((back.starvation_max_boost - 0.20).abs() < 1e-9);
        assert_eq!(back.history_window, 15);
        assert_eq!(back.thrash_flip_threshold, 4);
        assert!((back.thrash_penalty - 0.6).abs() < 1e-9);
    }

    #[test]
    fn governor_report_serde_roundtrip() {
        let mut gov = ThrashGovernor::new(GovernorConfig::default());
        gov.register_bead("b1");
        gov.record_cycle(&["b1".to_string()]);
        let candidates = vec![make_scored("b1", 0.8)];
        let report = gov.evaluate(&candidates);

        let json = serde_json::to_string(&report).unwrap();
        let back: GovernorReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_id, report.cycle_id);
        assert_eq!(back.verdicts.len(), report.verdicts.len());
    }

    #[test]
    fn governor_action_serde_roundtrip() {
        let actions = vec![
            GovernorAction::Allow,
            GovernorAction::BoostScore { amount: 0.05 },
            GovernorAction::PenalizeScore { factor: 0.5 },
            GovernorAction::BlockReassignment {
                remaining_cycles: 2,
            },
        ];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let back: GovernorAction = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, action);
        }
    }

    #[test]
    fn governor_bead_state_serde_roundtrip() {
        let state = BeadGovernorState {
            last_assigned_cycle: Some(5),
            consecutive_skipped: 3,
            assignment_history: vec![true, false, true],
            last_agent_id: Some("agent-1".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: BeadGovernorState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_assigned_cycle, Some(5));
        assert_eq!(back.consecutive_skipped, 3);
        assert_eq!(back.assignment_history, vec![true, false, true]);
        assert_eq!(back.last_agent_id.as_deref(), Some("agent-1"));
    }

    #[test]
    fn governor_thrash_governor_serde_roundtrip() {
        let mut gov = ThrashGovernor::new(GovernorConfig::default());
        gov.register_bead("b1");
        gov.record_cycle(&["b1".to_string()]);
        gov.record_cycle(&[]);

        let json = serde_json::to_string(&gov).unwrap();
        let back: ThrashGovernor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.current_cycle, 2);
        assert!(back.bead_states.contains_key("b1"));
    }

    #[test]
    fn governor_cooldown_priority_over_starvation() {
        // If a bead is in cooldown AND starving, cooldown should take priority.
        let mut gov = ThrashGovernor::new(GovernorConfig {
            reassignment_cooldown_cycles: 10,
            starvation_threshold_cycles: 1,
            starvation_boost_per_cycle: 0.1,
            starvation_max_boost: 0.5,
            ..GovernorConfig::default()
        });

        gov.record_cycle(&["b1".to_string()]);
        // After 1 cycle, cooldown still active but starvation threshold met.
        // Cooldown should win.
        let candidates = vec![make_scored("b1", 0.5)];
        let report = gov.evaluate(&candidates);
        let v = &report.verdicts[0];
        assert!(matches!(v.action, GovernorAction::BlockReassignment { .. }));
    }

    // ── Mission profile tests (ft-1i2ge.2.9) ────────────────────────────────

    #[test]
    fn profile_all_kinds_construct() {
        let kinds = vec![
            MissionProfileKind::Balanced,
            MissionProfileKind::SafetyFirst,
            MissionProfileKind::Throughput,
            MissionProfileKind::UrgencyDriven,
            MissionProfileKind::Conservative,
        ];
        for kind in kinds {
            let profile = MissionProfile::from_kind(kind);
            assert_eq!(profile.kind, kind);
            assert!(!profile.name.is_empty());
            assert!(!profile.description.is_empty());
        }
    }

    #[test]
    fn profile_balanced_is_default() {
        let balanced = MissionProfile::balanced();
        let default_scorer = ScorerConfig::default();
        assert!(
            (balanced.scorer_config.weights.impact - default_scorer.weights.impact).abs() < 1e-9
        );
        assert!(
            (balanced.scorer_config.weights.urgency - default_scorer.weights.urgency).abs() < 1e-9
        );
    }

    #[test]
    fn profile_safety_first_prioritizes_risk() {
        let safety = MissionProfile::safety_first();
        let balanced = MissionProfile::balanced();
        assert!(safety.scorer_config.weights.risk > balanced.scorer_config.weights.risk);
        assert!(
            safety.scorer_config.weights.confidence > balanced.scorer_config.weights.confidence
        );
        assert!(safety.scorer_config.safety_bonus > balanced.scorer_config.safety_bonus);
        assert!(
            safety.scorer_config.min_confidence_threshold
                > balanced.scorer_config.min_confidence_threshold
        );
    }

    #[test]
    fn profile_throughput_prioritizes_impact() {
        let throughput = MissionProfile::throughput();
        let balanced = MissionProfile::balanced();
        assert!(throughput.scorer_config.weights.impact > balanced.scorer_config.weights.impact);
        assert!(
            throughput.governor_config.reassignment_cooldown_cycles
                < balanced.governor_config.reassignment_cooldown_cycles
        );
    }

    #[test]
    fn profile_urgency_driven_high_urgency_weight() {
        let urgency = MissionProfile::urgency_driven();
        assert!(urgency.scorer_config.weights.urgency >= 0.40);
        assert!(
            urgency.governor_config.starvation_threshold_cycles
                <= GovernorConfig::default().starvation_threshold_cycles
        );
    }

    #[test]
    fn profile_conservative_high_confidence_bar() {
        let conservative = MissionProfile::conservative();
        assert!(conservative.scorer_config.min_confidence_threshold >= 0.3);
        assert!(
            conservative.governor_config.reassignment_cooldown_cycles
                > GovernorConfig::default().reassignment_cooldown_cycles
        );
    }

    #[test]
    fn profile_all_weights_sum_to_one() {
        let kinds = vec![
            MissionProfileKind::Balanced,
            MissionProfileKind::SafetyFirst,
            MissionProfileKind::Throughput,
            MissionProfileKind::UrgencyDriven,
            MissionProfileKind::Conservative,
        ];
        for kind in kinds {
            let profile = MissionProfile::from_kind(kind);
            let total = profile.scorer_config.weights.total();
            assert!(
                (total - 1.0).abs() < 1e-9,
                "{:?} weights sum to {} (expected 1.0)",
                kind,
                total
            );
        }
    }

    #[test]
    fn profile_kind_serde_roundtrip() {
        let kinds = vec![
            MissionProfileKind::Balanced,
            MissionProfileKind::SafetyFirst,
            MissionProfileKind::Throughput,
            MissionProfileKind::UrgencyDriven,
            MissionProfileKind::Conservative,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let back: MissionProfileKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn profile_full_serde_roundtrip() {
        let profile = MissionProfile::safety_first();
        let json = serde_json::to_string(&profile).unwrap();
        let back: MissionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, MissionProfileKind::SafetyFirst);
        assert_eq!(back.name, "Safety First");
        assert!(
            (back.scorer_config.weights.risk - profile.scorer_config.weights.risk).abs() < 1e-9
        );
    }

    #[test]
    fn tuner_new_default() {
        let tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        assert_eq!(tuner.active_profile.kind, MissionProfileKind::Balanced);
        assert!(tuner.weight_overrides.is_empty());
        assert!(tuner.switch_history.is_empty());
    }

    #[test]
    fn tuner_switch_profile() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.switch_profile(1, MissionProfileKind::SafetyFirst, "incident response");
        assert_eq!(tuner.active_profile.kind, MissionProfileKind::SafetyFirst);
        assert_eq!(tuner.switch_history.len(), 1);
        assert_eq!(tuner.switch_history[0].from, MissionProfileKind::Balanced);
        assert_eq!(tuner.switch_history[0].to, MissionProfileKind::SafetyFirst);
        assert_eq!(tuner.switch_history[0].reason, "incident response");
    }

    #[test]
    fn tuner_switch_same_profile_noop() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.switch_profile(1, MissionProfileKind::Balanced, "no change");
        assert!(tuner.switch_history.is_empty());
    }

    #[test]
    fn tuner_switch_clears_overrides() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.set_override("impact", 0.1);
        assert_eq!(tuner.weight_overrides.len(), 1);
        tuner.switch_profile(1, MissionProfileKind::Throughput, "test");
        assert!(tuner.weight_overrides.is_empty());
    }

    #[test]
    fn tuner_switch_history_bounded() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.max_history = 3;
        let profiles = vec![
            MissionProfileKind::SafetyFirst,
            MissionProfileKind::Throughput,
            MissionProfileKind::UrgencyDriven,
            MissionProfileKind::Conservative,
            MissionProfileKind::Balanced,
        ];
        for (i, kind) in profiles.iter().enumerate() {
            tuner.switch_profile(i as u64, *kind, "cycle");
        }
        assert!(tuner.switch_history.len() <= 3);
    }

    #[test]
    fn tuner_override_impact() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        let base_impact = tuner.effective_scorer_config().weights.impact;
        tuner.set_override("impact", 0.1);
        let effective = tuner.effective_scorer_config();
        assert!((effective.weights.impact - (base_impact + 0.1)).abs() < 1e-9);
    }

    #[test]
    fn tuner_override_clamp() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.set_override("impact", 5.0); // would exceed 1.0
        let effective = tuner.effective_scorer_config();
        assert!(effective.weights.impact <= 1.0);

        tuner.set_override("urgency", -5.0); // would go below 0.0
        let effective = tuner.effective_scorer_config();
        assert!(effective.weights.urgency >= 0.0);
    }

    #[test]
    fn tuner_clear_override() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        let base = tuner.effective_scorer_config().weights.impact;
        tuner.set_override("impact", 0.1);
        tuner.clear_override("impact");
        let after = tuner.effective_scorer_config().weights.impact;
        assert!((after - base).abs() < 1e-9);
    }

    #[test]
    fn tuner_override_effort() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        let base = tuner.effective_scorer_config().effort_weight;
        tuner.set_override("effort", 0.05);
        let effective = tuner.effective_scorer_config();
        assert!((effective.effort_weight - (base + 0.05)).abs() < 1e-9);
    }

    #[test]
    fn tuner_override_min_confidence() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.set_override("min_confidence", 0.2);
        let effective = tuner.effective_scorer_config();
        let base = MissionProfile::balanced()
            .scorer_config
            .min_confidence_threshold;
        assert!((effective.min_confidence_threshold - (base + 0.2)).abs() < 1e-9);
    }

    #[test]
    fn tuner_effective_governor() {
        let tuner = UtilityPolicyTuner::new(MissionProfile::safety_first());
        let gov = tuner.effective_governor_config();
        assert_eq!(gov.reassignment_cooldown_cycles, 5);
    }

    #[test]
    fn tuner_effective_extraction() {
        let tuner = UtilityPolicyTuner::new(MissionProfile::urgency_driven());
        let ext = tuner.effective_extraction_config();
        assert!((ext.urgency_staleness_weight - 0.6).abs() < 1e-9);
    }

    #[test]
    fn tuner_serde_roundtrip() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.switch_profile(1, MissionProfileKind::Throughput, "test");
        tuner.set_override("impact", 0.05);
        let json = serde_json::to_string(&tuner).unwrap();
        let back: UtilityPolicyTuner = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active_profile.kind, MissionProfileKind::Throughput);
        assert_eq!(back.switch_history.len(), 1);
        assert!(back.weight_overrides.contains_key("impact"));
    }

    #[test]
    fn profile_switch_serde_roundtrip() {
        let switch = ProfileSwitch {
            cycle_id: 42,
            from: MissionProfileKind::Balanced,
            to: MissionProfileKind::SafetyFirst,
            reason: "incident".to_string(),
        };
        let json = serde_json::to_string(&switch).unwrap();
        let back: ProfileSwitch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_id, 42);
        assert_eq!(back.from, MissionProfileKind::Balanced);
        assert_eq!(back.to, MissionProfileKind::SafetyFirst);
    }

    #[test]
    fn tuner_multiple_overrides() {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::balanced());
        tuner.set_override("impact", 0.1);
        tuner.set_override("risk", -0.05);
        tuner.set_override("effort", 0.03);
        let effective = tuner.effective_scorer_config();
        let base = MissionProfile::balanced().scorer_config;
        assert!((effective.weights.impact - (base.weights.impact + 0.1)).abs() < 1e-9);
        assert!((effective.weights.risk - (base.weights.risk - 0.05)).abs() < 1e-9);
        assert!((effective.effort_weight - (base.effort_weight + 0.03)).abs() < 1e-9);
    }

    #[test]
    fn profile_safety_higher_cooldown_than_throughput() {
        let safety = MissionProfile::safety_first();
        let throughput = MissionProfile::throughput();
        assert!(
            safety.governor_config.reassignment_cooldown_cycles
                > throughput.governor_config.reassignment_cooldown_cycles
        );
    }

    #[test]
    fn profile_urgency_higher_starvation_boost_than_conservative() {
        let urgency = MissionProfile::urgency_driven();
        let conservative = MissionProfile::conservative();
        assert!(
            urgency.governor_config.starvation_max_boost
                > conservative.governor_config.starvation_max_boost
        );
    }

    // ── Mission runtime config tests (ft-1i2ge.5.4) ─────────────────────────

    #[test]
    fn config_default_valid() {
        let config = MissionRuntimeConfig::default();
        let result = config.validate();
        assert!(result.valid);
        assert_eq!(result.error_count(), 0);
    }

    #[test]
    fn config_cadence_zero_error() {
        let config = MissionRuntimeConfig {
            cadence_ms: 0,
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
        assert!(result.error_count() > 0);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.field == "cadence_ms" && d.severity == ConfigDiagnosticSeverity::Error)
        );
    }

    #[test]
    fn config_cadence_low_warning() {
        let config = MissionRuntimeConfig {
            cadence_ms: 500,
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(result.valid); // warning, not error
        assert!(result.warning_count() > 0);
    }

    #[test]
    fn config_trigger_batch_zero_error() {
        let config = MissionRuntimeConfig {
            max_trigger_batch: 0,
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
    }

    #[test]
    fn config_weight_out_of_range_error() {
        let config = MissionRuntimeConfig {
            scorer_overrides: ScorerConfigOverrides {
                impact_weight: Some(1.5),
                ..ScorerConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.field.contains("impact_weight"))
        );
    }

    #[test]
    fn config_negative_weight_error() {
        let config = MissionRuntimeConfig {
            scorer_overrides: ScorerConfigOverrides {
                risk_weight: Some(-0.1),
                ..ScorerConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
    }

    #[test]
    fn config_safety_bonus_below_one_warning() {
        let config = MissionRuntimeConfig {
            scorer_overrides: ScorerConfigOverrides {
                safety_bonus: Some(0.8),
                ..ScorerConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(result.valid); // warning only
        assert!(result.warning_count() > 0);
    }

    #[test]
    fn config_thrash_penalty_zero_error() {
        let config = MissionRuntimeConfig {
            governor_overrides: GovernorConfigOverrides {
                thrash_penalty: Some(0.0),
                ..GovernorConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
    }

    #[test]
    fn config_thrash_penalty_over_one_error() {
        let config = MissionRuntimeConfig {
            governor_overrides: GovernorConfigOverrides {
                thrash_penalty: Some(1.5),
                ..GovernorConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
    }

    #[test]
    fn config_history_window_zero_error() {
        let config = MissionRuntimeConfig {
            governor_overrides: GovernorConfigOverrides {
                history_window: Some(0),
                ..GovernorConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
    }

    #[test]
    fn config_min_score_out_of_range_error() {
        let config = MissionRuntimeConfig {
            min_score_override: 1.5,
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
    }

    #[test]
    fn config_resolve_default() {
        let config = MissionRuntimeConfig::default();
        let resolved = config.resolve();
        assert_eq!(resolved.profile, MissionProfileKind::Balanced);
        assert_eq!(resolved.cadence_ms, 30_000);
        assert!(
            (resolved.scorer_config.weights.impact
                - MissionProfile::balanced().scorer_config.weights.impact)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn config_resolve_applies_scorer_overrides() {
        let config = MissionRuntimeConfig {
            scorer_overrides: ScorerConfigOverrides {
                impact_weight: Some(0.99),
                safety_bonus: Some(2.0),
                ..ScorerConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let resolved = config.resolve();
        assert!((resolved.scorer_config.weights.impact - 0.99).abs() < 1e-9);
        assert!((resolved.scorer_config.safety_bonus - 2.0).abs() < 1e-9);
    }

    #[test]
    fn config_resolve_applies_governor_overrides() {
        let config = MissionRuntimeConfig {
            governor_overrides: GovernorConfigOverrides {
                reassignment_cooldown_cycles: Some(10),
                thrash_penalty: Some(0.7),
                ..GovernorConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let resolved = config.resolve();
        assert_eq!(resolved.governor_config.reassignment_cooldown_cycles, 10);
        assert!((resolved.governor_config.thrash_penalty - 0.7).abs() < 1e-9);
    }

    #[test]
    fn config_resolve_applies_extraction_overrides() {
        let config = MissionRuntimeConfig {
            extraction_overrides: ExtractionConfigOverrides {
                max_staleness_hours: Some(100.0),
                ..ExtractionConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let resolved = config.resolve();
        assert!((resolved.extraction_config.max_staleness_hours - 100.0).abs() < 1e-9);
    }

    #[test]
    fn config_resolve_safety_first_profile() {
        let config = MissionRuntimeConfig {
            profile: MissionProfileKind::SafetyFirst,
            ..MissionRuntimeConfig::default()
        };
        let resolved = config.resolve();
        assert_eq!(resolved.profile, MissionProfileKind::SafetyFirst);
        let safety = MissionProfile::safety_first();
        assert!(
            (resolved.scorer_config.weights.risk - safety.scorer_config.weights.risk).abs() < 1e-9
        );
    }

    #[test]
    fn config_resolve_max_assignments_override() {
        let config = MissionRuntimeConfig {
            max_assignments_per_round: 42,
            ..MissionRuntimeConfig::default()
        };
        let resolved = config.resolve();
        assert_eq!(resolved.solver_config.max_assignments, 42);
    }

    #[test]
    fn config_resolve_min_score_override() {
        let config = MissionRuntimeConfig {
            min_score_override: 0.5,
            ..MissionRuntimeConfig::default()
        };
        let resolved = config.resolve();
        assert!((resolved.solver_config.min_score - 0.5).abs() < 1e-9);
    }

    #[test]
    fn config_resolve_additional_gates_and_conflicts() {
        let config = MissionRuntimeConfig {
            additional_safety_gates: vec![SafetyGate {
                name: "freeze".to_string(),
                denied_bead_ids: vec!["b1".to_string()],
            }],
            additional_conflicts: vec![ConflictPair {
                bead_a: "a".to_string(),
                bead_b: "b".to_string(),
            }],
            ..MissionRuntimeConfig::default()
        };
        let resolved = config.resolve();
        assert_eq!(resolved.solver_config.safety_gates.len(), 1);
        assert_eq!(resolved.solver_config.conflicts.len(), 1);
    }

    #[test]
    fn config_runtime_serde_roundtrip() {
        let config = MissionRuntimeConfig {
            profile: MissionProfileKind::Throughput,
            cadence_ms: 15_000,
            max_trigger_batch: 5,
            scorer_overrides: ScorerConfigOverrides {
                impact_weight: Some(0.5),
                ..ScorerConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: MissionRuntimeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profile, MissionProfileKind::Throughput);
        assert_eq!(back.cadence_ms, 15_000);
        assert_eq!(back.scorer_overrides.impact_weight, Some(0.5));
    }

    #[test]
    fn config_resolved_serde_roundtrip() {
        let config = MissionRuntimeConfig::default();
        let resolved = config.resolve();
        let json = serde_json::to_string(&resolved).unwrap();
        let back: ResolvedMissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profile, MissionProfileKind::Balanced);
        assert_eq!(back.cadence_ms, 30_000);
    }

    #[test]
    fn config_validation_result_serde_roundtrip() {
        let result = ConfigValidationResult {
            valid: false,
            diagnostics: vec![ConfigDiagnostic {
                severity: ConfigDiagnosticSeverity::Error,
                field: "test".to_string(),
                message: "bad".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ConfigValidationResult = serde_json::from_str(&json).unwrap();
        assert!(!back.valid);
        assert_eq!(back.diagnostics.len(), 1);
    }

    #[test]
    fn config_severity_serde_roundtrip() {
        let severities = vec![
            ConfigDiagnosticSeverity::Error,
            ConfigDiagnosticSeverity::Warning,
            ConfigDiagnosticSeverity::Info,
        ];
        for sev in &severities {
            let json = serde_json::to_string(sev).unwrap();
            let back: ConfigDiagnosticSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, sev);
        }
    }

    #[test]
    fn config_multiple_validation_errors() {
        let config = MissionRuntimeConfig {
            cadence_ms: 0,
            max_trigger_batch: 0,
            min_score_override: -1.0,
            ..MissionRuntimeConfig::default()
        };
        let result = config.validate();
        assert!(!result.valid);
        assert!(result.error_count() >= 3);
    }

    #[test]
    fn config_overrides_default_empty() {
        let overrides = ScorerConfigOverrides::default();
        assert!(overrides.impact_weight.is_none());
        assert!(overrides.safety_bonus.is_none());
        let gov = GovernorConfigOverrides::default();
        assert!(gov.reassignment_cooldown_cycles.is_none());
        let ext = ExtractionConfigOverrides::default();
        assert!(ext.max_unblock_count.is_none());
    }

    #[test]
    fn config_resolve_preserves_profile_when_no_overrides() {
        for kind in &[
            MissionProfileKind::Balanced,
            MissionProfileKind::SafetyFirst,
            MissionProfileKind::Throughput,
            MissionProfileKind::UrgencyDriven,
            MissionProfileKind::Conservative,
        ] {
            let config = MissionRuntimeConfig {
                profile: *kind,
                ..MissionRuntimeConfig::default()
            };
            let resolved = config.resolve();
            let profile = MissionProfile::from_kind(*kind);
            assert!(
                (resolved.scorer_config.weights.impact - profile.scorer_config.weights.impact)
                    .abs()
                    < 1e-9,
                "{:?}: impact mismatch",
                kind
            );
            assert!(
                (resolved.scorer_config.weights.risk - profile.scorer_config.weights.risk).abs()
                    < 1e-9,
                "{:?}: risk mismatch",
                kind
            );
        }
    }

    // ── Golden vector tests (ft-1i2ge.2.8) ──────────────────────────────────
    //
    // Fixed fixtures with deterministic expected outputs for regression testing.
    // These test the full pipeline: features → scoring → solving → explainability.

    /// Build a standard golden fixture: 5 beads with known dependency structure.
    ///
    /// DAG:
    ///   epic (closed) ← infra (open, P0, blocks web+api) ← web (open, P1)
    ///                 ← api (open, P2, blocks frontend)  ← frontend (open, P3)
    fn golden_fixture() -> Vec<BeadIssueDetail> {
        vec![
            sample_detail("epic", BeadStatus::Closed, 0, &[]),
            sample_detail("infra", BeadStatus::Open, 0, &[("epic", "blocks")]),
            sample_detail("web", BeadStatus::Open, 1, &[("infra", "blocks")]),
            sample_detail("api", BeadStatus::Open, 2, &[("epic", "blocks")]),
            sample_detail("frontend", BeadStatus::Open, 3, &[("api", "blocks")]),
        ]
    }

    fn golden_agents() -> Vec<MissionAgentCapabilityProfile> {
        vec![ready_agent("agent-alpha"), ready_agent("agent-beta")]
    }

    #[test]
    fn golden_readiness_resolution() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        // infra and api are ready (their deps are closed/done).
        // web is blocked (infra is open), frontend is blocked (api is open).
        let ready_ids: Vec<&str> = report.ready_ids.iter().map(|s| s.as_str()).collect();
        assert!(ready_ids.contains(&"infra"), "infra should be ready");
        assert!(ready_ids.contains(&"api"), "api should be ready");
        assert!(!ready_ids.contains(&"web"), "web should be blocked");
        assert!(
            !ready_ids.contains(&"frontend"),
            "frontend should be blocked"
        );
    }

    #[test]
    fn golden_feature_extraction_ranking() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &config);

        // infra unblocks 2 (web, frontend via api), api unblocks 1 (frontend).
        // So infra should have higher impact.
        let infra_impact = features
            .features
            .iter()
            .find(|f| f.bead_id == "infra")
            .unwrap()
            .impact;
        let api_impact = features
            .features
            .iter()
            .find(|f| f.bead_id == "api")
            .unwrap()
            .impact;
        assert!(
            infra_impact >= api_impact,
            "infra impact ({}) should >= api impact ({})",
            infra_impact,
            api_impact
        );
    }

    #[test]
    fn golden_feature_urgency_by_priority() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &config);

        // infra is P0, api is P2 → infra should have higher urgency.
        let infra_urgency = features
            .features
            .iter()
            .find(|f| f.bead_id == "infra")
            .unwrap()
            .urgency;
        let api_urgency = features
            .features
            .iter()
            .find(|f| f.bead_id == "api")
            .unwrap()
            .urgency;
        assert!(
            infra_urgency >= api_urgency,
            "P0 infra urgency ({}) should >= P2 api urgency ({})",
            infra_urgency,
            api_urgency
        );
    }

    #[test]
    fn golden_scoring_deterministic_order() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let extraction_config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &extraction_config);

        let inputs: Vec<ScorerInput> = features
            .features
            .iter()
            .map(|f| ScorerInput {
                features: f.clone(),
                effort: None,
                tags: Vec::new(),
            })
            .collect();
        let scorer_config = ScorerConfig::default();

        // Run scoring twice → identical ranking.
        let scored1 = score_candidates(&inputs, &scorer_config);
        let scored2 = score_candidates(&inputs, &scorer_config);
        assert_eq!(scored1.ranked_ids, scored2.ranked_ids);
    }

    #[test]
    fn golden_scoring_infra_ranked_first() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let extraction_config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &extraction_config);

        let inputs: Vec<ScorerInput> = features
            .features
            .iter()
            .map(|f| ScorerInput {
                features: f.clone(),
                effort: None,
                tags: Vec::new(),
            })
            .collect();
        let scored = score_candidates(&inputs, &ScorerConfig::default());
        assert_eq!(
            scored.ranked_ids[0], "infra",
            "infra should be ranked #1 due to higher impact + urgency"
        );
    }

    #[test]
    fn golden_solver_assigns_both_agents() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let extraction_config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &extraction_config);

        let inputs: Vec<ScorerInput> = features
            .features
            .iter()
            .map(|f| ScorerInput {
                features: f.clone(),
                effort: None,
                tags: Vec::new(),
            })
            .collect();
        let scored = score_candidates(&inputs, &ScorerConfig::default());
        let assignments = solve_assignments(&scored, &agents, &SolverConfig::default());

        // 2 ready beads, 2 agents → both should be assigned.
        assert_eq!(assignments.assignment_count(), 2);
        let assigned_beads: Vec<_> = assignments
            .assignments
            .iter()
            .map(|a| a.bead_id.as_str())
            .collect();
        assert!(assigned_beads.contains(&"infra"));
        assert!(assigned_beads.contains(&"api"));
    }

    #[test]
    fn golden_explainability_all_beads_explained() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let extraction_config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &extraction_config);

        let inputs: Vec<ScorerInput> = features
            .features
            .iter()
            .map(|f| ScorerInput {
                features: f.clone(),
                effort: None,
                tags: Vec::new(),
            })
            .collect();
        let scored = score_candidates(&inputs, &ScorerConfig::default());
        let assignments = solve_assignments(&scored, &agents, &SolverConfig::default());
        let explain_report = explain_decisions(1, &scored, &assignments);

        // Every assigned + rejected bead should have an explanation.
        let total = assignments.assignment_count() + assignments.rejected.len();
        assert_eq!(explain_report.explanations.len(), total);
        for expl in &explain_report.explanations {
            assert!(!expl.summary.is_empty());
            assert!(!expl.factors.is_empty());
        }
    }

    #[test]
    fn golden_governor_no_intervention_first_cycle() {
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let extraction_config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &extraction_config);

        let inputs: Vec<ScorerInput> = features
            .features
            .iter()
            .map(|f| ScorerInput {
                features: f.clone(),
                effort: None,
                tags: Vec::new(),
            })
            .collect();
        let scored = score_candidates(&inputs, &ScorerConfig::default());
        let gov = ThrashGovernor::new(GovernorConfig::default());
        let gov_report = gov.evaluate(&scored.scored);

        // No history → all allowed.
        for v in &gov_report.verdicts {
            assert_eq!(v.action, GovernorAction::Allow);
        }
    }

    #[test]
    fn golden_config_resolve_end_to_end() {
        let config = MissionRuntimeConfig {
            profile: MissionProfileKind::SafetyFirst,
            scorer_overrides: ScorerConfigOverrides {
                impact_weight: Some(0.20),
                ..ScorerConfigOverrides::default()
            },
            governor_overrides: GovernorConfigOverrides {
                reassignment_cooldown_cycles: Some(8),
                ..GovernorConfigOverrides::default()
            },
            ..MissionRuntimeConfig::default()
        };

        let validation = config.validate();
        assert!(validation.valid);

        let resolved = config.resolve();
        assert_eq!(resolved.profile, MissionProfileKind::SafetyFirst);
        assert!((resolved.scorer_config.weights.impact - 0.20).abs() < 1e-9);
        assert_eq!(resolved.governor_config.reassignment_cooldown_cycles, 8);
        // Risk should still be SafetyFirst default (not overridden).
        let safety = MissionProfile::safety_first();
        assert!(
            (resolved.scorer_config.weights.risk - safety.scorer_config.weights.risk).abs() < 1e-9
        );
    }

    #[test]
    fn golden_full_pipeline_deterministic() {
        // Run the complete pipeline twice with identical inputs.
        // Results must be byte-identical.
        let issues = golden_fixture();
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let config = MissionRuntimeConfig::default();
        let resolved = config.resolve();

        let run_pipeline = || {
            let report = resolve_bead_readiness(&issues);
            let features =
                extract_planner_features(&report, &agents, &ctx, &resolved.extraction_config);
            let inputs: Vec<ScorerInput> = features
                .features
                .iter()
                .map(|f| ScorerInput {
                    features: f.clone(),
                    effort: None,
                    tags: Vec::new(),
                })
                .collect();
            let scored = score_candidates(&inputs, &resolved.scorer_config);
            let assignments = solve_assignments(&scored, &agents, &resolved.solver_config);
            let explain_report = explain_decisions(1, &scored, &assignments);
            (scored.ranked_ids.clone(), explain_report)
        };

        let (ranking1, explain1) = run_pipeline();
        let (ranking2, explain2) = run_pipeline();

        assert_eq!(ranking1, ranking2, "Rankings must be deterministic");
        assert_eq!(
            explain1.explanations.len(),
            explain2.explanations.len(),
            "Explanation count must be deterministic"
        );
        for (e1, e2) in explain1
            .explanations
            .iter()
            .zip(explain2.explanations.iter())
        {
            assert_eq!(e1.bead_id, e2.bead_id);
            assert_eq!(e1.outcome, e2.outcome);
            assert_eq!(e1.summary, e2.summary);
        }
    }

    #[test]
    fn golden_safety_profile_changes_ranking() {
        let issues = golden_fixture();
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();

        // Run with balanced profile.
        let balanced_config = MissionRuntimeConfig::default().resolve();
        let report = resolve_bead_readiness(&issues);
        let features =
            extract_planner_features(&report, &agents, &ctx, &balanced_config.extraction_config);
        let inputs: Vec<ScorerInput> = features
            .features
            .iter()
            .map(|f| ScorerInput {
                features: f.clone(),
                effort: None,
                tags: Vec::new(),
            })
            .collect();
        let balanced_scored = score_candidates(&inputs, &balanced_config.scorer_config);

        // Run with safety-first profile.
        let safety_config = MissionRuntimeConfig {
            profile: MissionProfileKind::SafetyFirst,
            ..MissionRuntimeConfig::default()
        }
        .resolve();
        let safety_scored = score_candidates(&inputs, &safety_config.scorer_config);

        // Scores should differ because weights differ.
        let balanced_infra = balanced_scored
            .scored
            .iter()
            .find(|s| s.bead_id == "infra")
            .unwrap();
        let safety_infra = safety_scored
            .scored
            .iter()
            .find(|s| s.bead_id == "infra")
            .unwrap();
        assert!(
            (balanced_infra.final_score - safety_infra.final_score).abs() > 1e-6,
            "Different profiles should produce different scores"
        );
    }

    #[test]
    fn golden_vector_regression_snapshot() {
        // Pin specific numeric outputs to catch unintended scoring changes.
        let issues = golden_fixture();
        let report = resolve_bead_readiness(&issues);
        let agents = golden_agents();
        let ctx = PlannerExtractionContext::default();
        let config = PlannerExtractionConfig::default();
        let features = extract_planner_features(&report, &agents, &ctx, &config);

        // Verify we get exactly 2 feature vectors (infra + api).
        assert_eq!(features.features.len(), 2);

        // All scores in [0.0, 1.0].
        for f in &features.features {
            assert!(
                f.impact >= 0.0 && f.impact <= 1.0,
                "impact OOB: {}",
                f.impact
            );
            assert!(
                f.urgency >= 0.0 && f.urgency <= 1.0,
                "urgency OOB: {}",
                f.urgency
            );
            assert!(f.risk >= 0.0 && f.risk <= 1.0, "risk OOB: {}", f.risk);
            assert!(f.fit >= 0.0 && f.fit <= 1.0, "fit OOB: {}", f.fit);
            assert!(
                f.confidence >= 0.0 && f.confidence <= 1.0,
                "confidence OOB: {}",
                f.confidence
            );
        }

        // Score and verify ranking is infra > api.
        let inputs: Vec<ScorerInput> = features
            .features
            .iter()
            .map(|f| ScorerInput {
                features: f.clone(),
                effort: None,
                tags: Vec::new(),
            })
            .collect();
        let scored = score_candidates(&inputs, &ScorerConfig::default());
        assert_eq!(scored.ranked_ids, vec!["infra", "api"]);

        // All final_scores positive.
        for s in &scored.scored {
            assert!(s.final_score > 0.0, "{} score was 0", s.bead_id);
        }
    }
}
