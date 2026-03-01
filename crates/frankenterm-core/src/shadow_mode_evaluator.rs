//! Shadow-mode evaluator: compare mission recommendations vs actual execution.
//!
//! Runs in non-invasive mode alongside the mission loop, capturing each cycle's
//! recommendations and comparing them against the events that were actually
//! emitted during dispatch.
//!
//! # Integration
//!
//! ```text
//! MissionLoop.evaluate()
//!     ├─ AssignmentSet (recommendations)
//!     └─ MissionEventLog (execution events)
//!              ↓
//!     ShadowModeEvaluator.evaluate_cycle()
//!              ↓
//!     ShadowModeDiff (divergences, metrics)
//! ```
//!
//! # Usage
//!
//! The evaluator does not modify mission state; it observes and reports.
//! The `ShadowModeDiff` output feeds into:
//! - F4: Canary rollout controller (go/no-go signals)
//! - F5: SLO dashboard (accuracy metrics)
//! - Operator explain views (why did execution diverge from plan?)

use crate::mission_events::{MissionEvent, MissionEventKind};
use crate::planner_features::{Assignment, AssignmentSet};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ── Configuration ───────────────────────────────────────────────────────────

/// Shadow-mode evaluator configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowEvaluationConfig {
    /// Maximum number of cycle diffs to retain in history.
    pub max_history: usize,
    /// Score threshold below which a recommendation is considered "low-confidence".
    pub low_confidence_threshold: f64,
    /// Enable agent divergence tracking (recommended agent != actual agent).
    pub track_agent_divergence: bool,
    /// Enable score accuracy tracking (high-score recommendations that failed).
    pub track_score_accuracy: bool,
    /// Minimum number of cycles before drift metrics are meaningful.
    pub warmup_cycles: usize,
}

impl Default for ShadowEvaluationConfig {
    fn default() -> Self {
        Self {
            max_history: 256,
            low_confidence_threshold: 0.3,
            track_agent_divergence: true,
            track_score_accuracy: true,
            warmup_cycles: 5,
        }
    }
}

// ── Diff types ──────────────────────────────────────────────────────────────

/// A divergence where the recommended agent differs from the executed agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDivergence {
    /// Bead that was assigned differently than recommended.
    pub bead_id: String,
    /// Agent the planner recommended.
    pub recommended_agent: String,
    /// Agent that actually received the dispatch.
    pub actual_agent: String,
    /// Reason code from the execution event (if available).
    pub reason_code: String,
}

/// A score accuracy record: did a high-confidence recommendation succeed?
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreAccuracyRecord {
    /// Bead that was assigned.
    pub bead_id: String,
    /// Score the planner gave this assignment.
    pub recommended_score: f64,
    /// Whether the execution was emitted (dispatched) or rejected.
    pub was_dispatched: bool,
    /// Whether it was rejected by a safety gate.
    pub safety_rejected: bool,
}

/// An execution event not present in recommendations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnexpectedExecution {
    /// Bead that was dispatched but not recommended.
    pub bead_id: String,
    /// Agent that received the dispatch.
    pub agent_id: String,
    /// Reason code from the execution event.
    pub reason_code: String,
}

/// Diff between recommendations and actual execution for a single cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowModeDiff {
    /// Evaluation cycle this diff belongs to.
    pub cycle_id: u64,
    /// Timestamp when the diff was computed.
    pub timestamp_ms: i64,

    // ── Counts ──
    /// Number of assignments recommended by the planner.
    pub recommendations_count: usize,
    /// Number of rejections by the planner.
    pub rejections_count: usize,
    /// Number of assignments actually emitted (dispatched).
    pub emissions_count: usize,
    /// Number of assignments actually rejected during execution.
    pub execution_rejections_count: usize,

    // ── Divergences ──
    /// Bead+agent pairs recommended but not present in dispatch events.
    pub missing_executions: Vec<(String, String)>,
    /// Dispatch events that were not in the recommendation set.
    pub unexpected_executions: Vec<UnexpectedExecution>,
    /// Cases where the dispatched agent differs from the recommended agent.
    pub agent_divergences: Vec<AgentDivergence>,
    /// Score accuracy records for dispatched/rejected assignments.
    pub score_accuracy: Vec<ScoreAccuracyRecord>,

    // ── Safety gate analysis ──
    /// Number of safety gate rejections during this cycle.
    pub safety_gate_rejections: usize,
    /// Number of retry storm throttle events during this cycle.
    pub retry_storm_throttles: usize,

    // ── Conflict analysis ──
    /// Number of conflicts detected during reconciliation.
    pub conflicts_detected: usize,
    /// Number of conflicts auto-resolved.
    pub conflicts_auto_resolved: usize,

    // ── Derived metrics ──
    /// Fraction of recommendations that were dispatched (0.0..1.0).
    pub dispatch_rate: f64,
    /// Fraction of dispatched assignments whose agent matched recommendation.
    pub agent_match_rate: f64,
    /// Overall cycle fidelity: how closely execution matched recommendations.
    pub fidelity_score: f64,
}

