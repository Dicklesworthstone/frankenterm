//! Shadow-mode evaluator: compare mission recommendations vs actual execution.
//!
//! # Wiring status (ft-majms) — NOT YET DRIVEN
//!
//! `evaluate_cycle` has **no production caller** today (every call site is a
//! test); the mission loop never invokes it. The shipped `ft` binary compiles
//! this module (`subprocess-bridge` is a default feature) but is read-only —
//! `ft doctor`'s `shadow_mode_doctor_report` is the only live entry point and
//! it honestly reports `NoLiveSamples` / "no live MissionLoop sample stream
//! yet". The summary and integration diagram below describe the *intended*
//! flow once the mission loop is wired (gated on ft-ok02z; tracked by ft-majms).
//!
//! Designed to run in non-invasive mode alongside the mission loop, capturing
//! each cycle's recommendations and comparing them against the events that were
//! actually emitted during dispatch.
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
use crate::shadow_experiment_harness::{DivergenceCounts, ExperimentLedger};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Stable schema version for the `ft doctor --json .shadow_mode` block.
pub const SHADOW_MODE_DOCTOR_SCHEMA_VERSION: &str = "ft.shadow-mode.doctor.v1";

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

impl ShadowEvaluationConfig {
    /// Validate operator-tunable thresholds before using them for classification.
    pub fn validate(&self) -> Result<(), ShadowEvaluationConfigError> {
        if self.low_confidence_threshold.is_finite()
            && (0.0..=1.0).contains(&self.low_confidence_threshold)
        {
            Ok(())
        } else {
            Err(ShadowEvaluationConfigError::InvalidLowConfidenceThreshold {
                value: self.low_confidence_threshold,
            })
        }
    }

    fn validated_low_confidence_threshold(&self) -> f64 {
        self.validate()
            .map(|()| self.low_confidence_threshold)
            .unwrap_or_else(|_| Self::default().low_confidence_threshold)
    }
}

/// Invalid shadow-mode evaluator configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShadowEvaluationConfigError {
    /// `low_confidence_threshold` must be finite and inside `0.0..=1.0`.
    InvalidLowConfidenceThreshold { value: f64 },
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
    /// Whether the planner score was below the configured low-confidence threshold.
    #[serde(default)]
    pub low_confidence: bool,
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
    /// Number of recommendations below the configured confidence threshold.
    #[serde(default)]
    pub low_confidence_recommendations: usize,

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

// ── Doctor report ───────────────────────────────────────────────────────────

/// Whether a shadow engine is actually receiving live samples in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowModeFeedState {
    /// The engine is observing live traffic and appending only shadow telemetry.
    ObservingLiveTraffic,
    /// The engine is wired and safe, but this doctor pass saw no samples.
    NoLiveSamples,
    /// The engine substrate exists, but the live feed is owned by a follow-up phase.
    DormantNotWired,
}

impl ShadowModeFeedState {
    /// Stable string used by plain doctor output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ObservingLiveTraffic => "observing_live_traffic",
            Self::NoLiveSamples => "no_live_samples",
            Self::DormantNotWired => "dormant_not_wired",
        }
    }
}

/// Aggregate doctor status for all shadow-mode engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowModeDoctorStatus {
    /// At least one engine is receiving live samples.
    Observing,
    /// No engines received samples in this doctor process, but at least one substrate is ready.
    ReadyNoSamples,
    /// Every listed engine still needs a live-feed wiring pass.
    DormantNotWired,
}

impl ShadowModeDoctorStatus {
    /// Stable string used by plain doctor output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observing => "observing",
            Self::ReadyNoSamples => "ready_no_samples",
            Self::DormantNotWired => "dormant_not_wired",
        }
    }
}

