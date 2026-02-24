//! Mission control-loop engine (ft-1i2ge.3.1).
//!
//! Implements the mission loop cadence and event-triggered reevaluation.
//! Orchestrates the full planner pipeline:
//!   readiness → features → scoring → solving → decisions
//!
//! The loop is synchronous and deterministic — it does not spawn threads
//! or use async. The caller drives the loop by calling `tick()` or `trigger()`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::beads_types::{BeadIssueDetail, BeadReadinessReport};
use crate::plan::MissionAgentCapabilityProfile;
use crate::planner_features::{
    extract_planner_features, score_candidates, solve_assignments, AssignmentSet,
    PlannerExtractionConfig, PlannerExtractionContext, PlannerExtractionReport,
    PlannerFeatureVector, RejectedCandidate, RejectionReason, ScorerConfig, ScorerInput,
    ScorerReport, SolverConfig,
};

// ── Loop state ──────────────────────────────────────────────────────────────

/// Trigger event that can cause immediate reevaluation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTrigger {
    /// A bead changed status (opened, closed, blocked, etc.).
    BeadStatusChange { bead_id: String },
    /// An agent became available or went offline.
    AgentAvailabilityChange { agent_id: String },
    /// Manual trigger from operator.
    ManualTrigger { reason: String },
    /// Timer-based cadence tick.
    CadenceTick,
    /// External signal (e.g. webhook, CI completion).
    ExternalSignal { source: String, payload: String },
}

/// Mission-level limiter envelope for assignment safety.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionSafetyEnvelopeConfig {
    /// Hard cap on assignments emitted in a single evaluation cycle.
    pub max_assignments_per_cycle: usize,
    /// Hard cap on risky assignments emitted in a single evaluation cycle.
    pub max_risky_assignments_per_cycle: usize,
    /// Maximum consecutive cycles a bead can be assigned before forcing one backoff cycle.
    pub max_consecutive_retries_per_bead: u32,
    /// Label markers that classify a bead as risky.
    #[serde(default = "default_risky_label_markers")]
    pub risky_label_markers: Vec<String>,
}

fn default_risky_label_markers() -> Vec<String> {
    vec![
        "danger".to_string(),
        "risky".to_string(),
        "high-risk".to_string(),
        "destructive".to_string(),
        "approval".to_string(),
    ]
}

impl Default for MissionSafetyEnvelopeConfig {
    fn default() -> Self {
        Self {
            max_assignments_per_cycle: 10,
            max_risky_assignments_per_cycle: 2,
            max_consecutive_retries_per_bead: 3,
            risky_label_markers: default_risky_label_markers(),
        }
    }
}

/// Configuration for the mission loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionLoopConfig {
    /// Default cadence interval in milliseconds between ticks.
    pub cadence_ms: u64,
    /// Maximum triggers to batch before forcing evaluation.
    pub max_trigger_batch: usize,
    /// Extraction config for feature pipeline.
    pub extraction_config: PlannerExtractionConfig,
    /// Scorer config for multi-factor scoring.
    pub scorer_config: ScorerConfig,
    /// Solver config for assignment resolution.
    pub solver_config: SolverConfig,
    /// Whether to include blocked candidates in extraction (for analysis).
    pub include_blocked_in_extraction: bool,
    /// Mission-level envelope caps for safety and anti-thrash behavior.
    #[serde(default)]
    pub safety_envelope: MissionSafetyEnvelopeConfig,
}

impl Default for MissionLoopConfig {
    fn default() -> Self {
        Self {
            cadence_ms: 30_000, // 30 seconds
            max_trigger_batch: 10,
            extraction_config: PlannerExtractionConfig::default(),
            scorer_config: ScorerConfig::default(),
            solver_config: SolverConfig::default(),
            include_blocked_in_extraction: false,
            safety_envelope: MissionSafetyEnvelopeConfig::default(),
        }
    }
}

/// A single decision produced by the loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDecision {
    pub cycle_id: u64,
    pub timestamp_ms: i64,
    pub trigger: MissionTrigger,
    pub assignment_set: AssignmentSet,
    pub extraction_summary: ExtractionSummary,
    pub scorer_summary: ScorerSummary,
}

/// Compact summary of the extraction phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionSummary {
    pub total_candidates: usize,
    pub ready_candidates: usize,
    pub top_impact_bead: Option<String>,
}