impl ShadowModeDiff {
    /// Whether this diff indicates a healthy cycle (no major divergences).
    pub fn is_healthy(&self) -> bool {
        self.missing_executions.is_empty()
            && self.unexpected_executions.is_empty()
            && self.fidelity_score >= 0.8
    }

    /// Number of total divergences (missing + unexpected + agent).
    pub fn total_divergences(&self) -> usize {
        self.missing_executions.len()
            + self.unexpected_executions.len()
            + self.agent_divergences.len()
    }
}

// ── Aggregate metrics ───────────────────────────────────────────────────────

/// Running aggregate metrics across multiple cycles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowModeMetrics {
    /// Total evaluation cycles observed.
    pub total_cycles: u64,
    /// Total recommendations made across all cycles.
    pub total_recommendations: u64,
    /// Total dispatch events across all cycles.
    pub total_dispatches: u64,
    /// Running mean dispatch rate.
    pub mean_dispatch_rate: f64,
    /// Running mean agent match rate.
    pub mean_agent_match_rate: f64,
    /// Running mean fidelity score.
    pub mean_fidelity_score: f64,
    /// Number of cycles where fidelity dropped below 0.5.
    pub low_fidelity_count: u64,
    /// Maximum consecutive low-fidelity cycles observed.
    pub max_consecutive_low_fidelity: u64,
    /// Current consecutive low-fidelity streak.
    current_streak: u64,
}

impl Default for ShadowModeMetrics {
    fn default() -> Self {
        Self {
            total_cycles: 0,
            total_recommendations: 0,
            total_dispatches: 0,
            mean_dispatch_rate: 0.0,
            mean_agent_match_rate: 0.0,
            mean_fidelity_score: 0.0,
            low_fidelity_count: 0,
            max_consecutive_low_fidelity: 0,
            current_streak: 0,
        }
    }
}

impl ShadowModeMetrics {
    fn update(&mut self, diff: &ShadowModeDiff) {
        self.total_cycles += 1;
        self.total_recommendations += diff.recommendations_count as u64;
        self.total_dispatches += diff.emissions_count as u64;

        // Incremental mean update (Welford-style simple mean)
        let n = self.total_cycles as f64;
        self.mean_dispatch_rate += (diff.dispatch_rate - self.mean_dispatch_rate) / n;
        self.mean_agent_match_rate += (diff.agent_match_rate - self.mean_agent_match_rate) / n;
        self.mean_fidelity_score += (diff.fidelity_score - self.mean_fidelity_score) / n;

        if diff.fidelity_score < 0.5 {
            self.low_fidelity_count += 1;
            self.current_streak += 1;
            if self.current_streak > self.max_consecutive_low_fidelity {
                self.max_consecutive_low_fidelity = self.current_streak;
            }
        } else {
            self.current_streak = 0;
        }
    }
}

// ── Evaluator ───────────────────────────────────────────────────────────────

/// Shadow-mode evaluator that compares recommendations against execution.
///
/// Non-invasive: reads from AssignmentSet and MissionEventLog without
/// modifying any mission state.
#[derive(Debug)]
pub struct ShadowModeEvaluator {
    config: ShadowEvaluationConfig,
    history: Vec<ShadowModeDiff>,
    metrics: ShadowModeMetrics,
}

impl ShadowModeEvaluator {
    /// Create a new evaluator with the given configuration.
    pub fn new(config: ShadowEvaluationConfig) -> Self {
        Self {
            config,
            history: Vec::new(),
            metrics: ShadowModeMetrics::default(),
        }
    }