/// Per-engine decision/divergence counters emitted by `ft doctor --json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowModeDecisionCounts {
    /// Baseline decisions or real dispatches the shadow run compared against.
    pub baseline_decisions: u64,
    /// Candidate decisions the shadow engine would have made.
    pub shadow_decisions: u64,
    /// Exact baseline-vs-shadow matches.
    pub matches: u64,
    /// Safe semantic differences recorded by generic experiment ledgers.
    pub minor_divergences: u64,
    /// Outcome-changing differences recorded by generic experiment ledgers.
    pub major_divergences: u64,
    /// Mission recommendations that did not appear in execution events.
    pub missing_executions: u64,
    /// Mission execution events absent from recommendations.
    pub unexpected_executions: u64,
    /// Mission recommendations executed by a different agent.
    pub agent_divergences: u64,
    /// Low-confidence recommendations observed by the mission evaluator.
    pub low_confidence_recommendations: u64,
}

impl ShadowModeDecisionCounts {
    /// Total divergences represented by all divergence buckets.
    #[must_use]
    pub const fn total_divergences(self) -> u64 {
        self.minor_divergences
            .saturating_add(self.major_divergences)
            .saturating_add(self.missing_executions)
            .saturating_add(self.unexpected_executions)
            .saturating_add(self.agent_divergences)
    }

    fn from_divergence_counts(decisions: u64, counts: DivergenceCounts) -> Self {
        Self {
            baseline_decisions: decisions,
            shadow_decisions: decisions,
            matches: counts.match_count,
            minor_divergences: counts.minor_count,
            major_divergences: counts.major_count,
            ..Self::default()
        }
    }
}

/// One engine row in the shadow-mode doctor report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowModeEngineDoctorRow {
    pub engine_id: String,
    pub display_name: String,
    pub source_module: String,
    pub decision_surface: String,
    pub feed_state: ShadowModeFeedState,
    pub observe_only: bool,
    pub production_behavior_changed: bool,
    pub live_mutation_allowed: bool,
    pub counts: ShadowModeDecisionCounts,
    pub sampling_gaps: u64,
    pub last_cycle_id: Option<u64>,
    pub pending_reason: Option<String>,
    pub evidence: Vec<String>,
}

impl ShadowModeEngineDoctorRow {
    /// Row for a live or test-owned mission shadow evaluator.
    #[must_use]
    pub fn from_mission_evaluator(
        engine_id: impl Into<String>,
        display_name: impl Into<String>,
        evaluator: &ShadowModeEvaluator,
        pending_reason: Option<String>,
    ) -> Self {
        let mut counts = ShadowModeDecisionCounts {
            baseline_decisions: evaluator.metrics().total_dispatches,
            shadow_decisions: evaluator.metrics().total_recommendations,
            ..ShadowModeDecisionCounts::default()
        };
        for diff in evaluator.history() {
            let missing = usize_to_u64(diff.missing_executions.len());
            let unexpected = usize_to_u64(diff.unexpected_executions.len());
            let agent = usize_to_u64(diff.agent_divergences.len());
            counts.missing_executions = counts.missing_executions.saturating_add(missing);
            counts.unexpected_executions = counts.unexpected_executions.saturating_add(unexpected);
            counts.agent_divergences = counts.agent_divergences.saturating_add(agent);
            counts.low_confidence_recommendations = counts
                .low_confidence_recommendations
                .saturating_add(usize_to_u64(diff.low_confidence_recommendations));

            let matched = diff
                .emissions_count
                .saturating_sub(diff.unexpected_executions.len())
                .saturating_sub(diff.agent_divergences.len());
            counts.matches = counts.matches.saturating_add(usize_to_u64(matched));
        }

        Self {
            engine_id: engine_id.into(),
            display_name: display_name.into(),
            source_module: "shadow_mode_evaluator".to_string(),
            decision_surface: "MissionLoop AssignmentSet vs MissionEventLog".to_string(),
            feed_state: if evaluator.total_cycles() == 0 {
                ShadowModeFeedState::NoLiveSamples
            } else {
                ShadowModeFeedState::ObservingLiveTraffic
            },
            observe_only: true,
            production_behavior_changed: false,
            live_mutation_allowed: false,
            counts,
            sampling_gaps: 0,
            last_cycle_id: evaluator.last_diff().map(|diff| diff.cycle_id),
            pending_reason,
            evidence: vec!["crates/frankenterm-core/src/shadow_mode_evaluator.rs".to_string()],
        }
    }