/// Compact summary of the scoring phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorerSummary {
    pub scored_count: usize,
    pub above_threshold_count: usize,
    pub top_scored_bead: Option<String>,
}

/// Snapshot of the loop's internal state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionLoopState {
    pub cycle_count: u64,
    pub last_evaluation_ms: Option<i64>,
    pub pending_triggers: Vec<MissionTrigger>,
    pub last_decision: Option<MissionDecision>,
    pub total_assignments_made: u64,
    pub total_rejections: u64,
    /// Consecutive assignment streaks by bead id (used for retry-storm limiting).
    #[serde(default)]
    pub retry_streaks: HashMap<String, u32>,
}

// ── Mission loop engine ─────────────────────────────────────────────────────

/// The mission control-loop engine.
///
/// Caller-driven: use `trigger()` to enqueue events, `tick()` to advance,
/// or `evaluate()` to force immediate processing.
pub struct MissionLoop {
    config: MissionLoopConfig,
    state: MissionLoopState,
}

impl MissionLoop {
    /// Create a new mission loop with the given configuration.
    #[must_use]
    pub fn new(config: MissionLoopConfig) -> Self {
        Self {
            config,
            state: MissionLoopState {
                cycle_count: 0,
                last_evaluation_ms: None,
                pending_triggers: Vec::new(),
                last_decision: None,
                total_assignments_made: 0,
                total_rejections: 0,
                retry_streaks: HashMap::new(),
            },
        }
    }

    /// Get a reference to the current configuration.
    #[must_use]
    pub fn config(&self) -> &MissionLoopConfig {
        &self.config
    }

    /// Get a snapshot of the loop state.
    #[must_use]
    pub fn state(&self) -> &MissionLoopState {
        &self.state
    }

    /// Enqueue a trigger event for the next evaluation.
    pub fn trigger(&mut self, trigger: MissionTrigger) {
        self.state.pending_triggers.push(trigger);
    }

    /// Number of pending triggers.
    #[must_use]
    pub fn pending_trigger_count(&self) -> usize {
        self.state.pending_triggers.len()
    }

    /// Check whether evaluation should happen now.
    ///
    /// Returns true if:
    /// - Pending triggers exceed the batch limit, or
    /// - Enough time has passed since last evaluation (cadence), or
    /// - No evaluation has happened yet.
    #[must_use]
    pub fn should_evaluate(&self, current_ms: i64) -> bool {
        if self.state.pending_triggers.len() >= self.config.max_trigger_batch {
            return true;
        }
        match self.state.last_evaluation_ms {
            None => true,
            Some(last) => (current_ms - last) >= self.config.cadence_ms as i64,
        }
    }

    /// Run a cadence tick: evaluate if the cadence interval has elapsed or triggers are batched.
    ///
    /// Returns `Some(decision)` if evaluation ran, `None` if skipped.
    pub fn tick(
        &mut self,
        current_ms: i64,
        issues: &[BeadIssueDetail],
        agents: &[MissionAgentCapabilityProfile],
        context: &PlannerExtractionContext,
    ) -> Option<MissionDecision> {
        if !self.should_evaluate(current_ms) {
            return None;
        }
        let trigger = if self.state.pending_triggers.is_empty() {
            MissionTrigger::CadenceTick
        } else {
            // Use the most recent trigger as the decision trigger.
            self.state.pending_triggers.last().cloned().unwrap()
        };
        Some(self.evaluate(current_ms, trigger, issues, agents, context))
    }

