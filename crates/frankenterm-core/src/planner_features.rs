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
        let raw = w.impact * self.impact
            + w.urgency * self.urgency
            + w.risk * (1.0 - self.risk) // invert: low risk is good
            + w.fit * self.fit
            + w.confidence * self.confidence;
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

    let impact = config.impact_unblock_weight * unblock_norm
        + config.impact_depth_weight * depth_norm;
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

    let urgency = config.urgency_priority_weight * priority_norm
        + config.urgency_staleness_weight * staleness_norm;
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
fn extract_fit(
    _candidate: &BeadReadyCandidate,
    agents: &[MissionAgentCapabilityProfile],
) -> f64 {
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
fn extract_confidence(
    candidate: &BeadReadyCandidate,
    context: &PlannerExtractionContext,
) -> f64 {
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beads_types::{
        resolve_bead_readiness, BeadDependencyRef, BeadIssueDetail, BeadIssueType,
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
        ctx.staleness_hours
            .insert("very-stale".to_string(), 1000.0); // way over max
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
        assert!(score >= 0.0 && score <= 1.0);
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
}