    /// Row for a generic shadow-experiment ledger.
    #[must_use]
    pub fn from_experiment_ledger(
        engine_id: impl Into<String>,
        display_name: impl Into<String>,
        source_module: impl Into<String>,
        decision_surface: impl Into<String>,
        ledger: &ExperimentLedger,
        feed_state: ShadowModeFeedState,
        pending_reason: Option<String>,
    ) -> Self {
        let decisions = usize_to_u64(ledger.decisions.len());
        Self {
            engine_id: engine_id.into(),
            display_name: display_name.into(),
            source_module: source_module.into(),
            decision_surface: decision_surface.into(),
            feed_state,
            observe_only: true,
            production_behavior_changed: false,
            live_mutation_allowed: false,
            counts: ShadowModeDecisionCounts::from_divergence_counts(
                decisions,
                ledger.divergence_counts(),
            ),
            sampling_gaps: ledger.sampling_gaps,
            last_cycle_id: None,
            pending_reason,
            evidence: vec!["crates/frankenterm-core/src/shadow_experiment_harness.rs".to_string()],
        }
    }

    /// Row for a known engine whose observe-only live feed has not landed yet.
    #[must_use]
    pub fn dormant(
        engine_id: impl Into<String>,
        display_name: impl Into<String>,
        source_module: impl Into<String>,
        decision_surface: impl Into<String>,
        pending_reason: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            engine_id: engine_id.into(),
            display_name: display_name.into(),
            source_module: source_module.into(),
            decision_surface: decision_surface.into(),
            feed_state: ShadowModeFeedState::DormantNotWired,
            observe_only: true,
            production_behavior_changed: false,
            live_mutation_allowed: false,
            counts: ShadowModeDecisionCounts::default(),
            sampling_gaps: 0,
            last_cycle_id: None,
            pending_reason: Some(pending_reason.into()),
            evidence: vec![evidence.into()],
        }
    }
}

/// Aggregate counts across all shadow-mode doctor rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowModeDoctorTotals {
    pub engines_total: u64,
    pub engines_observing_live_traffic: u64,
    pub engines_ready_without_samples: u64,
    pub engines_dormant_not_wired: u64,
    pub baseline_decisions: u64,
    pub shadow_decisions: u64,
    pub matches: u64,
    pub total_divergences: u64,
    pub sampling_gaps: u64,
}

impl ShadowModeDoctorTotals {
    fn from_rows(rows: &[ShadowModeEngineDoctorRow]) -> Self {
        let mut totals = Self {
            engines_total: usize_to_u64(rows.len()),
            ..Self::default()
        };
        for row in rows {
            match row.feed_state {
                ShadowModeFeedState::ObservingLiveTraffic => {
                    totals.engines_observing_live_traffic =
                        totals.engines_observing_live_traffic.saturating_add(1);
                }
                ShadowModeFeedState::NoLiveSamples => {
                    totals.engines_ready_without_samples =
                        totals.engines_ready_without_samples.saturating_add(1);
                }
                ShadowModeFeedState::DormantNotWired => {
                    totals.engines_dormant_not_wired =
                        totals.engines_dormant_not_wired.saturating_add(1);
                }
            }
            totals.baseline_decisions = totals
                .baseline_decisions
                .saturating_add(row.counts.baseline_decisions);
            totals.shadow_decisions = totals
                .shadow_decisions
                .saturating_add(row.counts.shadow_decisions);
            totals.matches = totals.matches.saturating_add(row.counts.matches);
            totals.total_divergences = totals
                .total_divergences
                .saturating_add(row.counts.total_divergences());
            totals.sampling_gaps = totals.sampling_gaps.saturating_add(row.sampling_gaps);
        }
        totals
    }
}

/// Stable `ft doctor --json .shadow_mode` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowModeDoctorReport {
    pub schema_version: String,
    pub status: ShadowModeDoctorStatus,
    pub observe_only: bool,
    pub production_behavior_changed: bool,
    pub live_mutation_allowed: bool,
    pub totals: ShadowModeDoctorTotals,
    pub engines: Vec<ShadowModeEngineDoctorRow>,
}