    /// Force immediate evaluation regardless of cadence.
    pub fn evaluate(
        &mut self,
        current_ms: i64,
        trigger: MissionTrigger,
        issues: &[BeadIssueDetail],
        agents: &[MissionAgentCapabilityProfile],
        context: &PlannerExtractionContext,
    ) -> MissionDecision {
        self.state.cycle_count += 1;
        let cycle_id = self.state.cycle_count;

        // Phase 1: Readiness resolution.
        let readiness: BeadReadinessReport = crate::beads_types::resolve_bead_readiness(issues);

        // Phase 2: Feature extraction.
        let extraction: PlannerExtractionReport = if self.config.include_blocked_in_extraction {
            crate::planner_features::extract_planner_features_all(
                &readiness,
                agents,
                context,
                &self.config.extraction_config,
            )
        } else {
            extract_planner_features(&readiness, agents, context, &self.config.extraction_config)
        };

        let extraction_summary = ExtractionSummary {
            total_candidates: readiness.candidates.len(),
            ready_candidates: readiness.ready_ids.len(),
            top_impact_bead: extraction.features.first().map(|f| f.bead_id.clone()),
        };

        // Phase 3: Multi-factor scoring.
        let scorer_inputs: Vec<ScorerInput> = extraction
            .features
            .iter()
            .map(|f| {
                let tags: Vec<String> = issues
                    .iter()
                    .find(|i| i.id == f.bead_id)
                    .map(|i| i.labels.clone())
                    .unwrap_or_default();
                ScorerInput {
                    features: f.clone(),
                    effort: None, // effort estimation not available in this phase
                    tags,
                }
            })
            .collect();

        let scorer_report: ScorerReport =
            score_candidates(&scorer_inputs, &self.config.scorer_config);

        let scorer_summary = ScorerSummary {
            scored_count: scorer_report.scored.len(),
            above_threshold_count: scorer_report
                .scored
                .iter()
                .filter(|s| !s.below_confidence_threshold && s.final_score > 0.0)
                .count(),
            top_scored_bead: scorer_report.scored.first().map(|s| s.bead_id.clone()),
        };

        // Phase 4: Assignment solving.
        let assignment_set: AssignmentSet =
            solve_assignments(&scorer_report, agents, &self.config.solver_config);
        let assignment_set = self.apply_safety_envelope(assignment_set, issues);

        // Update state.
        self.state.total_assignments_made += assignment_set.assignments.len() as u64;
        self.state.total_rejections += assignment_set.rejected.len() as u64;
        self.state.last_evaluation_ms = Some(current_ms);
        self.state.pending_triggers.clear();

        let decision = MissionDecision {
            cycle_id,
            timestamp_ms: current_ms,
            trigger,
            assignment_set,
            extraction_summary,
            scorer_summary,
        };

        self.state.last_decision = Some(decision.clone());
        decision
    }

    fn apply_safety_envelope(
        &mut self,
        assignment_set: AssignmentSet,
        issues: &[BeadIssueDetail],
    ) -> AssignmentSet {
        const GATE_MAX_ASSIGNMENTS: &str = "mission.envelope.max_assignments_per_cycle";
        const GATE_MAX_RISKY_ASSIGNMENTS: &str = "mission.envelope.max_risky_assignments_per_cycle";
        const GATE_RETRY_STORM: &str = "mission.envelope.retry_storm";

        let mut kept_assignments = Vec::with_capacity(assignment_set.assignments.len());
        let mut envelope_rejections: Vec<RejectedCandidate> = Vec::new();
        let mut risky_assigned_count = 0usize;
        let mut next_retry_streaks: HashMap<String, u32> = HashMap::new();

        for mut assignment in assignment_set.assignments {
            let previous_retry_streak = self
                .state
                .retry_streaks
                .get(&assignment.bead_id)
                .copied()
                .unwrap_or(0);
            let retry_limit = self.config.safety_envelope.max_consecutive_retries_per_bead;

            if retry_limit > 0 && previous_retry_streak >= retry_limit {
                envelope_rejections.push(RejectedCandidate {
                    bead_id: assignment.bead_id.clone(),
                    score: assignment.score,
                    reasons: vec![RejectionReason::SafetyGateDenied {
                        gate_name: GATE_RETRY_STORM.to_string(),
                    }],
                });
                // Reset streak after one forced backoff cycle.
                next_retry_streaks.insert(assignment.bead_id, 0);
                continue;
            }

            let is_risky = self.is_risky_assignment(&assignment.bead_id, issues);
            if kept_assignments.len() >= self.config.safety_envelope.max_assignments_per_cycle {
                envelope_rejections.push(RejectedCandidate {
                    bead_id: assignment.bead_id,
                    score: assignment.score,
                    reasons: vec![RejectionReason::SafetyGateDenied {
                        gate_name: GATE_MAX_ASSIGNMENTS.to_string(),
                    }],
                });
                continue;
            }

            if is_risky
                && risky_assigned_count
                    >= self.config.safety_envelope.max_risky_assignments_per_cycle
            {
                envelope_rejections.push(RejectedCandidate {
                    bead_id: assignment.bead_id,
                    score: assignment.score,
                    reasons: vec![RejectionReason::SafetyGateDenied {
                        gate_name: GATE_MAX_RISKY_ASSIGNMENTS.to_string(),
                    }],
                });
                continue;
            }

            if is_risky {
                risky_assigned_count += 1;
            }

            assignment.rank = kept_assignments.len() + 1;
            next_retry_streaks.insert(
                assignment.bead_id.clone(),
                previous_retry_streak.saturating_add(1),
            );
            kept_assignments.push(assignment);
        }

        let mut rejected = assignment_set.rejected;
        rejected.extend(envelope_rejections);
        self.state.retry_streaks = next_retry_streaks;

        AssignmentSet {
            assignments: kept_assignments,
            rejected,
            solver_config: assignment_set.solver_config,
        }
    }