    /// Create a new evaluator with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ShadowEvaluationConfig::default())
    }

    /// Evaluate a single cycle by comparing recommendations against events.
    ///
    /// `recommendations` is the planner's AssignmentSet from MissionDecision.
    /// `events` is the slice of MissionEvents for this cycle (filtered by cycle_id).
    /// `cycle_id` and `timestamp_ms` identify the cycle.
    pub fn evaluate_cycle(
        &mut self,
        cycle_id: u64,
        timestamp_ms: i64,
        recommendations: &AssignmentSet,
        events: &[MissionEvent],
    ) -> ShadowModeDiff {
        let diff = compute_diff(
            cycle_id,
            timestamp_ms,
            recommendations,
            events,
            &self.config,
        );

        self.metrics.update(&diff);

        // Store in history with FIFO eviction
        if self.history.len() >= self.config.max_history {
            self.history.remove(0);
        }
        self.history.push(diff.clone());

        diff
    }

    /// Get the running aggregate metrics.
    pub fn metrics(&self) -> &ShadowModeMetrics {
        &self.metrics
    }

    /// Get the most recent diff.
    pub fn last_diff(&self) -> Option<&ShadowModeDiff> {
        self.history.last()
    }

    /// Get the diff history.
    pub fn history(&self) -> &[ShadowModeDiff] {
        &self.history
    }

    /// Whether the evaluator has completed warmup (enough cycles for meaningful metrics).
    pub fn is_warmed_up(&self) -> bool {
        self.metrics.total_cycles >= self.config.warmup_cycles as u64
    }

    /// Clear all history and reset metrics.
    pub fn reset(&mut self) {
        self.history.clear();
        self.metrics = ShadowModeMetrics::default();
    }

    /// Get the current configuration.
    pub fn config(&self) -> &ShadowEvaluationConfig {
        &self.config
    }

    /// Total cycles evaluated.
    pub fn total_cycles(&self) -> u64 {
        self.metrics.total_cycles
    }
}

// ── Core diff computation ───────────────────────────────────────────────────

/// Extract dispatch events (bead_id, agent_id) from mission events.
fn extract_dispatches(events: &[MissionEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|e| e.kind == MissionEventKind::AssignmentEmitted)
        .filter_map(|e| {
            let bead_id = e
                .details
                .get("bead_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())?;
            let agent_id = e
                .details
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())?;
            Some((bead_id, agent_id))
        })
        .collect()
}