impl ShadowModeDoctorReport {
    /// Build a report from explicit per-engine rows.
    #[must_use]
    pub fn from_engines(engines: Vec<ShadowModeEngineDoctorRow>) -> Self {
        let totals = ShadowModeDoctorTotals::from_rows(&engines);
        let status = if totals.engines_observing_live_traffic > 0 {
            ShadowModeDoctorStatus::Observing
        } else if totals.engines_ready_without_samples > 0 {
            ShadowModeDoctorStatus::ReadyNoSamples
        } else {
            ShadowModeDoctorStatus::DormantNotWired
        };
        Self {
            schema_version: SHADOW_MODE_DOCTOR_SCHEMA_VERSION.to_string(),
            status,
            observe_only: true,
            production_behavior_changed: false,
            live_mutation_allowed: false,
            totals,
            engines,
        }
    }
}

/// Build the standalone CLI doctor report.
///
/// The CLI process is read-only and does not own live MissionLoop or watcher
/// state. The report therefore exposes honest zero-count rows for ready
/// substrates and explicit `dormant_not_wired` rows for W4 follow-up engines.
#[must_use]
pub fn shadow_mode_doctor_report() -> ShadowModeDoctorReport {
    let mission_evaluator = ShadowModeEvaluator::with_defaults();
    let mut rows = vec![ShadowModeEngineDoctorRow::from_mission_evaluator(
        "mission_loop_assignments",
        "Mission assignment shadow evaluator",
        &mission_evaluator,
        Some("standalone ft doctor has no live MissionLoop sample stream yet".to_string()),
    )];

    rows.push(ShadowModeEngineDoctorRow {
        engine_id: "generic_shadow_experiments".to_string(),
        display_name: "Generic shadow experiment harness".to_string(),
        source_module: "shadow_experiment_harness".to_string(),
        decision_surface: "baseline vs candidate decision ledgers".to_string(),
        feed_state: ShadowModeFeedState::NoLiveSamples,
        observe_only: true,
        production_behavior_changed: false,
        live_mutation_allowed: false,
        counts: ShadowModeDecisionCounts::default(),
        sampling_gaps: 0,
        last_cycle_id: None,
        pending_reason: Some(
            "no live experiment ledger was supplied to this doctor process".to_string(),
        ),
        evidence: vec!["crates/frankenterm-core/src/shadow_experiment_harness.rs".to_string()],
    });

    rows.extend([
        ShadowModeEngineDoctorRow::dormant(
            "bocpd_change_points",
            "BOCPD change-point detector",
            "bocpd",
            "watcher detect-stage pane-output regime changes",
            "W4.2 must wire BOCPD into the live watcher detect stage before counts become non-zero",
            "crates/frankenterm-core/src/bocpd.rs",
        ),
        ShadowModeEngineDoctorRow::dormant(
            "connector_reliability_governor",
            "Connector reliability and quota governor",
            "connector_governor",
            "outbound connector dispatch decisions",
            "W4.3 must run connector governor decisions beside dispatch before divergence can be measured",
            "crates/frankenterm-core/src/connector_governor.rs",
        ),
        ShadowModeEngineDoctorRow::dormant(
            "capacity_governor_admission",
            "Capacity governor admission",
            "swarm_scheduler",
            "resource-pressure admission and quality decisions",
            "W4.4 must route capacity-governor dry-run decisions into the operating envelope actuator",
            "crates/frankenterm-core/src/swarm_scheduler.rs",
        ),
    ]);

    ShadowModeDoctorReport::from_engines(rows)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

// ── Evaluator ───────────────────────────────────────────────────────────────

/// Shadow-mode evaluator that compares recommendations against execution.
///
/// Non-invasive: reads from AssignmentSet and MissionEventLog without
/// modifying any mission state.
#[derive(Debug)]
pub struct ShadowModeEvaluator {
    config: ShadowEvaluationConfig,
    history: VecDeque<ShadowModeDiff>,
    metrics: ShadowModeMetrics,
}

impl ShadowModeEvaluator {
    /// Create a new evaluator with the given configuration.
    pub fn new(config: ShadowEvaluationConfig) -> Self {
        Self {
            config,
            history: VecDeque::new(),
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
    /// `events` may contain events from adjacent cycles; only matching
    /// `cycle_id` events are considered.
    /// `cycle_id` and `timestamp_ms` identify the cycle.
    pub fn evaluate_cycle(
        &mut self,
        cycle_id: u64,
        timestamp_ms: i64,
        recommendations: &AssignmentSet,
        events: impl AsRef<[MissionEvent]>,
    ) -> ShadowModeDiff {
        let events = events.as_ref();
        let cycle_events: Vec<MissionEvent> = events
            .iter()
            .filter(|event| event.cycle_id == cycle_id)
            .cloned()
            .collect();
        let diff = compute_diff(
            cycle_id,
            timestamp_ms,
            recommendations,
            &cycle_events,
            &self.config,
        );

        self.metrics.update(&diff);

        // Store in history with FIFO eviction
        if self.history.len() >= self.config.max_history {
            self.history.pop_front();
        }
        self.history.push_back(diff.clone());

        diff
    }

    /// Get the running aggregate metrics.
    pub fn metrics(&self) -> &ShadowModeMetrics {
        &self.metrics
    }

    /// Get the most recent diff.
    pub fn last_diff(&self) -> Option<&ShadowModeDiff> {
        self.history.back()
    }

    /// Get the diff history.
    pub fn history(&self) -> Vec<ShadowModeDiff> {
        self.history.iter().cloned().collect()
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

/// A dispatch event normalized for shadow-mode comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchRecord {
    bead_id: String,
    agent_id: String,
    reason_code: String,
}

/// Extract dispatch events from mission events.
fn extract_dispatches(events: &[MissionEvent]) -> Vec<DispatchRecord> {
    let mut seen = HashSet::new();
    let mut dispatches = Vec::new();

    for event in events
        .iter()
        .filter(|e| e.kind == MissionEventKind::AssignmentEmitted)
    {
        let Some(bead_id) = event.details.get("bead_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(agent_id) = event.details.get("agent_id").and_then(|v| v.as_str()) else {
            continue;
        };

        if seen.insert((bead_id.to_string(), agent_id.to_string())) {
            dispatches.push(DispatchRecord {
                bead_id: bead_id.to_string(),
                agent_id: agent_id.to_string(),
                reason_code: event.reason_code.clone(),
            });
        }
    }

    dispatches
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

    let dispatched_set: HashMap<&str, &DispatchRecord> = dispatches
        .iter()
        .map(|dispatch| (dispatch.bead_id.as_str(), dispatch))
        .collect();

    let rejected_bead_set: HashSet<&str> =
        execution_rejections.iter().map(|s| s.as_str()).collect();
    let low_confidence_threshold = config.validated_low_confidence_threshold();
    let low_confidence_recommendations = recommendations
        .assignments
        .iter()
        .filter(|assignment| {
            !assignment.score.is_finite() || assignment.score < low_confidence_threshold
        })
        .count();

    // Find missing executions: recommended but not dispatched
    let mut missing_executions = Vec::new();
    for assignment in &recommendations.assignments {
        if !dispatched_set.contains_key(assignment.bead_id.as_str()) {
            missing_executions.push((assignment.bead_id.clone(), assignment.agent_id.clone()));
        }
    }

    // Find unexpected executions: dispatched but not recommended
    let mut unexpected_executions = Vec::new();
    for dispatch in &dispatches {
        if !recommended_set.contains_key(dispatch.bead_id.as_str()) {
            unexpected_executions.push(UnexpectedExecution {
                bead_id: dispatch.bead_id.clone(),
                agent_id: dispatch.agent_id.clone(),
                reason_code: dispatch.reason_code.clone(),
            });
        }
    }

    // Agent divergences: recommended agent != dispatched agent
    let mut agent_divergences = Vec::new();
    if config.track_agent_divergence {
        for assignment in &recommendations.assignments {
            if let Some(dispatch) = dispatched_set.get(assignment.bead_id.as_str()) {
                if dispatch.agent_id != assignment.agent_id {
                    agent_divergences.push(AgentDivergence {
                        bead_id: assignment.bead_id.clone(),
                        recommended_agent: assignment.agent_id.clone(),
                        actual_agent: dispatch.agent_id.clone(),
                        reason_code: dispatch.reason_code.clone(),
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
                low_confidence: !assignment.score.is_finite()
                    || assignment.score < low_confidence_threshold,
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
                .is_some_and(|dispatch| dispatch.agent_id == a.agent_id)
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
        low_confidence_recommendations,
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
    use crate::shadow_experiment_harness::{
        DivergenceKind, ExperimentBudget, ExperimentManifest, ShadowDecisionPair,
        ShadowExperimentHarness,
    };
    use proptest::prelude::*;

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
        emit_dispatch_with_reason(
            log,
            cycle_id,
            bead_id,
            agent_id,
            "mission.dispatch.assignment_emitted",
        );
    }

    fn emit_dispatch_with_reason(
        log: &mut MissionEventLog,
        cycle_id: u64,
        bead_id: &str,
        agent_id: &str,
        reason_code: &str,
    ) {
        log.emit(
            MissionEventBuilder::new(MissionEventKind::AssignmentEmitted, reason_code)
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

    fn experiment_manifest() -> ExperimentManifest {
        ExperimentManifest {
            experiment_id: "exp-shadow-doctor".to_string(),
            baseline_label: "baseline".to_string(),
            candidate_label: "candidate".to_string(),
            budget: ExperimentBudget::conservative(),
            redaction_policy_version: "redaction-v1".to_string(),
        }
    }

    fn shadow_pair(
        event_id: &str,
        divergence: DivergenceKind,
        overhead_us: u64,
    ) -> ShadowDecisionPair {
        ShadowDecisionPair {
            event_id: event_id.to_string(),
            baseline_decision: "baseline:allow".to_string(),
            candidate_decision: "candidate:allow".to_string(),
            divergence,
            overhead_us,
            evidence_citations: vec![format!("event:{event_id}")],
        }
    }

    #[test]
    fn shadow_mode_doctor_report_lists_engines_without_mutation() {
        let report = shadow_mode_doctor_report();

        assert_eq!(report.schema_version, SHADOW_MODE_DOCTOR_SCHEMA_VERSION);
        assert_eq!(report.status, ShadowModeDoctorStatus::ReadyNoSamples);
        assert!(report.observe_only);
        assert!(!report.live_mutation_allowed);
        assert!(!report.production_behavior_changed);
        assert_eq!(report.totals.engines_total, 5);
        assert_eq!(report.totals.shadow_decisions, 0);
        assert_eq!(report.totals.total_divergences, 0);
        assert!(
            report
                .engines
                .iter()
                .any(|engine| engine.engine_id == "bocpd_change_points"
                    && engine.feed_state == ShadowModeFeedState::DormantNotWired)
        );
        assert!(report.engines.iter().all(|engine| engine.observe_only
            && !engine.live_mutation_allowed
            && !engine.production_behavior_changed));
    }

    #[test]
    fn shadow_mode_doctor_row_summarizes_mission_evaluator_counts() {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.2, 2),
        ]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a-other");
        emit_dispatch(&mut log, 1, "b-extra", "a3");

        eval.evaluate_cycle(1, 1000, &recs, log.events());
        let row =
            ShadowModeEngineDoctorRow::from_mission_evaluator("mission", "Mission", &eval, None);

        assert_eq!(row.feed_state, ShadowModeFeedState::ObservingLiveTraffic);
        assert_eq!(row.counts.shadow_decisions, 2);
        assert_eq!(row.counts.baseline_decisions, 2);
        assert_eq!(row.counts.matches, 0);
        assert_eq!(row.counts.missing_executions, 1);
        assert_eq!(row.counts.unexpected_executions, 1);
        assert_eq!(row.counts.agent_divergences, 1);
        assert_eq!(row.counts.low_confidence_recommendations, 1);
        assert_eq!(row.counts.total_divergences(), 3);
        assert_eq!(row.last_cycle_id, Some(1));
    }

    #[test]
    fn shadow_mode_doctor_report_summarizes_experiment_ledger_counts() {
        let mut harness = ShadowExperimentHarness::new(experiment_manifest());
        assert!(harness.record_pair(shadow_pair("e1", DivergenceKind::Match, 10)));
        assert!(harness.record_pair(shadow_pair("e2", DivergenceKind::Minor, 10)));
        assert!(harness.record_pair(shadow_pair("e3", DivergenceKind::Major, 10)));

        let row = ShadowModeEngineDoctorRow::from_experiment_ledger(
            "experiment",
            "Experiment",
            "shadow_experiment_harness",
            "baseline vs candidate",
            harness.ledger(),
            ShadowModeFeedState::ObservingLiveTraffic,
            None,
        );
        let report = ShadowModeDoctorReport::from_engines(vec![row]);

        assert_eq!(report.status, ShadowModeDoctorStatus::Observing);
        assert_eq!(report.totals.engines_observing_live_traffic, 1);
        assert_eq!(report.totals.shadow_decisions, 3);
        assert_eq!(report.totals.baseline_decisions, 3);
        assert_eq!(report.totals.matches, 1);
        assert_eq!(report.totals.total_divergences, 2);
        assert_eq!(report.engines[0].counts.minor_divergences, 1);
        assert_eq!(report.engines[0].counts.major_divergences, 1);
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

    proptest! {
        #[test]
        fn duplicate_dispatches_do_not_inflate_fidelity(duplicates in 1usize..32) {
            let mut eval = ShadowModeEvaluator::with_defaults();
            let recs = make_assignment_set(vec![
                make_assignment("b1", "a1", 0.9, 1),
                make_assignment("b2", "a2", 0.8, 2),
            ]);
            let mut log = make_log();

            for _ in 0..duplicates {
                emit_dispatch(&mut log, 1, "b1", "a1");
            }

            let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

            prop_assert_eq!(diff.emissions_count, 1);
            prop_assert_eq!(
                &diff.missing_executions,
                &vec![("b2".to_string(), "a2".to_string())]
            );
            prop_assert!((diff.dispatch_rate - 0.5).abs() < f64::EPSILON);
            prop_assert!(diff.fidelity_score < 0.8);
            prop_assert!(!diff.is_healthy());
        }

        #[test]
        fn evaluate_cycle_filters_events_to_requested_cycle_ft_4x2c9(
            prior_noise in 0usize..16,
            future_noise in 0usize..16,
            target_dispatched in any::<bool>(),
        ) {
            let mut eval = ShadowModeEvaluator::with_defaults();
            let recs = make_assignment_set(vec![make_assignment("b-current", "a-current", 0.9, 1)]);
            let mut log = make_log();

            for idx in 0..prior_noise {
                emit_dispatch(&mut log, 40, &format!("b-prior-{idx}"), "a-prior");
                emit_safety(&mut log, 40);
                emit_conflict(&mut log, 40, MissionEventKind::ConflictDetected);
            }

            if target_dispatched {
                emit_dispatch(&mut log, 41, "b-current", "a-current");
            }

            for idx in 0..future_noise {
                emit_dispatch(&mut log, 42, &format!("b-future-{idx}"), "a-future");
                emit_rejection(&mut log, 42, "b-current");
                emit_retry_storm(&mut log, 42);
                emit_conflict(&mut log, 42, MissionEventKind::ConflictAutoResolved);
            }

            let diff = eval.evaluate_cycle(41, 1000, &recs, log.events());

            prop_assert_eq!(diff.emissions_count, usize::from(target_dispatched));
            prop_assert!(diff.unexpected_executions.is_empty());
            prop_assert_eq!(diff.safety_gate_rejections, 0);
            prop_assert_eq!(diff.retry_storm_throttles, 0);
            prop_assert_eq!(diff.conflicts_detected, 0);
            prop_assert_eq!(diff.conflicts_auto_resolved, 0);

            if target_dispatched {
                prop_assert!(diff.missing_executions.is_empty());
                prop_assert!((diff.dispatch_rate - 1.0).abs() < f64::EPSILON);
                prop_assert!((diff.agent_match_rate - 1.0).abs() < f64::EPSILON);
                prop_assert!(diff.is_healthy());
            } else {
                prop_assert_eq!(
                    &diff.missing_executions,
                    &vec![("b-current".to_string(), "a-current".to_string())]
                );
                prop_assert!((diff.dispatch_rate - 0.0).abs() < f64::EPSILON);
                prop_assert!(!diff.is_healthy());
            }
        }
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

    proptest! {
        #[test]
        fn agent_divergence_preserves_dispatch_reason_code_ft_2cp5g(
            reason_code in "[a-z][a-z0-9_.]{0,48}",
        ) {
            let mut eval = ShadowModeEvaluator::with_defaults();
            let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
            let mut log = make_log();
            emit_dispatch_with_reason(&mut log, 1, "b1", "a_other", &reason_code);

            let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

            prop_assert_eq!(diff.agent_divergences.len(), 1);
            prop_assert_eq!(&diff.agent_divergences[0].recommended_agent, "a1");
            prop_assert_eq!(&diff.agent_divergences[0].actual_agent, "a_other");
            prop_assert_eq!(&diff.agent_divergences[0].reason_code, &reason_code);
        }
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
    fn score_accuracy_classifies_low_confidence_assignments() {
        let config = ShadowEvaluationConfig {
            low_confidence_threshold: 0.5,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(vec![
            make_assignment("b-low", "a1", 0.49, 1),
            make_assignment("b-boundary", "a1", 0.5, 2),
            make_assignment("b-high", "a1", 0.9, 3),
        ]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b-low", "a1");
        emit_dispatch(&mut log, 1, "b-boundary", "a1");
        emit_dispatch(&mut log, 1, "b-high", "a1");

        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        assert_eq!(diff.low_confidence_recommendations, 1);
        assert!(
            diff.score_accuracy
                .iter()
                .find(|record| record.bead_id == "b-low")
                .unwrap()
                .low_confidence
        );
        assert!(
            !diff
                .score_accuracy
                .iter()
                .find(|record| record.bead_id == "b-boundary")
                .unwrap()
                .low_confidence
        );
        assert!(
            !diff
                .score_accuracy
                .iter()
                .find(|record| record.bead_id == "b-high")
                .unwrap()
                .low_confidence
        );
    }

    #[test]
    fn config_validation_rejects_invalid_low_confidence_threshold() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 1.01] {
            let config = ShadowEvaluationConfig {
                low_confidence_threshold: value,
                ..Default::default()
            };
            assert!(matches!(
                config.validate(),
                Err(ShadowEvaluationConfigError::InvalidLowConfidenceThreshold { .. })
            ));
        }

        for value in [0.0, 0.5, 1.0] {
            let config = ShadowEvaluationConfig {
                low_confidence_threshold: value,
                ..Default::default()
            };
            assert!(config.validate().is_ok());
        }
    }

    proptest! {
        #[test]
        fn score_accuracy_low_confidence_matches_valid_threshold(
            score in 0.0f64..=1.0,
            threshold in 0.0f64..=1.0,
            dispatched in any::<bool>(),
        ) {
            let config = ShadowEvaluationConfig {
                low_confidence_threshold: threshold,
                ..Default::default()
            };
            let mut eval = ShadowModeEvaluator::new(config);
            let recs = make_assignment_set(vec![make_assignment("b1", "a1", score, 1)]);
            let mut log = make_log();
            if dispatched {
                emit_dispatch(&mut log, 1, "b1", "a1");
            } else {
                emit_rejection(&mut log, 1, "b1");
            }

            let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());
            let expected = score < threshold;

            prop_assert_eq!(diff.low_confidence_recommendations, usize::from(expected));
            prop_assert_eq!(diff.score_accuracy.len(), 1);
            prop_assert_eq!(diff.score_accuracy[0].low_confidence, expected);
        }
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

        eval.evaluate_cycle(1, 1000, &recs, &bad_events);
        eval.evaluate_cycle(2, 2000, &recs, &bad_events);

        assert_eq!(eval.metrics().low_fidelity_count, 2);
        assert_eq!(eval.metrics().max_consecutive_low_fidelity, 2);

        // Good cycle breaks the streak
        let good_events: Vec<MissionEvent> = Vec::new();
        eval.evaluate_cycle(3, 3000, &recs, &good_events);

        assert_eq!(eval.metrics().max_consecutive_low_fidelity, 2);

        // Another bad cycle starts a new streak
        eval.evaluate_cycle(4, 4000, &recs, &bad_events);
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