    fn is_risky_assignment(&self, bead_id: &str, issues: &[BeadIssueDetail]) -> bool {
        let Some(issue) = issues.iter().find(|issue| issue.id == bead_id) else {
            return false;
        };
        issue.labels.iter().any(|label| {
            let normalized_label = label.to_ascii_lowercase();
            self.config
                .safety_envelope
                .risky_label_markers
                .iter()
                .any(|marker| normalized_label.contains(&marker.to_ascii_lowercase()))
        })
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beads_types::{BeadDependencyRef, BeadIssueType, BeadStatus};

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

    fn sample_detail_with_labels(
        id: &str,
        status: BeadStatus,
        priority: u8,
        dependency_ids: &[(&str, &str)],
        labels: &[&str],
    ) -> BeadIssueDetail {
        let mut detail = sample_detail(id, status, priority, dependency_ids);
        detail.labels = labels.iter().map(|label| (*label).to_string()).collect();
        detail
    }

    fn ready_agent(agent_id: &str) -> MissionAgentCapabilityProfile {
        MissionAgentCapabilityProfile {
            agent_id: agent_id.to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: crate::plan::MissionAgentAvailability::Ready,
        }
    }

    #[test]
    fn loop_new_initial_state() {
        let ml = MissionLoop::new(MissionLoopConfig::default());
        assert_eq!(ml.state().cycle_count, 0);
        assert!(ml.state().last_evaluation_ms.is_none());
        assert!(ml.state().pending_triggers.is_empty());
        assert!(ml.state().last_decision.is_none());
        assert_eq!(ml.state().total_assignments_made, 0);
    }

    #[test]
    fn loop_trigger_enqueues() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        assert_eq!(ml.pending_trigger_count(), 0);
        ml.trigger(MissionTrigger::ManualTrigger {
            reason: "test".to_string(),
        });
        assert_eq!(ml.pending_trigger_count(), 1);
    }

    #[test]
    fn loop_should_evaluate_first_time() {
        let ml = MissionLoop::new(MissionLoopConfig::default());
        assert!(ml.should_evaluate(0));
    }