/// Extract rejection events (bead_id) from mission events.
fn extract_execution_rejections(events: &[MissionEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind == MissionEventKind::AssignmentRejected)
        .filter_map(|e| {
            e.details
                .get("bead_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Count safety-related events.
fn count_safety_events(events: &[MissionEvent]) -> (usize, usize) {
    let mut gate_rejections = 0;
    let mut retry_throttles = 0;
    for e in events {
        match e.kind {
            MissionEventKind::SafetyGateRejection => gate_rejections += 1,
            MissionEventKind::RetryStormThrottled => retry_throttles += 1,
            _ => {}
        }
    }
    (gate_rejections, retry_throttles)
}

/// Count conflict-related events.
fn count_conflict_events(events: &[MissionEvent]) -> (usize, usize) {
    let mut detected = 0;
    let mut auto_resolved = 0;
    for e in events {
        match e.kind {
            MissionEventKind::ConflictDetected => detected += 1,
            MissionEventKind::ConflictAutoResolved => auto_resolved += 1,
            _ => {}
        }
    }
    (detected, auto_resolved)
}

/// Compute the diff between recommendations and actual execution.
fn compute_diff(
    cycle_id: u64,
    timestamp_ms: i64,
    recommendations: &AssignmentSet,
    events: &[MissionEvent],
    config: &ShadowEvaluationConfig,
) -> ShadowModeDiff {
    let dispatches = extract_dispatches(events);
    let execution_rejections = extract_execution_rejections(events);
    let (safety_gate_rejections, retry_storm_throttles) = count_safety_events(events);
    let (conflicts_detected, conflicts_auto_resolved) = count_conflict_events(events);

    // Build lookup sets
    let recommended_set: HashMap<&str, &Assignment> = recommendations
        .assignments
        .iter()
        .map(|a| (a.bead_id.as_str(), a))
        .collect();

    let dispatched_set: HashMap<&str, &str> = dispatches
        .iter()
        .map(|(b, a)| (b.as_str(), a.as_str()))
        .collect();

    let rejected_bead_set: HashSet<&str> =
        execution_rejections.iter().map(|s| s.as_str()).collect();

    // Find missing executions: recommended but not dispatched
    let mut missing_executions = Vec::new();
    for assignment in &recommendations.assignments {
        if !dispatched_set.contains_key(assignment.bead_id.as_str()) {
            missing_executions.push((assignment.bead_id.clone(), assignment.agent_id.clone()));
        }
    }

    // Find unexpected executions: dispatched but not recommended
    let mut unexpected_executions = Vec::new();
    for (bead_id, agent_id) in &dispatches {
        if !recommended_set.contains_key(bead_id.as_str()) {
            unexpected_executions.push(UnexpectedExecution {
                bead_id: bead_id.clone(),
                agent_id: agent_id.clone(),
                reason_code: events
                    .iter()
                    .find(|e| {
                        e.kind == MissionEventKind::AssignmentEmitted
                            && e.details
                                .get("bead_id")
                                .and_then(|v| v.as_str())
                                .is_some_and(|b| b == bead_id)
                    })
                    .map(|e| e.reason_code.clone())
                    .unwrap_or_default(),
            });
        }
    }

    // Agent divergences: recommended agent != dispatched agent
    let mut agent_divergences = Vec::new();
    if config.track_agent_divergence {
        for assignment in &recommendations.assignments {
            if let Some(&actual_agent) = dispatched_set.get(assignment.bead_id.as_str()) {
                if actual_agent != assignment.agent_id {
                    agent_divergences.push(AgentDivergence {
                        bead_id: assignment.bead_id.clone(),
                        recommended_agent: assignment.agent_id.clone(),
                        actual_agent: actual_agent.to_string(),
                        reason_code: String::new(),
                    });
                }
            }
        }
    }

    // Score accuracy: track whether dispatched/rejected matches expectations
    let mut score_accuracy = Vec::new();
    if config.track_score_accuracy {
        for assignment in &recommendations.assignments {
            let was_dispatched = dispatched_set.contains_key(assignment.bead_id.as_str());
            let safety_rejected = rejected_bead_set.contains(assignment.bead_id.as_str());
            score_accuracy.push(ScoreAccuracyRecord {
                bead_id: assignment.bead_id.clone(),
                recommended_score: assignment.score,
                was_dispatched,
                safety_rejected,
            });
        }
    }

    // Compute derived metrics
    let recommendations_count = recommendations.assignments.len();
    let emissions_count = dispatches.len();

    let dispatch_rate = if recommendations_count > 0 {
        emissions_count as f64 / recommendations_count as f64
    } else if emissions_count == 0 {
        1.0 // Both zero → perfect (no work to do)
    } else {
        0.0 // Unexpected executions with no recommendations
    };

    let matched_agents = recommendations
        .assignments
        .iter()
        .filter(|a| {
            dispatched_set
                .get(a.bead_id.as_str())
                .is_some_and(|&actual| actual == a.agent_id)
        })
        .count();

    let denominator = emissions_count.min(recommendations_count);
    let agent_match_rate = if denominator > 0 {
        matched_agents as f64 / denominator as f64
    } else if recommendations_count == 0 && emissions_count == 0 {
        1.0 // Both zero → perfect
    } else {
        0.0 // Unexpected executions with no recommendations
    };

    // Fidelity: weighted combination of dispatch rate and agent match rate
    // with penalty for unexpected executions
    let unexpected_penalty = if emissions_count > 0 {
        1.0 - (unexpected_executions.len() as f64 / emissions_count as f64).min(1.0)
    } else {
        1.0
    };
    let fidelity_score =
        (dispatch_rate * 0.5 + agent_match_rate * 0.3 + unexpected_penalty * 0.2).clamp(0.0, 1.0);

    ShadowModeDiff {
        cycle_id,
        timestamp_ms,
        recommendations_count,
        rejections_count: recommendations.rejected.len(),
        emissions_count,
        execution_rejections_count: execution_rejections.len(),
        missing_executions,
        unexpected_executions,
        agent_divergences,
        score_accuracy,
        safety_gate_rejections,
        retry_storm_throttles,
        conflicts_detected,
        conflicts_auto_resolved,
        dispatch_rate,
        agent_match_rate,
        fidelity_score,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_features::{RejectedCandidate, SolverConfig};

    fn make_assignment(bead_id: &str, agent_id: &str, score: f64, rank: usize) -> Assignment {
        Assignment {
            bead_id: bead_id.to_string(),
            agent_id: agent_id.to_string(),
            score,
            rank,
        }
    }

    fn make_assignment_set(assignments: Vec<Assignment>) -> AssignmentSet {
        AssignmentSet {
            assignments,
            rejected: Vec::new(),
            solver_config: SolverConfig::default(),
        }
    }

    use crate::mission_events::{MissionEventBuilder, MissionEventLog, MissionEventLogConfig};

    fn make_log() -> MissionEventLog {
        MissionEventLog::new(MissionEventLogConfig {
            max_events: 1024,
            enabled: true,
        })
    }

    fn emit_dispatch(log: &mut MissionEventLog, cycle_id: u64, bead_id: &str, agent_id: &str) {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::AssignmentEmitted,
                "mission.dispatch.assignment_emitted",
            )
            .cycle(cycle_id, 1000)
            .correlation("corr-1")
            .labels("workspace", "track")
            .detail_str("bead_id", bead_id)
            .detail_str("agent_id", agent_id),
        );
    }

    fn emit_rejection(log: &mut MissionEventLog, cycle_id: u64, bead_id: &str) {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::AssignmentRejected,
                "mission.dispatch.assignment_rejected",
            )
            .cycle(cycle_id, 1000)
            .correlation("corr-1")
            .labels("workspace", "track")
            .detail_str("bead_id", bead_id),
        );
    }

    fn emit_safety(log: &mut MissionEventLog, cycle_id: u64) {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::SafetyGateRejection,
                "mission.safety.gate_rejection",
            )
            .cycle(cycle_id, 1000)
            .correlation("corr-1")
            .labels("workspace", "track"),
        );
    }

    fn emit_conflict(log: &mut MissionEventLog, cycle_id: u64, kind: MissionEventKind) {
        log.emit(
            MissionEventBuilder::new(kind, "mission.reconcile.conflict_detected")
                .cycle(cycle_id, 1000)
                .correlation("corr-1")
                .labels("workspace", "track"),
        );
    }

    fn emit_retry_storm(log: &mut MissionEventLog, cycle_id: u64) {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::RetryStormThrottled,
                "mission.safety.retry_storm",
            )
            .cycle(cycle_id, 1000)
            .correlation("corr-1")
            .labels("workspace", "track"),
        );
    }

    // ── Perfect match tests ─────────────────────────────────────────────

    #[test]
    fn perfect_match_has_full_fidelity() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.8, 2),
        ]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");
        emit_dispatch(&mut log, 1, "b2", "a2");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.recommendations_count, 2);
        assert_eq!(diff.emissions_count, 2);
        assert!(diff.missing_executions.is_empty());
        assert!(diff.unexpected_executions.is_empty());
        assert!(diff.agent_divergences.is_empty());
        assert!((diff.dispatch_rate - 1.0).abs() < f64::EPSILON);
        assert!((diff.agent_match_rate - 1.0).abs() < f64::EPSILON);
        assert!(diff.fidelity_score >= 0.95);
        assert!(diff.is_healthy());
    }

    // ── Missing execution tests ─────────────────────────────────────────

    #[test]
    fn missing_execution_detected() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.8, 2),
        ]);
        // Only b1 dispatched, b2 missing
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.missing_executions.len(), 1);
        assert_eq!(diff.missing_executions[0].0, "b2");
        assert!((diff.dispatch_rate - 0.5).abs() < f64::EPSILON);
        assert!(!diff.is_healthy());
    }

    // ── Unexpected execution tests ──────────────────────────────────────

    #[test]
    fn unexpected_execution_detected() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");
        emit_dispatch(&mut log, 1, "b_extra", "a_extra");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.unexpected_executions.len(), 1);
        assert_eq!(diff.unexpected_executions[0].bead_id, "b_extra");
    }

    // ── Agent divergence tests ──────────────────────────────────────────

    #[test]
    fn agent_divergence_detected() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        // b1 dispatched but to a different agent
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a_other");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.agent_divergences.len(), 1);
        assert_eq!(diff.agent_divergences[0].recommended_agent, "a1");
        assert_eq!(diff.agent_divergences[0].actual_agent, "a_other");
        assert!(diff.agent_match_rate < 1.0);
    }

    #[test]
    fn agent_divergence_disabled_by_config() {
        let config = ShadowEvaluationConfig {
            track_agent_divergence: false,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a_other");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert!(diff.agent_divergences.is_empty());
    }

    // ── Score accuracy tests ────────────────────────────────────────────

    #[test]
    fn score_accuracy_tracked() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.4, 2),
        ]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");
        emit_rejection(&mut log, 1, "b2");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.score_accuracy.len(), 2);
        let b1_acc = diff
            .score_accuracy
            .iter()
            .find(|s| s.bead_id == "b1")
            .unwrap();
        assert!(b1_acc.was_dispatched);
        assert!(!b1_acc.safety_rejected);
        assert!((b1_acc.recommended_score - 0.9).abs() < 1e-10);

        let b2_acc = diff
            .score_accuracy
            .iter()
            .find(|s| s.bead_id == "b2")
            .unwrap();
        assert!(!b2_acc.was_dispatched);
        assert!(b2_acc.safety_rejected);
    }

    #[test]
    fn score_accuracy_disabled_by_config() {
        let config = ShadowEvaluationConfig {
            track_score_accuracy: false,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert!(diff.score_accuracy.is_empty());
    }

    // ── Safety event tests ──────────────────────────────────────────────

    #[test]
    fn safety_events_counted() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(Vec::new());
        let mut log = make_log();
        emit_safety(&mut log, 1);
        emit_safety(&mut log, 1);
        emit_retry_storm(&mut log, 1);

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.safety_gate_rejections, 2);
        assert_eq!(diff.retry_storm_throttles, 1);
    }

    // ── Conflict event tests ────────────────────────────────────────────

    #[test]
    fn conflict_events_counted() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(Vec::new());
        let mut log = make_log();
        emit_conflict(&mut log, 1, MissionEventKind::ConflictDetected);
        emit_conflict(&mut log, 1, MissionEventKind::ConflictAutoResolved);
        emit_conflict(&mut log, 1, MissionEventKind::ConflictAutoResolved);

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.conflicts_detected, 1);
        assert_eq!(diff.conflicts_auto_resolved, 2);
    }

    // ── Empty cycle tests ───────────────────────────────────────────────

    #[test]
    fn empty_cycle_is_healthy() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        let diff = eval.evaluate_cycle(1, 1000, &recs, &empty);

        assert!((diff.dispatch_rate - 1.0).abs() < f64::EPSILON);
        assert!((diff.fidelity_score - 1.0).abs() < f64::EPSILON);
        assert!(diff.is_healthy());
    }

    // ── History and metrics tests ───────────────────────────────────────

    #[test]
    fn history_accumulates() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        eval.evaluate_cycle(1, 1000, &recs, &empty);
        eval.evaluate_cycle(2, 2000, &recs, &empty);
        eval.evaluate_cycle(3, 3000, &recs, &empty);

        assert_eq!(eval.history().len(), 3);
        assert_eq!(eval.total_cycles(), 3);
    }

    #[test]
    fn history_evicts_when_full() {
        let config = ShadowEvaluationConfig {
            max_history: 2,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        eval.evaluate_cycle(1, 1000, &recs, &empty);
        eval.evaluate_cycle(2, 2000, &recs, &empty);
        eval.evaluate_cycle(3, 3000, &recs, &empty);

        assert_eq!(eval.history().len(), 2);
        assert_eq!(eval.history()[0].cycle_id, 2);
        assert_eq!(eval.history()[1].cycle_id, 3);
    }

    #[test]
    fn metrics_accumulate_correctly() {
        let mut eval = ShadowModeEvaluator::with_defaults();

        // Perfect cycle
        let recs1 = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log1 = make_log();
        emit_dispatch(&mut log1, 1, "b1", "a1");
        eval.evaluate_cycle(1, 1000, &recs1, log1.events());

        // Imperfect cycle (missing execution)
        let recs2 = make_assignment_set(vec![
            make_assignment("b2", "a2", 0.8, 1),
            make_assignment("b3", "a3", 0.7, 2),
        ]);
        let mut log2 = make_log();
        emit_dispatch(&mut log2, 2, "b2", "a2");
        eval.evaluate_cycle(2, 2000, &recs2, log2.events());

        let metrics = eval.metrics();
        assert_eq!(metrics.total_cycles, 2);
        assert_eq!(metrics.total_recommendations, 3);
        assert_eq!(metrics.total_dispatches, 2);
    }

    #[test]
    fn warmup_detection() {
        let config = ShadowEvaluationConfig {
            warmup_cycles: 3,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        assert!(!eval.is_warmed_up());
        eval.evaluate_cycle(1, 1000, &recs, &empty);
        assert!(!eval.is_warmed_up());
        eval.evaluate_cycle(2, 2000, &recs, &empty);
        assert!(!eval.is_warmed_up());
        eval.evaluate_cycle(3, 3000, &recs, &empty);
        assert!(eval.is_warmed_up());
    }

    #[test]
    fn low_fidelity_streak_tracking() {
        let config = ShadowEvaluationConfig {
            warmup_cycles: 0,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);

        // Two cycles with unexpected executions (low fidelity)
        let recs = make_assignment_set(Vec::new());
        let mut bad_log = make_log();
        emit_dispatch(&mut bad_log, 1, "b_extra", "a_extra");
        let bad_events = bad_log.events();

        eval.evaluate_cycle(1, 1000, &recs, bad_events);
        eval.evaluate_cycle(2, 2000, &recs, bad_events);

        assert_eq!(eval.metrics().low_fidelity_count, 2);
        assert_eq!(eval.metrics().max_consecutive_low_fidelity, 2);

        // Good cycle breaks the streak
        let good_events: Vec<MissionEvent> = Vec::new();
        eval.evaluate_cycle(3, 3000, &recs, &good_events);

        assert_eq!(eval.metrics().max_consecutive_low_fidelity, 2);

        // Another bad cycle starts a new streak
        eval.evaluate_cycle(4, 4000, &recs, bad_events);
        assert_eq!(eval.metrics().max_consecutive_low_fidelity, 2);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        eval.evaluate_cycle(1, 1000, &recs, log.events());
        assert_eq!(eval.total_cycles(), 1);
        assert_eq!(eval.history().len(), 1);

        eval.reset();
        assert_eq!(eval.total_cycles(), 0);
        assert!(eval.history().is_empty());
    }

    #[test]
    fn last_diff_returns_most_recent() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        assert!(eval.last_diff().is_none());

        eval.evaluate_cycle(1, 1000, &recs, &empty);
        assert_eq!(eval.last_diff().unwrap().cycle_id, 1);

        eval.evaluate_cycle(2, 2000, &recs, &empty);
        assert_eq!(eval.last_diff().unwrap().cycle_id, 2);
    }

    #[test]
    fn total_divergences_sums_all_types() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.8, 2),
        ]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a_other"); // divergence
        emit_dispatch(&mut log, 1, "b_extra", "a3"); // unexpected
        // b2 missing

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.missing_executions.len(), 1); // b2
        assert_eq!(diff.unexpected_executions.len(), 1); // b_extra
        assert_eq!(diff.agent_divergences.len(), 1); // b1: a1 → a_other
        assert_eq!(diff.total_divergences(), 3);
    }

    // ── Rejected candidate tracking ─────────────────────────────────────

    #[test]
    fn rejected_candidates_counted() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = AssignmentSet {
            assignments: vec![make_assignment("b1", "a1", 0.9, 1)],
            rejected: vec![RejectedCandidate {
                bead_id: "b_low".to_string(),
                score: 0.1,
                reasons: Vec::new(),
            }],
            solver_config: SolverConfig::default(),
        };
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.rejections_count, 1);
    }

    // ── Serde roundtrip ─────────────────────────────────────────────────

    #[test]
    fn diff_serde_roundtrip() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());
        let json = serde_json::to_string(&diff).unwrap();
        let restored: ShadowModeDiff = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.cycle_id, diff.cycle_id);
        assert_eq!(restored.recommendations_count, diff.recommendations_count);
        assert_eq!(restored.emissions_count, diff.emissions_count);
        assert!((restored.fidelity_score - diff.fidelity_score).abs() < 1e-10);
    }

    #[test]
    fn metrics_serde_roundtrip() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        eval.evaluate_cycle(1, 1000, &recs, log.events());

        let json = serde_json::to_string(eval.metrics()).unwrap();
        let restored: ShadowModeMetrics = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.total_cycles, 1);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = ShadowEvaluationConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: ShadowEvaluationConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.max_history, config.max_history);
        assert!(
            (restored.low_confidence_threshold - config.low_confidence_threshold).abs() < 1e-10
        );
    }
}