    #[test]
    fn loop_should_evaluate_after_cadence() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // Not enough time elapsed
        assert!(!ml.should_evaluate(2000));
        // Cadence elapsed (30s = 30000ms)
        assert!(ml.should_evaluate(32000));
    }

    #[test]
    fn loop_should_evaluate_trigger_batch() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            max_trigger_batch: 2,
            ..MissionLoopConfig::default()
        });
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // Not time yet, no triggers
        assert!(!ml.should_evaluate(2000));
        ml.trigger(MissionTrigger::BeadStatusChange {
            bead_id: "x".to_string(),
        });
        // One trigger, below batch limit
        assert!(!ml.should_evaluate(2000));
        ml.trigger(MissionTrigger::BeadStatusChange {
            bead_id: "y".to_string(),
        });
        // Two triggers = batch limit hit
        assert!(ml.should_evaluate(2000));
    }

    #[test]
    fn loop_evaluate_produces_decision() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("dep", BeadStatus::Closed, 0, &[]),
            sample_detail("ready", BeadStatus::Open, 0, &[("dep", "blocks")]),
        ];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(
            5000,
            MissionTrigger::ManualTrigger {
                reason: "test".to_string(),
            },
            &issues,
            &agents,
            &ctx,
        );

        assert_eq!(decision.cycle_id, 1);
        assert_eq!(decision.timestamp_ms, 5000);
        assert!(decision.assignment_set.assignment_count() > 0);
        assert_eq!(ml.state().cycle_count, 1);
        assert_eq!(ml.state().last_evaluation_ms, Some(5000));
    }

    #[test]
    fn loop_evaluate_increments_cycle() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(ml.state().cycle_count, 1);

        ml.evaluate(32000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(ml.state().cycle_count, 2);
    }

    #[test]
    fn loop_evaluate_clears_triggers() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.trigger(MissionTrigger::BeadStatusChange {
            bead_id: "x".to_string(),
        });
        ml.trigger(MissionTrigger::AgentAvailabilityChange {
            agent_id: "a1".to_string(),
        });
        assert_eq!(ml.pending_trigger_count(), 2);

        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(ml.pending_trigger_count(), 0);
    }

    #[test]
    fn loop_tick_returns_none_when_not_due() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // Too soon
        let result = ml.tick(2000, &issues, &agents, &ctx);
        assert!(result.is_none());
    }

    #[test]
    fn loop_tick_returns_some_when_due() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();

        // First tick always evaluates
        let result = ml.tick(1000, &issues, &agents, &ctx);
        assert!(result.is_some());

        // Second tick after cadence
        let result = ml.tick(32000, &issues, &agents, &ctx);
        assert!(result.is_some());
    }

    #[test]
    fn loop_empty_issues() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let decision = ml.evaluate(
            1000,
            MissionTrigger::CadenceTick,
            &[],
            &[ready_agent("a1")],
            &PlannerExtractionContext::default(),
        );
        assert_eq!(decision.assignment_set.assignment_count(), 0);
        assert_eq!(decision.extraction_summary.total_candidates, 0);
        assert_eq!(decision.extraction_summary.ready_candidates, 0);
    }

    #[test]
    fn loop_empty_agents() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let decision = ml.evaluate(
            1000,
            MissionTrigger::CadenceTick,
            &issues,
            &[],
            &PlannerExtractionContext::default(),
        );
        // No agents => no assignments
        assert_eq!(decision.assignment_set.assignment_count(), 0);
    }

    #[test]
    fn loop_tracks_total_assignments() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert!(ml.state().total_assignments_made > 0);
    }

    #[test]
    fn loop_envelope_limits_assignments_per_cycle() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            safety_envelope: MissionSafetyEnvelopeConfig {
                max_assignments_per_cycle: 1,
                max_risky_assignments_per_cycle: 10,
                max_consecutive_retries_per_bead: 100,
                ..MissionSafetyEnvelopeConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let agents = vec![ready_agent("a1"), ready_agent("a2")];
        let ctx = PlannerExtractionContext::default();

        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(decision.assignment_set.assignment_count(), 1);
        assert!(decision.assignment_set.rejected.iter().any(|rejected| {
            rejected.reasons.iter().any(|reason| {
                matches!(
                    reason,
                    RejectionReason::SafetyGateDenied { gate_name }
                    if gate_name == "mission.envelope.max_assignments_per_cycle"
                )
            })
        }));
    }

    #[test]
    fn loop_envelope_limits_risky_assignments_by_label() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            safety_envelope: MissionSafetyEnvelopeConfig {
                max_assignments_per_cycle: 10,
                max_risky_assignments_per_cycle: 1,
                max_consecutive_retries_per_bead: 100,
                risky_label_markers: vec!["danger".to_string()],
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![
            sample_detail_with_labels("r1", BeadStatus::Open, 0, &[], &["dangerous"]),
            sample_detail_with_labels("r2", BeadStatus::Open, 1, &[], &["danger-zone"]),
            sample_detail_with_labels("r3", BeadStatus::Open, 2, &[], &["danger"]),
        ];
        let agents = vec![ready_agent("a1"), ready_agent("a2"), ready_agent("a3")];
        let ctx = PlannerExtractionContext::default();

        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(decision.assignment_set.assignment_count(), 1);
        assert!(decision.assignment_set.rejected.iter().any(|rejected| {
            rejected.reasons.iter().any(|reason| {
                matches!(
                    reason,
                    RejectionReason::SafetyGateDenied { gate_name }
                    if gate_name == "mission.envelope.max_risky_assignments_per_cycle"
                )
            })
        }));
    }

    #[test]
    fn loop_envelope_blocks_retry_storm_for_one_cycle() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            safety_envelope: MissionSafetyEnvelopeConfig {
                max_assignments_per_cycle: 10,
                max_risky_assignments_per_cycle: 10,
                max_consecutive_retries_per_bead: 1,
                ..MissionSafetyEnvelopeConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![sample_detail("retry", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();

        let first = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(first.assignment_set.assignment_count(), 1);

        let second = ml.evaluate(2000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(second.assignment_set.assignment_count(), 0);
        assert!(second.assignment_set.rejected.iter().any(|rejected| {
            rejected.reasons.iter().any(|reason| {
                matches!(
                    reason,
                    RejectionReason::SafetyGateDenied { gate_name }
                    if gate_name == "mission.envelope.retry_storm"
                )
            })
        }));

        let third = ml.evaluate(3000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        assert_eq!(third.assignment_set.assignment_count(), 1);
    }

    #[test]
    fn loop_last_decision_stored() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        assert!(ml.state().last_decision.is_none());

        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        assert!(ml.state().last_decision.is_some());
        assert_eq!(ml.state().last_decision.as_ref().unwrap().cycle_id, 1);
    }

    #[test]
    fn loop_blocked_not_assigned() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("blocker", BeadStatus::Open, 0, &[]),
            sample_detail("blocked", BeadStatus::Open, 1, &[("blocker", "blocks")]),
        ];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // Only blocker should be assigned, not blocked
        assert_eq!(decision.assignment_set.assignment_count(), 1);
        assert_eq!(decision.assignment_set.assignments[0].bead_id, "blocker");
    }

    #[test]
    fn loop_uses_labels_as_tags() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let mut issue = sample_detail("safe-bead", BeadStatus::Open, 0, &[]);
        issue.labels = vec!["safety".to_string(), "mission".to_string()];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &[issue], &agents, &ctx);

        // Safety label should boost the score
        assert_eq!(decision.assignment_set.assignment_count(), 1);
    }

    #[test]
    fn loop_extraction_summary_accurate() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("ready1", BeadStatus::Open, 0, &[]),
            sample_detail("ready2", BeadStatus::Open, 1, &[]),
            sample_detail("blocked", BeadStatus::Open, 2, &[("ready1", "blocks")]),
        ];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        assert_eq!(decision.extraction_summary.total_candidates, 3);
        assert_eq!(decision.extraction_summary.ready_candidates, 2);
        assert!(decision.extraction_summary.top_impact_bead.is_some());
    }

    #[test]
    fn loop_scorer_summary_accurate() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        assert_eq!(decision.scorer_summary.scored_count, 2);
        assert!(decision.scorer_summary.top_scored_bead.is_some());
    }

    #[test]
    fn loop_config_serde_roundtrip() {
        let config = MissionLoopConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: MissionLoopConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cadence_ms, config.cadence_ms);
        assert_eq!(
            back.safety_envelope.max_assignments_per_cycle,
            config.safety_envelope.max_assignments_per_cycle
        );
    }

    #[test]
    fn loop_decision_serde_roundtrip() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("a1")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let json = serde_json::to_string(&decision).unwrap();
        let back: MissionDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_id, 1);
        assert_eq!(back.timestamp_ms, 1000);
    }

    #[test]
    fn loop_trigger_serde_roundtrip() {
        let triggers = vec![
            MissionTrigger::BeadStatusChange {
                bead_id: "x".to_string(),
            },
            MissionTrigger::AgentAvailabilityChange {
                agent_id: "a".to_string(),
            },
            MissionTrigger::ManualTrigger {
                reason: "test".to_string(),
            },
            MissionTrigger::CadenceTick,
            MissionTrigger::ExternalSignal {
                source: "ci".to_string(),
                payload: "{}".to_string(),
            },
        ];
        for trigger in &triggers {
            let json = serde_json::to_string(trigger).unwrap();
            let back: MissionTrigger = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, trigger);
        }
    }

    #[test]
    fn loop_state_serde_roundtrip() {
        let state = MissionLoopState {
            cycle_count: 5,
            last_evaluation_ms: Some(1000),
            pending_triggers: vec![MissionTrigger::CadenceTick],
            last_decision: None,
            total_assignments_made: 10,
            total_rejections: 3,
            retry_streaks: HashMap::from([("bead-a".to_string(), 2)]),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionLoopState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_count, 5);
        assert_eq!(back.total_assignments_made, 10);
        assert_eq!(back.retry_streaks.get("bead-a"), Some(&2));
    }
}
