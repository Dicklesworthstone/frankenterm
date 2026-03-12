//! Mission control-loop engine (ft-1i2ge.3.1).
//!
//! Implements the mission loop cadence and event-triggered reevaluation.
//! Orchestrates the full planner pipeline:
//!   readiness → features → scoring → solving → decisions
//!
//! ## Conflict Detection (ft-1i2ge.4.5)
//!
//! After assignment solving and safety envelope enforcement, the loop can
//! detect assignment conflicts across three dimensions:
//!
//! - **File reservation overlaps**: Two assignments targeting overlapping
//!   file paths (using wildcard-aware path matching).
//! - **Resource reservation overlaps**: Two assignments targeting the same
//!   logical resource scope (for example panes, sockets, or named locks).
//! - **Concurrent bead claims**: Multiple agents assigned the same bead
//!   in the same cycle, or a bead already claimed by an active agent.
//!
//! Detected conflicts produce structured `DeconflictionMessage` payloads
//! that the caller dispatches via agent mail or other coordination channels.
//!
//! The loop is synchronous and deterministic — it does not spawn threads
//! or use async. The caller drives the loop by calling `tick()` or `trigger()`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::beads_types::{BeadIssueDetail, BeadReadinessReport};
use crate::plan::MissionAgentCapabilityProfile;
use crate::planner_features::{
    Assignment, AssignmentSet, PlannerExtractionConfig, PlannerExtractionContext,
    PlannerExtractionReport, RejectedCandidate, RejectionReason, SafetyGate, ScorerConfig,
    ScorerInput, ScorerReport, SolverConfig, extract_planner_features, score_candidates,
    solve_assignments,
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

fn default_metrics_workspace_label() -> String {
    "default".to_string()
}

fn default_metrics_track_label() -> String {
    "mission".to_string()
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

/// Dimension labels for mission metrics segmentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionMetricsLabels {
    #[serde(default = "default_metrics_workspace_label")]
    pub workspace: String,
    #[serde(default = "default_metrics_track_label")]
    pub track: String,
}

impl Default for MissionMetricsLabels {
    fn default() -> Self {
        Self {
            workspace: default_metrics_workspace_label(),
            track: default_metrics_track_label(),
        }
    }
}

/// Mission metrics instrumentation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionMetricsConfig {
    /// Enable or disable mission metrics collection.
    pub enabled: bool,
    /// Maximum retained cycle samples (bounded memory/overhead).
    pub max_samples: usize,
    /// Segmentation labels carried with each sample.
    #[serde(default)]
    pub labels: MissionMetricsLabels,
}

impl Default for MissionMetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_samples: 256,
            labels: MissionMetricsLabels::default(),
        }
    }
}

// ── Conflict detection (ft-1i2ge.4.5) ──────────────────────────────────────

/// Strategy for auto-resolving assignment conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeconflictionStrategy {
    /// Higher-priority (lower numeric value) assignment wins.
    PriorityWins,
    /// First claim (earlier timestamp) wins.
    FirstClaimWins,
    /// Surface to operator for manual resolution.
    ManualResolution,
}

/// The kind of detected assignment conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictType {
    /// Two assignments touch overlapping file paths.
    FileReservationOverlap,
    /// Two assignments touch overlapping logical resource scopes.
    ResourceReservationOverlap,
    /// Multiple agents assigned the same bead in one cycle.
    ConcurrentBeadClaim,
    /// An assignment targets a bead already actively claimed.
    ActiveClaimCollision,
}

/// How a conflict was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    /// Auto-resolved: winner keeps assignment, loser is rejected.
    AutoResolved {
        winner_agent: String,
        loser_agent: String,
        strategy: DeconflictionStrategy,
    },
    /// Deferred: both assignments held, retry after specified time.
    Deferred { retry_after_ms: i64 },
    /// Requires manual operator resolution.
    PendingManualResolution,
}

/// A detected assignment conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentConflict {
    pub conflict_id: String,
    pub conflict_type: ConflictType,
    pub involved_agents: Vec<String>,
    pub involved_beads: Vec<String>,
    /// Conflicting scope identifiers (paths or resource ids).
    pub conflicting_paths: Vec<String>,
    pub detected_at_ms: i64,
    pub resolution: ConflictResolution,
    pub reason_code: String,
    pub error_code: String,
}

/// A deconfliction message to be dispatched via agent mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeconflictionMessage {
    pub recipient: String,
    pub subject: String,
    pub body: String,
    pub thread_id: String,
    pub importance: String,
    pub conflict_id: String,
}

/// Report from a conflict detection pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictDetectionReport {
    pub cycle_id: u64,
    pub detected_at_ms: i64,
    pub conflicts: Vec<AssignmentConflict>,
    pub messages: Vec<DeconflictionMessage>,
    pub auto_resolved_count: usize,
    pub pending_resolution_count: usize,
}

/// Known reservation held by an agent (for conflict detection input).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownReservation {
    pub holder: String,
    pub paths: Vec<String>,
    pub exclusive: bool,
    pub bead_id: Option<String>,
    pub expires_at_ms: Option<i64>,
}

/// Known logical resource reservation held by an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownResourceReservation {
    pub holder: String,
    pub resources: Vec<String>,
    pub exclusive: bool,
    pub bead_id: Option<String>,
    pub expires_at_ms: Option<i64>,
}

/// Known active claim on a bead (for active-claim collision detection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveBeadClaim {
    pub bead_id: String,
    pub agent_id: String,
    pub claimed_at_ms: i64,
}

/// Configuration for conflict detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictDetectionConfig {
    /// Enable conflict detection. When disabled, `detect_conflicts` is a no-op.
    pub enabled: bool,
    /// Maximum conflicts to surface per cycle (prevents flood).
    pub max_conflicts_per_cycle: usize,
    /// Strategy for auto-resolving detected conflicts.
    pub strategy: DeconflictionStrategy,
    /// Whether to include deconfliction messages in the report.
    pub generate_messages: bool,
}

impl Default for ConflictDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_conflicts_per_cycle: 20,
            strategy: DeconflictionStrategy::PriorityWins,
            generate_messages: true,
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
    /// Mission-level instrumentation and rate metrics.
    #[serde(default)]
    pub metrics: MissionMetricsConfig,
    /// Conflict detection and deconfliction messaging.
    #[serde(default)]
    pub conflict_detection: ConflictDetectionConfig,
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
            metrics: MissionMetricsConfig::default(),
            conflict_detection: ConflictDetectionConfig::default(),
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

/// Aggregate mission metrics counters retained in loop state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MissionMetricsTotals {
    pub cycles: u64,
    pub assignments: u64,
    pub rejections: u64,
    pub conflict_rejections: u64,
    pub policy_denials: u64,
    pub unblocked_transitions: u64,
    pub planner_churn_events: u64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub assignments_by_agent: HashMap<String, u64>,
}

/// Per-cycle mission metrics sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionCycleMetricsSample {
    pub cycle_id: u64,
    pub timestamp_ms: i64,
    pub evaluation_latency_ms: u64,
    pub assignments: usize,
    pub rejections: usize,
    pub conflict_rejections: usize,
    pub policy_denials: usize,
    pub unblocked_transitions: usize,
    pub planner_churn_events: usize,
    pub throughput_assignments_per_minute: f64,
    pub unblock_velocity_per_minute: f64,
    pub conflict_rate: f64,
    pub planner_churn_rate: f64,
    pub policy_deny_rate: f64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub assignments_by_agent: HashMap<String, u64>,
    pub workspace_label: String,
    pub track_label: String,
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
    /// Bounded per-cycle metrics samples.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics_history: Vec<MissionCycleMetricsSample>,
    /// Aggregate totals over all evaluated cycles.
    #[serde(default)]
    pub metrics_totals: MissionMetricsTotals,
    /// Previous cycle ready set for unblock velocity accounting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_ready_ids: Vec<String>,
    /// Previous cycle assignment map for planner churn accounting.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub previous_assignment_by_bead: HashMap<String, String>,
    /// Conflict history from recent detection passes (bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_history: Vec<AssignmentConflict>,
    /// Total conflicts detected across all cycles.
    #[serde(default)]
    pub total_conflicts_detected: u64,
    /// Total auto-resolved conflicts.
    #[serde(default)]
    pub total_conflicts_auto_resolved: u64,
    /// Operator override state (pin/exclude/reprioritize controls).
    #[serde(default)]
    pub override_state: OperatorOverrideState,
    /// Summary from last override application (if any overrides were active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_override_summary: Option<OverrideApplicationSummary>,
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
                metrics_history: Vec::new(),
                metrics_totals: MissionMetricsTotals::default(),
                previous_ready_ids: Vec::new(),
                previous_assignment_by_bead: HashMap::new(),
                conflict_history: Vec::new(),
                total_conflicts_detected: 0,
                total_conflicts_auto_resolved: 0,
                override_state: OperatorOverrideState::default(),
                last_override_summary: None,
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

    /// Latest recorded mission metrics sample.
    #[must_use]
    pub fn latest_metrics(&self) -> Option<&MissionCycleMetricsSample> {
        self.state.metrics_history.last()
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
        let eval_started = Instant::now();
        let previous_evaluation_ms = self.state.last_evaluation_ms;
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

        let mut scorer_report: ScorerReport =
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

        // Phase 3.5: Apply operator overrides (pin/exclude/reprioritize).
        let mut solver_config = self.config.solver_config.clone();
        let override_summary = self.apply_operator_overrides(
            current_ms,
            &mut scorer_report,
            agents,
            &mut solver_config,
        );
        // Generate pinned assignments from overrides.
        let pinned: Vec<Assignment> = override_summary
            .pinned_assignments
            .iter()
            .enumerate()
            .map(|(i, pin)| Assignment {
                bead_id: pin.bead_id.clone(),
                agent_id: pin.agent_id.clone(),
                score: 1.0, // Pinned assignments get maximum score.
                rank: i + 1,
            })
            .collect();
        self.state.last_override_summary = if override_summary.excluded_beads.is_empty()
            && override_summary.excluded_agents.is_empty()
            && override_summary.pinned_assignments.is_empty()
            && override_summary.reprioritized_beads.is_empty()
            && override_summary.expired_overrides == 0
        {
            None
        } else {
            Some(override_summary)
        };

        // Filter out excluded agents from available agents for solving.
        let excluded_agents = &self.state.override_state.excluded_agent_ids();
        let filtered_agents: Vec<MissionAgentCapabilityProfile> = if excluded_agents.is_empty() {
            agents.to_vec()
        } else {
            agents
                .iter()
                .filter(|a| !excluded_agents.contains(&a.agent_id.as_str()))
                .cloned()
                .collect()
        };

        // Phase 4: Assignment solving.
        let mut assignment_set: AssignmentSet =
            solve_assignments(&scorer_report, &filtered_agents, &solver_config);
        // Prepend pinned assignments (they take priority).
        if !pinned.is_empty() {
            let pin_count = pinned.len();
            // Re-rank solver assignments after pins.
            for a in &mut assignment_set.assignments {
                a.rank += pin_count;
            }
            let mut combined = pinned;
            combined.append(&mut assignment_set.assignments);
            assignment_set.assignments = combined;
        }
        let assignment_set = self.apply_safety_envelope(assignment_set, issues);

        // Update state.
        self.state.total_assignments_made += assignment_set.assignments.len() as u64;
        self.state.total_rejections += assignment_set.rejected.len() as u64;
        self.state.last_evaluation_ms = Some(current_ms);
        self.state.pending_triggers.clear();
        let evaluation_latency_ms =
            eval_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        self.record_cycle_metrics(
            cycle_id,
            current_ms,
            previous_evaluation_ms,
            &readiness,
            &assignment_set,
            evaluation_latency_ms,
        );

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

    fn record_cycle_metrics(
        &mut self,
        cycle_id: u64,
        current_ms: i64,
        previous_evaluation_ms: Option<i64>,
        readiness: &BeadReadinessReport,
        assignment_set: &AssignmentSet,
        evaluation_latency_ms: u64,
    ) {
        if !self.config.metrics.enabled {
            return;
        }

        let mut assignments_by_agent: HashMap<String, u64> = HashMap::new();
        for assignment in &assignment_set.assignments {
            *assignments_by_agent
                .entry(assignment.agent_id.clone())
                .or_insert(0) += 1;
        }

        let conflict_rejections = assignment_set
            .rejected
            .iter()
            .filter(|rejected| {
                rejected
                    .reasons
                    .iter()
                    .any(|reason| matches!(reason, RejectionReason::ConflictWithAssigned { .. }))
            })
            .count();

        let policy_denials = assignment_set
            .rejected
            .iter()
            .filter(|rejected| {
                rejected
                    .reasons
                    .iter()
                    .any(|reason| matches!(reason, RejectionReason::SafetyGateDenied { .. }))
            })
            .count();

        let current_assignment_by_bead: HashMap<String, String> = assignment_set
            .assignments
            .iter()
            .map(|assignment| (assignment.bead_id.clone(), assignment.agent_id.clone()))
            .collect();

        let previous_assignment_keys: HashSet<&String> =
            self.state.previous_assignment_by_bead.keys().collect();
        let current_assignment_keys: HashSet<&String> = current_assignment_by_bead.keys().collect();
        let union_assignment_keys: HashSet<&String> = previous_assignment_keys
            .union(&current_assignment_keys)
            .copied()
            .collect();
        let planner_churn_events = union_assignment_keys
            .iter()
            .filter(|bead_id| {
                self.state.previous_assignment_by_bead.get(**bead_id)
                    != current_assignment_by_bead.get(**bead_id)
            })
            .count();
        let planner_churn_denominator = union_assignment_keys.len();

        let previous_ready_ids: HashSet<&String> = self.state.previous_ready_ids.iter().collect();
        let current_ready_ids: HashSet<&String> = readiness.ready_ids.iter().collect();
        let unblocked_transitions = if previous_evaluation_ms.is_none() {
            0
        } else {
            current_ready_ids
                .difference(&previous_ready_ids)
                .copied()
                .count()
        };

        let interval_ms = previous_evaluation_ms
            .and_then(|last| {
                if current_ms > last {
                    Some((current_ms - last) as u64)
                } else {
                    None
                }
            })
            .unwrap_or(0);

        let throughput_assignments_per_minute = if interval_ms > 0 {
            (assignment_set.assignments.len() as f64) * 60_000.0 / interval_ms as f64
        } else {
            assignment_set.assignments.len() as f64
        };
        let unblock_velocity_per_minute = if interval_ms > 0 {
            (unblocked_transitions as f64) * 60_000.0 / interval_ms as f64
        } else {
            0.0
        };

        let total_considered = assignment_set.assignments.len() + assignment_set.rejected.len();
        let conflict_rate = if total_considered > 0 {
            conflict_rejections as f64 / total_considered as f64
        } else {
            0.0
        };
        let policy_deny_rate = if assignment_set.rejected.is_empty() {
            0.0
        } else {
            policy_denials as f64 / assignment_set.rejected.len() as f64
        };
        let planner_churn_rate = if planner_churn_denominator > 0 {
            planner_churn_events as f64 / planner_churn_denominator as f64
        } else {
            0.0
        };

        let sample = MissionCycleMetricsSample {
            cycle_id,
            timestamp_ms: current_ms,
            evaluation_latency_ms,
            assignments: assignment_set.assignments.len(),
            rejections: assignment_set.rejected.len(),
            conflict_rejections,
            policy_denials,
            unblocked_transitions,
            planner_churn_events,
            throughput_assignments_per_minute,
            unblock_velocity_per_minute,
            conflict_rate,
            planner_churn_rate,
            policy_deny_rate,
            assignments_by_agent: assignments_by_agent.clone(),
            workspace_label: self.config.metrics.labels.workspace.clone(),
            track_label: self.config.metrics.labels.track.clone(),
        };

        self.state.metrics_totals.cycles = self.state.metrics_totals.cycles.saturating_add(1);
        self.state.metrics_totals.assignments = self
            .state
            .metrics_totals
            .assignments
            .saturating_add(assignment_set.assignments.len() as u64);
        self.state.metrics_totals.rejections = self
            .state
            .metrics_totals
            .rejections
            .saturating_add(assignment_set.rejected.len() as u64);
        self.state.metrics_totals.conflict_rejections = self
            .state
            .metrics_totals
            .conflict_rejections
            .saturating_add(conflict_rejections as u64);
        self.state.metrics_totals.policy_denials = self
            .state
            .metrics_totals
            .policy_denials
            .saturating_add(policy_denials as u64);
        self.state.metrics_totals.unblocked_transitions = self
            .state
            .metrics_totals
            .unblocked_transitions
            .saturating_add(unblocked_transitions as u64);
        self.state.metrics_totals.planner_churn_events = self
            .state
            .metrics_totals
            .planner_churn_events
            .saturating_add(planner_churn_events as u64);

        for (agent_id, assignment_count) in assignments_by_agent {
            *self
                .state
                .metrics_totals
                .assignments_by_agent
                .entry(agent_id)
                .or_insert(0) += assignment_count;
        }

        let max_samples = self.config.metrics.max_samples.max(1);
        if self.state.metrics_history.len() >= max_samples {
            self.state.metrics_history.remove(0);
        }
        self.state.metrics_history.push(sample);
        self.state
            .previous_ready_ids
            .clone_from(&readiness.ready_ids);
        self.state.previous_assignment_by_bead = current_assignment_by_bead;
    }

    // ── Conflict detection (ft-1i2ge.4.5) ──────────────────────────────────

    /// Detect assignment conflicts across file reservations and bead claims.
    ///
    /// This should be called after `evaluate()` with the current set of known
    /// reservations and active claims. Resource reservations default to empty;
    /// use `detect_conflicts_with_resources` to include them explicitly.
    pub fn detect_conflicts(
        &mut self,
        assignment_set: &AssignmentSet,
        known_reservations: &[KnownReservation],
        active_claims: &[ActiveBeadClaim],
        current_ms: i64,
        issues: &[BeadIssueDetail],
    ) -> ConflictDetectionReport {
        self.detect_conflicts_with_resources(
            assignment_set,
            known_reservations,
            &[],
            active_claims,
            current_ms,
            issues,
        )
    }

    /// Detect assignment conflicts across file reservations, resource
    /// reservations, and bead claims.
    pub fn detect_conflicts_with_resources(
        &mut self,
        assignment_set: &AssignmentSet,
        known_reservations: &[KnownReservation],
        known_resource_reservations: &[KnownResourceReservation],
        active_claims: &[ActiveBeadClaim],
        current_ms: i64,
        issues: &[BeadIssueDetail],
    ) -> ConflictDetectionReport {
        let config = &self.config.conflict_detection;
        if !config.enabled {
            return ConflictDetectionReport {
                cycle_id: self.state.cycle_count,
                detected_at_ms: current_ms,
                conflicts: Vec::new(),
                messages: Vec::new(),
                auto_resolved_count: 0,
                pending_resolution_count: 0,
            };
        }

        let mut conflicts = Vec::new();
        let max = config.max_conflicts_per_cycle;

        // Phase 1: Detect file reservation overlaps between assignments and
        // existing reservations.
        self.detect_reservation_overlaps(
            assignment_set,
            known_reservations,
            current_ms,
            &mut conflicts,
            max,
            issues,
        );

        // Phase 2: Detect resource reservation overlaps between assignments
        // and existing reservations.
        if conflicts.len() < max {
            self.detect_resource_reservation_overlaps(
                assignment_set,
                known_resource_reservations,
                current_ms,
                &mut conflicts,
                max,
                issues,
            );
        }

        // Phase 3: Detect concurrent bead claims (same bead assigned to
        // multiple agents in this cycle).
        if conflicts.len() < max {
            self.detect_concurrent_bead_claims(
                assignment_set,
                current_ms,
                &mut conflicts,
                max,
                issues,
            );
        }

        // Phase 4: Detect collisions with active bead claims from previous
        // cycles / external state.
        if conflicts.len() < max {
            self.detect_active_claim_collisions(
                assignment_set,
                active_claims,
                current_ms,
                &mut conflicts,
                max,
                issues,
            );
        }

        // Generate deconfliction messages.
        let messages = if config.generate_messages {
            conflicts
                .iter()
                .flat_map(generate_conflict_messages)
                .collect()
        } else {
            Vec::new()
        };

        let auto_resolved_count = conflicts
            .iter()
            .filter(|c| matches!(c.resolution, ConflictResolution::AutoResolved { .. }))
            .count();
        let pending_resolution_count = conflicts
            .iter()
            .filter(|c| {
                matches!(
                    c.resolution,
                    ConflictResolution::PendingManualResolution
                        | ConflictResolution::Deferred { .. }
                )
            })
            .count();

        // Update state totals.
        self.state.total_conflicts_detected += conflicts.len() as u64;
        self.state.total_conflicts_auto_resolved += auto_resolved_count as u64;

        // Append to conflict history (bounded).
        let history_max = self.config.conflict_detection.max_conflicts_per_cycle * 4;
        for conflict in &conflicts {
            if self.state.conflict_history.len() >= history_max {
                self.state.conflict_history.remove(0);
            }
            self.state.conflict_history.push(conflict.clone());
        }

        ConflictDetectionReport {
            cycle_id: self.state.cycle_count,
            detected_at_ms: current_ms,
            conflicts,
            messages,
            auto_resolved_count,
            pending_resolution_count,
        }
    }

    fn detect_resource_reservation_overlaps(
        &self,
        assignment_set: &AssignmentSet,
        known_resource_reservations: &[KnownResourceReservation],
        current_ms: i64,
        conflicts: &mut Vec<AssignmentConflict>,
        max: usize,
        issues: &[BeadIssueDetail],
    ) {
        for assignment in &assignment_set.assignments {
            if conflicts.len() >= max {
                break;
            }

            let assignment_resources = dedup_owned_strings(
                known_resource_reservations
                    .iter()
                    .filter(|r| {
                        r.bead_id.as_deref() == Some(assignment.bead_id.as_str())
                            && r.holder == assignment.agent_id
                    })
                    .flat_map(|r| r.resources.iter().cloned()),
            );

            if assignment_resources.is_empty() {
                continue;
            }

            let mut buckets = Vec::new();
            for reservation in known_resource_reservations {
                if !reservation.exclusive {
                    continue;
                }
                if reservation.holder == assignment.agent_id {
                    continue;
                }
                if let Some(exp) = reservation.expires_at_ms {
                    if exp <= current_ms {
                        continue;
                    }
                }

                let overlapping = dedup_owned_strings(
                    assignment_resources
                        .iter()
                        .filter(|resource| {
                            reservation
                                .resources
                                .iter()
                                .any(|reserved| resource_scopes_overlap(resource, reserved))
                        })
                        .cloned(),
                );

                if !overlapping.is_empty() {
                    merge_conflict_bucket(
                        &mut buckets,
                        &reservation.holder,
                        reservation.bead_id.as_deref(),
                        overlapping,
                    );
                }
            }

            for bucket in buckets {
                if conflicts.len() >= max {
                    break;
                }

                let resolution = resolve_conflict(
                    self.config.conflict_detection.strategy,
                    &assignment.agent_id,
                    &bucket.holder,
                    assignment.score,
                    0.0,
                    &assignment.bead_id,
                    bucket.bead_id.as_deref().unwrap_or("unknown"),
                    issues,
                );

                conflicts.push(AssignmentConflict {
                    conflict_id: make_reservation_conflict_id(
                        "conflict-resource",
                        self.state.cycle_count,
                        &assignment.bead_id,
                        &bucket.holder,
                        bucket.bead_id.as_deref(),
                    ),
                    conflict_type: ConflictType::ResourceReservationOverlap,
                    involved_agents: vec![assignment.agent_id.clone(), bucket.holder.clone()],
                    involved_beads: build_conflicting_beads(
                        &assignment.bead_id,
                        bucket.bead_id.as_deref(),
                    ),
                    conflicting_paths: bucket.overlaps,
                    detected_at_ms: current_ms,
                    resolution,
                    reason_code: "resource_reservation_overlap".to_string(),
                    error_code: "FTM2004".to_string(),
                });
            }
        }
    }

    /// Latest conflict detection report summary.
    #[must_use]
    pub fn conflict_stats(&self) -> (u64, u64) {
        (
            self.state.total_conflicts_detected,
            self.state.total_conflicts_auto_resolved,
        )
    }

    fn detect_reservation_overlaps(
        &self,
        assignment_set: &AssignmentSet,
        known_reservations: &[KnownReservation],
        current_ms: i64,
        conflicts: &mut Vec<AssignmentConflict>,
        max: usize,
        issues: &[BeadIssueDetail],
    ) {
        // For each assignment, check if any known exclusive reservation
        // from a *different* agent overlaps with the assignment's likely
        // file surface (derived from bead labels/paths).
        for assignment in &assignment_set.assignments {
            if conflicts.len() >= max {
                break;
            }
            // Derive file paths for this assignment from bead labels or
            // reservations that share the same bead_id.
            let assignment_paths = dedup_owned_strings(
                known_reservations
                    .iter()
                    .filter(|r| {
                        r.bead_id.as_deref() == Some(assignment.bead_id.as_str())
                            && r.holder == assignment.agent_id
                    })
                    .flat_map(|r| r.paths.iter().cloned()),
            );

            if assignment_paths.is_empty() {
                continue;
            }

            let mut buckets = Vec::new();
            for reservation in known_reservations {
                // Skip non-exclusive, expired, or same-agent reservations.
                if !reservation.exclusive {
                    continue;
                }
                if reservation.holder == assignment.agent_id {
                    continue;
                }
                if let Some(exp) = reservation.expires_at_ms {
                    if exp <= current_ms {
                        continue;
                    }
                }

                let overlapping = dedup_owned_strings(
                    assignment_paths
                        .iter()
                        .filter(|p| reservation.paths.iter().any(|rp| paths_overlap(p, rp)))
                        .cloned(),
                );

                if !overlapping.is_empty() {
                    merge_conflict_bucket(
                        &mut buckets,
                        &reservation.holder,
                        reservation.bead_id.as_deref(),
                        overlapping,
                    );
                }
            }

            for bucket in buckets {
                if conflicts.len() >= max {
                    break;
                }

                let resolution = resolve_conflict(
                    self.config.conflict_detection.strategy,
                    &assignment.agent_id,
                    &bucket.holder,
                    assignment.score,
                    0.0, // existing reservation holder has no score context
                    &assignment.bead_id,
                    bucket.bead_id.as_deref().unwrap_or("unknown"),
                    issues,
                );

                conflicts.push(AssignmentConflict {
                    conflict_id: make_reservation_conflict_id(
                        "conflict-res",
                        self.state.cycle_count,
                        &assignment.bead_id,
                        &bucket.holder,
                        bucket.bead_id.as_deref(),
                    ),
                    conflict_type: ConflictType::FileReservationOverlap,
                    involved_agents: vec![assignment.agent_id.clone(), bucket.holder.clone()],
                    involved_beads: build_conflicting_beads(
                        &assignment.bead_id,
                        bucket.bead_id.as_deref(),
                    ),
                    conflicting_paths: bucket.overlaps,
                    detected_at_ms: current_ms,
                    resolution,
                    reason_code: "reservation_overlap".to_string(),
                    error_code: "FTM2001".to_string(),
                });
            }
        }
    }

    fn detect_concurrent_bead_claims(
        &self,
        assignment_set: &AssignmentSet,
        current_ms: i64,
        conflicts: &mut Vec<AssignmentConflict>,
        max: usize,
        issues: &[BeadIssueDetail],
    ) {
        // Group assignments by bead_id.
        let mut by_bead: HashMap<&str, Vec<&Assignment>> = HashMap::new();
        for assignment in &assignment_set.assignments {
            by_bead
                .entry(assignment.bead_id.as_str())
                .or_default()
                .push(assignment);
        }

        for (bead_id, bead_agents) in &by_bead {
            if bead_agents.len() <= 1 || conflicts.len() >= max {
                continue;
            }
            // Multiple agents assigned to the same bead — conflict.

            // Auto-resolve: highest score wins.
            let winner = bead_agents
                .iter()
                .max_by(|a, b| {
                    a.score
                        .partial_cmp(&b.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap();
            let losers: Vec<&str> = bead_agents
                .iter()
                .filter(|a| a.agent_id != winner.agent_id)
                .map(|a| a.agent_id.as_str())
                .collect();

            for loser in &losers {
                if conflicts.len() >= max {
                    break;
                }
                let loser_score = bead_agents
                    .iter()
                    .find(|a| a.agent_id == *loser)
                    .map(|a| a.score)
                    .unwrap_or(0.0);

                let resolution = resolve_conflict(
                    self.config.conflict_detection.strategy,
                    &winner.agent_id,
                    loser,
                    winner.score,
                    loser_score,
                    bead_id,
                    bead_id,
                    issues,
                );

                let conflict_id = format!(
                    "conflict-bead-{}-{}-{}",
                    self.state.cycle_count, bead_id, loser
                );
                conflicts.push(AssignmentConflict {
                    conflict_id,
                    conflict_type: ConflictType::ConcurrentBeadClaim,
                    involved_agents: vec![winner.agent_id.clone(), loser.to_string()],
                    involved_beads: vec![bead_id.to_string()],
                    conflicting_paths: Vec::new(),
                    detected_at_ms: current_ms,
                    resolution,
                    reason_code: "concurrent_bead_claim".to_string(),
                    error_code: "FTM2002".to_string(),
                });
            }
        }
    }

    fn detect_active_claim_collisions(
        &self,
        assignment_set: &AssignmentSet,
        active_claims: &[ActiveBeadClaim],
        current_ms: i64,
        conflicts: &mut Vec<AssignmentConflict>,
        max: usize,
        issues: &[BeadIssueDetail],
    ) {
        for assignment in &assignment_set.assignments {
            if conflicts.len() >= max {
                break;
            }
            // Check if the bead is already claimed by a different agent.
            if let Some(existing) = active_claims
                .iter()
                .find(|c| c.bead_id == assignment.bead_id && c.agent_id != assignment.agent_id)
            {
                let resolution = resolve_conflict(
                    self.config.conflict_detection.strategy,
                    &assignment.agent_id,
                    &existing.agent_id,
                    assignment.score,
                    0.0,
                    &assignment.bead_id,
                    &existing.bead_id,
                    issues,
                );

                let conflict_id = format!(
                    "conflict-active-{}-{}-{}",
                    self.state.cycle_count, assignment.bead_id, existing.agent_id
                );
                conflicts.push(AssignmentConflict {
                    conflict_id,
                    conflict_type: ConflictType::ActiveClaimCollision,
                    involved_agents: vec![assignment.agent_id.clone(), existing.agent_id.clone()],
                    involved_beads: vec![assignment.bead_id.clone()],
                    conflicting_paths: Vec::new(),
                    detected_at_ms: current_ms,
                    resolution,
                    reason_code: "active_claim_collision".to_string(),
                    error_code: "FTM2003".to_string(),
                });
            }
        }
    }
}

// ── Free functions for conflict resolution ──────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReservationConflictBucket {
    holder: String,
    bead_id: Option<String>,
    overlaps: Vec<String>,
}

fn dedup_owned_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut unique = Vec::new();
    append_unique_strings(&mut unique, values);
    unique
}

fn append_unique_strings(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn merge_conflict_bucket(
    buckets: &mut Vec<ReservationConflictBucket>,
    holder: &str,
    bead_id: Option<&str>,
    overlaps: Vec<String>,
) {
    if let Some(existing) = buckets
        .iter_mut()
        .find(|bucket| bucket.holder == holder && bucket.bead_id.as_deref() == bead_id)
    {
        append_unique_strings(&mut existing.overlaps, overlaps);
        return;
    }

    buckets.push(ReservationConflictBucket {
        holder: holder.to_string(),
        bead_id: bead_id.map(str::to_string),
        overlaps: dedup_owned_strings(overlaps),
    });
}

fn build_conflicting_beads(primary_bead_id: &str, peer_bead_id: Option<&str>) -> Vec<String> {
    let mut beads = vec![primary_bead_id.to_string()];
    if let Some(peer_bead_id) = peer_bead_id {
        if !beads.iter().any(|bead| bead == peer_bead_id) {
            beads.push(peer_bead_id.to_string());
        }
    }
    beads
}

fn make_reservation_conflict_id(
    prefix: &str,
    cycle_id: u64,
    assignment_bead_id: &str,
    holder: &str,
    peer_bead_id: Option<&str>,
) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        prefix,
        cycle_id,
        assignment_bead_id,
        holder,
        peer_bead_id.unwrap_or("unknown")
    )
}

/// Check if two file paths overlap (bidirectional wildcard matching).
fn paths_overlap(a: &str, b: &str) -> bool {
    // Exact match.
    if a == b {
        return true;
    }
    // One is a prefix of the other (directory containment).
    let a_norm = a.trim_end_matches('/');
    let b_norm = b.trim_end_matches('/');
    if a_norm.starts_with(b_norm) || b_norm.starts_with(a_norm) {
        // Check boundary: must be at a `/` or end.
        let (shorter, longer) = if a_norm.len() <= b_norm.len() {
            (a_norm, b_norm)
        } else {
            (b_norm, a_norm)
        };
        if longer.len() == shorter.len() {
            return true;
        }
        let next_char = longer.as_bytes().get(shorter.len());
        if next_char == Some(&b'/') {
            return true;
        }
    }
    // Wildcard matching (bidirectional).
    wildcard_match(a, b) || wildcard_match(b, a)
}

/// Check if two logical resource scopes overlap.
fn resource_scopes_overlap(a: &str, b: &str) -> bool {
    a == b || wildcard_match(a, b) || wildcard_match(b, a)
}

/// Simple wildcard path matching: `*` matches any sequence, `?` matches one char.
fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let c: Vec<char> = candidate.chars().collect();
    let (pn, cn) = (p.len(), c.len());

    // DP: dp[i][j] = pattern[0..i] matches candidate[0..j]
    let mut dp = vec![vec![false; cn + 1]; pn + 1];
    dp[0][0] = true;

    // Leading `*` in pattern can match empty.
    for i in 1..=pn {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        } else {
            break;
        }
    }

    for i in 1..=pn {
        for j in 1..=cn {
            if p[i - 1] == '*' {
                // `*` matches zero chars (dp[i-1][j]) or one more char (dp[i][j-1]).
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[i - 1] == '?' || p[i - 1] == c[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }

    dp[pn][cn]
}

/// Resolve a conflict using the configured strategy.
fn resolve_conflict(
    strategy: DeconflictionStrategy,
    agent_a: &str,
    agent_b: &str,
    score_a: f64,
    score_b: f64,
    bead_a: &str,
    bead_b: &str,
    issues: &[BeadIssueDetail],
) -> ConflictResolution {
    match strategy {
        DeconflictionStrategy::PriorityWins => {
            // Lower priority number = higher priority.
            let pri_a = issues
                .iter()
                .find(|i| i.id == bead_a)
                .map(|i| i.priority)
                .unwrap_or(u8::MAX);
            let pri_b = issues
                .iter()
                .find(|i| i.id == bead_b)
                .map(|i| i.priority)
                .unwrap_or(u8::MAX);

            if pri_a < pri_b || (pri_a == pri_b && score_a >= score_b) {
                ConflictResolution::AutoResolved {
                    winner_agent: agent_a.to_string(),
                    loser_agent: agent_b.to_string(),
                    strategy,
                }
            } else {
                ConflictResolution::AutoResolved {
                    winner_agent: agent_b.to_string(),
                    loser_agent: agent_a.to_string(),
                    strategy,
                }
            }
        }
        DeconflictionStrategy::FirstClaimWins => {
            // Agent B is the existing holder — they win.
            ConflictResolution::AutoResolved {
                winner_agent: agent_b.to_string(),
                loser_agent: agent_a.to_string(),
                strategy,
            }
        }
        DeconflictionStrategy::ManualResolution => ConflictResolution::PendingManualResolution,
    }
}

/// Generate deconfliction messages for a conflict.
fn generate_conflict_messages(conflict: &AssignmentConflict) -> Vec<DeconflictionMessage> {
    let mut messages = Vec::new();

    let conflict_desc = match conflict.conflict_type {
        ConflictType::FileReservationOverlap => {
            format!(
                "File reservation overlap on: {}",
                conflict.conflicting_paths.join(", ")
            )
        }
        ConflictType::ResourceReservationOverlap => {
            format!(
                "Resource reservation overlap on: {}",
                conflict.conflicting_paths.join(", ")
            )
        }
        ConflictType::ConcurrentBeadClaim => {
            format!(
                "Concurrent claim on bead(s): {}",
                conflict.involved_beads.join(", ")
            )
        }
        ConflictType::ActiveClaimCollision => {
            format!(
                "Collision with active claim on bead(s): {}",
                conflict.involved_beads.join(", ")
            )
        }
    };

    let resolution_desc = match &conflict.resolution {
        ConflictResolution::AutoResolved {
            winner_agent,
            loser_agent,
            strategy,
        } => {
            format!(
                "Auto-resolved ({:?}): **{}** retains assignment, **{}** should yield.",
                strategy, winner_agent, loser_agent
            )
        }
        ConflictResolution::Deferred { retry_after_ms } => {
            format!(
                "Deferred: both assignments held. Retry after {}ms.",
                retry_after_ms
            )
        }
        ConflictResolution::PendingManualResolution => {
            "Pending manual resolution by operator.".to_string()
        }
    };

    let thread_id = conflict
        .involved_beads
        .first()
        .cloned()
        .unwrap_or_else(|| conflict.conflict_id.clone());

    // Send to all involved agents.
    for agent in &conflict.involved_agents {
        let (handoff_role, handoff_action, handoff_continuity) =
            handoff_contract_for_recipient(conflict, agent, &thread_id);
        let body = format!(
            "**Conflict detected** ({})\n\n{}\n\nBeads: {}\nAgents: {}\n\n**Resolution:** {}\n\n**Handoff contract**\n- role: `{}`\n- action: `{}`\n- thread_id: `{}`\n- continuity: {}\n\nReason: `{}` | Error: `{}`",
            conflict.conflict_id,
            conflict_desc,
            conflict.involved_beads.join(", "),
            conflict.involved_agents.join(", "),
            resolution_desc,
            handoff_role,
            handoff_action,
            thread_id,
            handoff_continuity,
            conflict.reason_code,
            conflict.error_code,
        );
        messages.push(DeconflictionMessage {
            recipient: agent.clone(),
            subject: format!(
                "[conflict] {} on {}",
                conflict.reason_code,
                conflict.involved_beads.join(", ")
            ),
            body,
            thread_id: thread_id.clone(),
            importance: match conflict.conflict_type {
                ConflictType::FileReservationOverlap => "high".to_string(),
                ConflictType::ResourceReservationOverlap => "high".to_string(),
                ConflictType::ConcurrentBeadClaim => "high".to_string(),
                ConflictType::ActiveClaimCollision => "normal".to_string(),
            },
            conflict_id: conflict.conflict_id.clone(),
        });
    }

    messages
}

fn handoff_contract_for_recipient(
    conflict: &AssignmentConflict,
    recipient: &str,
    thread_id: &str,
) -> (String, String, String) {
    match &conflict.resolution {
        ConflictResolution::AutoResolved {
            winner_agent,
            loser_agent,
            ..
        } => {
            if recipient == winner_agent {
                (
                    "winner".to_string(),
                    "retain_assignment".to_string(),
                    format!(
                        "Continue execution and publish status updates in `{}` to preserve continuity.",
                        thread_id
                    ),
                )
            } else if recipient == loser_agent {
                (
                    "loser".to_string(),
                    "yield_assignment".to_string(),
                    format!(
                        "Yield immediately, stop conflicting edits, and acknowledge handoff in `{}`.",
                        thread_id
                    ),
                )
            } else {
                (
                    "participant".to_string(),
                    "observe_resolution".to_string(),
                    format!(
                        "Track follow-up in `{}` and do not mutate overlapping scope until resolved.",
                        thread_id
                    ),
                )
            }
        }
        ConflictResolution::Deferred { retry_after_ms } => (
            "participant".to_string(),
            "hold_and_retry".to_string(),
            format!(
                "Hold current state and retry deconfliction after {}ms in `{}`.",
                retry_after_ms, thread_id
            ),
        ),
        ConflictResolution::PendingManualResolution => (
            "participant".to_string(),
            "await_manual_resolution".to_string(),
            format!(
                "Pause conflicting work and wait for operator decision in `{}`.",
                thread_id
            ),
        ),
    }
}

// ── Operator override controls (ft-1i2ge.5.6) ───────────────────────────────

/// Kind of operator override applied to the mission planner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OperatorOverrideKind {
    /// Pin a bead to a specific agent, bypassing normal assignment.
    Pin {
        bead_id: String,
        target_agent: String,
    },
    /// Exclude a bead from all future assignments until cleared.
    Exclude { bead_id: String },
    /// Exclude an agent from receiving any assignments until cleared.
    ExcludeAgent { agent_id: String },
    /// Manual priority boost or reduction for a bead.
    Reprioritize {
        bead_id: String,
        /// Additive score adjustment (positive = boost, negative = penalize).
        score_delta: i32,
    },
}

/// A single operator override with activation metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorOverride {
    /// Unique identifier for this override.
    pub override_id: String,
    /// The override action.
    pub kind: OperatorOverrideKind,
    /// Who activated this override (operator name or system).
    pub activated_by: String,
    /// Structured reason code.
    pub reason_code: String,
    /// Human-readable rationale.
    pub rationale: String,
    /// Activation timestamp (ms since epoch).
    pub activated_at_ms: i64,
    /// Optional TTL expiry (ms since epoch). None = permanent until cleared.
    pub expires_at_ms: Option<i64>,
    /// Correlation ID for audit trail.
    pub correlation_id: Option<String>,
}

impl OperatorOverride {
    /// Check whether this override has expired at the given timestamp.
    #[must_use]
    pub fn is_expired(&self, current_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|exp| current_ms >= exp)
    }

    /// The bead ID targeted by this override, if any.
    #[must_use]
    pub fn target_bead_id(&self) -> Option<&str> {
        match &self.kind {
            OperatorOverrideKind::Pin { bead_id, .. }
            | OperatorOverrideKind::Exclude { bead_id }
            | OperatorOverrideKind::Reprioritize { bead_id, .. } => Some(bead_id.as_str()),
            OperatorOverrideKind::ExcludeAgent { .. } => None,
        }
    }
}

/// Aggregate operator override state tracked in the mission loop.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OperatorOverrideState {
    /// Currently active overrides.
    #[serde(default)]
    pub active: Vec<OperatorOverride>,
    /// Historical overrides (bounded, most recent first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<OperatorOverride>,
}

impl OperatorOverrideState {
    const MAX_HISTORY: usize = 100;

    /// Add an override, making it immediately active.
    pub fn activate(&mut self, ovr: OperatorOverride) {
        self.active.push(ovr);
    }

    /// Clear (deactivate) an override by ID, moving it to history.
    /// Returns `true` if found and cleared.
    pub fn clear(&mut self, override_id: &str, cleared_at_ms: i64) -> bool {
        if let Some(pos) = self
            .active
            .iter()
            .position(|o| o.override_id == override_id)
        {
            let mut cleared = self.active.remove(pos);
            // Mark expiry as cleared time for audit clarity.
            cleared.expires_at_ms = Some(cleared_at_ms);
            self.history.insert(0, cleared);
            if self.history.len() > Self::MAX_HISTORY {
                self.history.truncate(Self::MAX_HISTORY);
            }
            true
        } else {
            false
        }
    }

    /// Evict all expired overrides, moving them to history.
    pub fn evict_expired(&mut self, current_ms: i64) {
        let (expired, remaining): (Vec<_>, Vec<_>) = self
            .active
            .drain(..)
            .partition(|o| o.is_expired(current_ms));
        self.active = remaining;
        for e in expired {
            self.history.insert(0, e);
        }
        if self.history.len() > Self::MAX_HISTORY {
            self.history.truncate(Self::MAX_HISTORY);
        }
    }

    /// Get all active pin overrides.
    #[must_use]
    pub fn active_pins(&self) -> Vec<(&str, &str)> {
        self.active
            .iter()
            .filter_map(|o| match &o.kind {
                OperatorOverrideKind::Pin {
                    bead_id,
                    target_agent,
                } => Some((bead_id.as_str(), target_agent.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Get all active bead exclusions.
    #[must_use]
    pub fn excluded_bead_ids(&self) -> Vec<&str> {
        self.active
            .iter()
            .filter_map(|o| match &o.kind {
                OperatorOverrideKind::Exclude { bead_id } => Some(bead_id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Get all active agent exclusions.
    #[must_use]
    pub fn excluded_agent_ids(&self) -> Vec<&str> {
        self.active
            .iter()
            .filter_map(|o| match &o.kind {
                OperatorOverrideKind::ExcludeAgent { agent_id } => Some(agent_id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Get active reprioritization deltas by bead ID.
    #[must_use]
    pub fn reprioritize_deltas(&self) -> HashMap<&str, i32> {
        let mut deltas: HashMap<&str, i32> = HashMap::new();
        for ovr in &self.active {
            if let OperatorOverrideKind::Reprioritize {
                bead_id,
                score_delta,
            } = &ovr.kind
            {
                *deltas.entry(bead_id.as_str()).or_default() += score_delta;
            }
        }
        deltas
    }
}

/// Result of applying operator overrides to the evaluation pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OverrideApplicationSummary {
    /// Beads excluded by operator override.
    pub excluded_beads: Vec<String>,
    /// Agents excluded by operator override.
    pub excluded_agents: Vec<String>,
    /// Beads pinned to specific agents.
    pub pinned_assignments: Vec<PinnedAssignmentRecord>,
    /// Beads whose scores were adjusted.
    pub reprioritized_beads: Vec<ReprioritizedBeadRecord>,
    /// Overrides that were evicted due to TTL expiry this cycle.
    pub expired_overrides: usize,
}

/// Record of a pinned assignment produced by an operator override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedAssignmentRecord {
    pub bead_id: String,
    pub agent_id: String,
    pub override_id: String,
}

/// Record of a reprioritized bead's score adjustment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReprioritizedBeadRecord {
    pub bead_id: String,
    pub original_score: f64,
    pub adjusted_score: f64,
    pub delta: i32,
}

impl MissionLoop {
    /// Apply an operator override, making it immediately active.
    /// Returns an error string if the override is invalid (e.g., duplicate ID).
    pub fn apply_override(&mut self, ovr: OperatorOverride) -> Result<(), String> {
        // Validate: no duplicate override IDs.
        if self
            .state
            .override_state
            .active
            .iter()
            .any(|existing| existing.override_id == ovr.override_id)
        {
            return Err(format!("override ID '{}' already active", ovr.override_id));
        }
        self.state.override_state.activate(ovr);
        Ok(())
    }

    /// Clear an operator override by its ID.
    /// Returns `true` if found and cleared.
    pub fn clear_override(&mut self, override_id: &str, cleared_at_ms: i64) -> bool {
        self.state.override_state.clear(override_id, cleared_at_ms)
    }

    /// List currently active overrides.
    #[must_use]
    pub fn active_overrides(&self) -> &[OperatorOverride] {
        &self.state.override_state.active
    }

    /// Apply operator overrides to the solver config and scored candidates,
    /// producing pin assignments and filtering exclusions. Called during evaluate().
    fn apply_operator_overrides(
        &mut self,
        current_ms: i64,
        scored: &mut ScorerReport,
        agents: &[MissionAgentCapabilityProfile],
        solver_config: &mut SolverConfig,
    ) -> OverrideApplicationSummary {
        // Evict expired overrides first.
        let before_count = self.state.override_state.active.len();
        self.state.override_state.evict_expired(current_ms);
        let expired_count = before_count - self.state.override_state.active.len();

        let mut summary = OverrideApplicationSummary {
            expired_overrides: expired_count,
            ..Default::default()
        };

        // 1. Exclude beads via safety gates.
        let excluded_beads = self.state.override_state.excluded_bead_ids();
        if !excluded_beads.is_empty() {
            let gate = SafetyGate {
                name: "operator.override.exclude_bead".to_string(),
                denied_bead_ids: excluded_beads.iter().map(|s| (*s).to_string()).collect(),
            };
            solver_config.safety_gates.push(gate);
            summary.excluded_beads = excluded_beads.iter().map(|s| (*s).to_string()).collect();
        }

        // 2. Exclude agents by zeroing their capacity in the agents list.
        // We can't modify agents directly, so we add exclusion via safety-gate
        // denial of all beads for excluded agents. Instead, we track excluded
        // agents and filter them downstream in the solver by injecting conflict
        // information. The simplest approach: remove scored candidates that would
        // go to excluded agents (agent selection happens in solver, not here).
        // We'll record excluded agents and the solver respects them.
        let excluded_agents = self.state.override_state.excluded_agent_ids();
        summary.excluded_agents = excluded_agents.iter().map(|s| (*s).to_string()).collect();

        // 3. Apply reprioritization score deltas.
        let deltas = self.state.override_state.reprioritize_deltas();
        for candidate in &mut scored.scored {
            if let Some(&delta) = deltas.get(candidate.bead_id.as_str()) {
                let original = candidate.final_score;
                // Apply delta as fraction: +100 = +1.0, -50 = -0.5
                let adjustment = f64::from(delta) / 100.0;
                candidate.final_score = (candidate.final_score + adjustment).clamp(0.0, 10.0);
                summary.reprioritized_beads.push(ReprioritizedBeadRecord {
                    bead_id: candidate.bead_id.clone(),
                    original_score: original,
                    adjusted_score: candidate.final_score,
                    delta,
                });
            }
        }
        // Re-sort by adjusted score (descending) to preserve deterministic ordering.
        scored.scored.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // 4. Generate pin assignments (forced assignments bypass solver).
        let pins = self.state.override_state.active_pins();
        for (bead_id, target_agent) in &pins {
            // Verify the bead is in the scored set (it's a real candidate).
            let in_scored = scored.scored.iter().any(|c| c.bead_id == *bead_id);
            // Verify the target agent exists.
            let agent_exists = agents.iter().any(|a| a.agent_id == *target_agent);
            if in_scored && agent_exists {
                // Find the override_id for audit trail.
                let override_id = self
                    .state
                    .override_state
                    .active
                    .iter()
                    .find(|o| matches!(&o.kind, OperatorOverrideKind::Pin { bead_id: b, target_agent: t } if b == bead_id && t == target_agent))
                    .map(|o| o.override_id.clone())
                    .unwrap_or_default();
                summary.pinned_assignments.push(PinnedAssignmentRecord {
                    bead_id: (*bead_id).to_string(),
                    agent_id: (*target_agent).to_string(),
                    override_id,
                });
                // Remove from scored set so solver doesn't double-assign.
                scored.scored.retain(|c| c.bead_id != *bead_id);
            }
        }

        summary
    }
}

// ── Operator report views (ft-1i2ge.5.5) ────────────────────────────────────

use crate::mission_events::{MissionEventLog, MissionEventLogSummary};
use crate::planner_features::ExplainabilityReport;

/// Top-level operator report synthesizing mission state into a human- and
/// machine-consumable overview. Serializable for robot-mode JSON output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorStatusReport {
    /// Mission lifecycle phase and timing.
    pub status: OperatorStatusSection,
    /// Per-agent assignment summary table.
    pub assignment_table: Vec<AgentAssignmentRow>,
    /// Health indicators derived from recent metrics.
    pub health: OperatorHealthSection,
    /// Conflict history summary.
    pub conflicts: OperatorConflictSection,
    /// Event log phase breakdown.
    pub event_summary: OperatorEventSection,
    /// Decision explanations for the latest cycle (if available).
    pub latest_explanations: Vec<OperatorDecisionSummary>,
}

/// Status overview section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorStatusSection {
    pub cycle_count: u64,
    pub last_evaluation_ms: Option<i64>,
    pub total_assignments: u64,
    pub total_rejections: u64,
    pub pending_trigger_count: usize,
    /// Textual phase label (e.g. "active", "idle", "degraded").
    pub phase_label: String,
}

/// One row in the per-agent assignment table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignmentRow {
    pub agent_id: String,
    pub total_assignments: u64,
    /// Number of beads currently assigned (from latest cycle).
    pub active_beads: usize,
    /// Bead IDs currently assigned.
    pub active_bead_ids: Vec<String>,
}

/// Health indicators computed from the metrics window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorHealthSection {
    pub throughput_assignments_per_minute: f64,
    pub unblock_velocity_per_minute: f64,
    pub conflict_rate: f64,
    pub planner_churn_rate: f64,
    pub policy_deny_rate: f64,
    pub avg_evaluation_latency_ms: f64,
    /// One of "healthy", "degraded", "critical".
    pub overall: String,
}

/// Conflict section of the operator report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorConflictSection {
    pub total_detected: u64,
    pub total_auto_resolved: u64,
    pub pending_manual: u64,
    pub recent_conflicts: Vec<OperatorConflictSummary>,
}

/// Compact conflict summary for the operator report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorConflictSummary {
    pub conflict_id: String,
    pub conflict_type: String,
    pub agents: Vec<String>,
    pub beads: Vec<String>,
    pub resolution: String,
    pub reason_code: String,
}

/// Event log phase-breakdown section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorEventSection {
    pub retained_events: usize,
    pub total_emitted: u64,
    pub by_phase: HashMap<String, usize>,
    pub by_kind: HashMap<String, usize>,
}

/// Decision explanation summary for operator view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorDecisionSummary {
    pub bead_id: String,
    pub outcome: String,
    pub summary: String,
    pub top_factors: Vec<OperatorFactorSummary>,
}

/// Factor summary (top contributing dimensions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorFactorSummary {
    pub dimension: String,
    pub value: f64,
    pub polarity: String,
    pub description: String,
}

impl MissionLoop {
    /// Generate a full operator status report from current loop state and an
    /// optional event log. The report is designed for both human CLI display
    /// and robot-mode JSON serialization.
    #[must_use]
    pub fn generate_operator_report(
        &self,
        event_log: Option<&MissionEventLog>,
        explainability: Option<&ExplainabilityReport>,
    ) -> OperatorStatusReport {
        let state = &self.state;

        // ── Status section ──────────────────────────────────────────────
        let phase_label = self.compute_phase_label();
        let status = OperatorStatusSection {
            cycle_count: state.cycle_count,
            last_evaluation_ms: state.last_evaluation_ms,
            total_assignments: state.total_assignments_made,
            total_rejections: state.total_rejections,
            pending_trigger_count: state.pending_triggers.len(),
            phase_label,
        };

        // ── Agent assignment table ──────────────────────────────────────
        let assignment_table = self.build_assignment_table();

        // ── Health section ──────────────────────────────────────────────
        let health = self.compute_health_section();

        // ── Conflict section ────────────────────────────────────────────
        let conflicts = self.build_conflict_section();

        // ── Event log section ───────────────────────────────────────────
        let event_summary = if let Some(log) = event_log {
            let summary = log.summary();
            build_event_section(&summary)
        } else {
            OperatorEventSection {
                retained_events: 0,
                total_emitted: 0,
                by_phase: HashMap::new(),
                by_kind: HashMap::new(),
            }
        };

        // ── Decision explanations ───────────────────────────────────────
        let latest_explanations = if let Some(explain) = explainability {
            explain
                .explanations
                .iter()
                .map(|e| OperatorDecisionSummary {
                    bead_id: e.bead_id.clone(),
                    outcome: format!("{:?}", e.outcome),
                    summary: e.summary.clone(),
                    top_factors: e
                        .factors
                        .iter()
                        .take(5)
                        .map(|f| OperatorFactorSummary {
                            dimension: f.dimension.clone(),
                            value: f.value,
                            polarity: format!("{:?}", f.polarity),
                            description: f.description.clone(),
                        })
                        .collect(),
                })
                .collect()
        } else {
            Vec::new()
        };

        OperatorStatusReport {
            status,
            assignment_table,
            health,
            conflicts,
            event_summary,
            latest_explanations,
        }
    }

    /// Classify current operational phase based on state signals.
    fn compute_phase_label(&self) -> String {
        let state = &self.state;
        if state.cycle_count == 0 {
            return "idle".to_string();
        }
        // Check health signals from latest metrics
        if let Some(latest) = state.metrics_history.last() {
            if latest.conflict_rate > 0.3 || latest.policy_deny_rate > 0.5 {
                return "degraded".to_string();
            }
        }
        if !state.pending_triggers.is_empty() {
            return "pending".to_string();
        }
        "active".to_string()
    }

    /// Build per-agent assignment summary table.
    fn build_assignment_table(&self) -> Vec<AgentAssignmentRow> {
        let state = &self.state;
        let mut rows: Vec<AgentAssignmentRow> = state
            .metrics_totals
            .assignments_by_agent
            .iter()
            .map(|(agent_id, &total)| {
                // Find beads currently assigned to this agent from latest decision
                let (active_count, active_ids) = state
                    .last_decision
                    .as_ref()
                    .map(|dec| {
                        let ids: Vec<String> = dec
                            .assignment_set
                            .assignments
                            .iter()
                            .filter(|a| a.agent_id == *agent_id)
                            .map(|a| a.bead_id.clone())
                            .collect();
                        (ids.len(), ids)
                    })
                    .unwrap_or((0, Vec::new()));
                AgentAssignmentRow {
                    agent_id: agent_id.clone(),
                    total_assignments: total,
                    active_beads: active_count,
                    active_bead_ids: active_ids,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.total_assignments
                .cmp(&a.total_assignments)
                .then_with(|| a.agent_id.cmp(&b.agent_id))
        });
        rows
    }

    /// Compute health indicators from the metrics window.
    fn compute_health_section(&self) -> OperatorHealthSection {
        let state = &self.state;
        let window = &state.metrics_history;

        if window.is_empty() {
            return OperatorHealthSection {
                throughput_assignments_per_minute: 0.0,
                unblock_velocity_per_minute: 0.0,
                conflict_rate: 0.0,
                planner_churn_rate: 0.0,
                policy_deny_rate: 0.0,
                avg_evaluation_latency_ms: 0.0,
                overall: "idle".to_string(),
            };
        }

        let n = window.len() as f64;
        let avg_throughput = window
            .iter()
            .map(|m| m.throughput_assignments_per_minute)
            .sum::<f64>()
            / n;
        let avg_unblock = window
            .iter()
            .map(|m| m.unblock_velocity_per_minute)
            .sum::<f64>()
            / n;
        let avg_conflict = window.iter().map(|m| m.conflict_rate).sum::<f64>() / n;
        let avg_churn = window.iter().map(|m| m.planner_churn_rate).sum::<f64>() / n;
        let avg_deny = window.iter().map(|m| m.policy_deny_rate).sum::<f64>() / n;
        let avg_latency = window
            .iter()
            .map(|m| m.evaluation_latency_ms as f64)
            .sum::<f64>()
            / n;

        let overall = if avg_conflict > 0.3 || avg_deny > 0.5 {
            "critical"
        } else if avg_conflict > 0.1 || avg_churn > 0.3 || avg_deny > 0.2 {
            "degraded"
        } else {
            "healthy"
        };

        OperatorHealthSection {
            throughput_assignments_per_minute: avg_throughput,
            unblock_velocity_per_minute: avg_unblock,
            conflict_rate: avg_conflict,
            planner_churn_rate: avg_churn,
            policy_deny_rate: avg_deny,
            avg_evaluation_latency_ms: avg_latency,
            overall: overall.to_string(),
        }
    }

    /// Build conflict summary section.
    fn build_conflict_section(&self) -> OperatorConflictSection {
        let state = &self.state;
        let pending_manual = state
            .conflict_history
            .iter()
            .filter(|c| c.resolution == ConflictResolution::PendingManualResolution)
            .count() as u64;

        let recent_conflicts: Vec<OperatorConflictSummary> = state
            .conflict_history
            .iter()
            .rev()
            .take(10)
            .map(|c| OperatorConflictSummary {
                conflict_id: c.conflict_id.clone(),
                conflict_type: format!("{:?}", c.conflict_type),
                agents: c.involved_agents.clone(),
                beads: c.involved_beads.clone(),
                resolution: format!("{:?}", c.resolution),
                reason_code: c.reason_code.clone(),
            })
            .collect();

        OperatorConflictSection {
            total_detected: state.total_conflicts_detected,
            total_auto_resolved: state.total_conflicts_auto_resolved,
            pending_manual,
            recent_conflicts,
        }
    }
}

/// Build event section from an event log summary.
fn build_event_section(summary: &MissionEventLogSummary) -> OperatorEventSection {
    OperatorEventSection {
        retained_events: summary.retained_count,
        total_emitted: summary.total_appended,
        by_phase: summary
            .by_phase
            .iter()
            .map(|(phase, &count)| (format!("{phase:?}"), count))
            .collect(),
        by_kind: summary
            .by_kind
            .iter()
            .map(|(kind, &count)| (format!("{kind:?}"), count))
            .collect(),
    }
}

/// Format an `OperatorStatusReport` as human-readable plain text for CLI display.
#[must_use]
pub fn format_operator_report_plain(report: &OperatorStatusReport) -> String {
    let mut out = String::new();

    // ── Status ──
    out.push_str("=== Mission Status ===\n");
    out.push_str(&format!("  Phase:       {}\n", report.status.phase_label));
    out.push_str(&format!("  Cycles:      {}\n", report.status.cycle_count));
    out.push_str(&format!(
        "  Assignments: {}\n",
        report.status.total_assignments
    ));
    out.push_str(&format!(
        "  Rejections:  {}\n",
        report.status.total_rejections
    ));
    if let Some(ts) = report.status.last_evaluation_ms {
        out.push_str(&format!("  Last eval:   {}ms\n", ts));
    }
    if report.status.pending_trigger_count > 0 {
        out.push_str(&format!(
            "  Pending:     {} trigger(s)\n",
            report.status.pending_trigger_count
        ));
    }
    out.push('\n');

    // ── Health ──
    out.push_str("=== Health ===\n");
    out.push_str(&format!("  Overall:         {}\n", report.health.overall));
    out.push_str(&format!(
        "  Throughput:      {:.1} assign/min\n",
        report.health.throughput_assignments_per_minute
    ));
    out.push_str(&format!(
        "  Unblock vel:     {:.1}/min\n",
        report.health.unblock_velocity_per_minute
    ));
    out.push_str(&format!(
        "  Conflict rate:   {:.1}%\n",
        report.health.conflict_rate * 100.0
    ));
    out.push_str(&format!(
        "  Churn rate:      {:.1}%\n",
        report.health.planner_churn_rate * 100.0
    ));
    out.push_str(&format!(
        "  Policy deny:     {:.1}%\n",
        report.health.policy_deny_rate * 100.0
    ));
    out.push_str(&format!(
        "  Avg latency:     {:.0}ms\n",
        report.health.avg_evaluation_latency_ms
    ));
    out.push('\n');

    // ── Assignment Table ──
    if !report.assignment_table.is_empty() {
        out.push_str("=== Agent Assignments ===\n");
        out.push_str("  Agent              Total  Active  Beads\n");
        out.push_str("  ─────              ─────  ──────  ─────\n");
        for row in &report.assignment_table {
            let beads_str = if row.active_bead_ids.is_empty() {
                "—".to_string()
            } else {
                row.active_bead_ids.join(", ")
            };
            out.push_str(&format!(
                "  {:<18} {:>5}  {:>6}  {}\n",
                row.agent_id, row.total_assignments, row.active_beads, beads_str
            ));
        }
        out.push('\n');
    }

    // ── Conflicts ──
    if report.conflicts.total_detected > 0 {
        out.push_str("=== Conflicts ===\n");
        out.push_str(&format!(
            "  Detected: {}  Auto-resolved: {}  Pending manual: {}\n",
            report.conflicts.total_detected,
            report.conflicts.total_auto_resolved,
            report.conflicts.pending_manual
        ));
        for c in &report.conflicts.recent_conflicts {
            out.push_str(&format!(
                "  [{:>12}] {} — agents: {} beads: {} — {}\n",
                c.conflict_type,
                c.conflict_id,
                c.agents.join(","),
                c.beads.join(","),
                c.resolution,
            ));
        }
        out.push('\n');
    }

    // ── Events ──
    if report.event_summary.total_emitted > 0 {
        out.push_str("=== Event Log ===\n");
        out.push_str(&format!(
            "  Retained: {}  Total emitted: {}\n",
            report.event_summary.retained_events, report.event_summary.total_emitted
        ));
        if !report.event_summary.by_phase.is_empty() {
            out.push_str("  By phase: ");
            let mut phases: Vec<_> = report.event_summary.by_phase.iter().collect();
            phases.sort_by_key(|&(_, &v)| std::cmp::Reverse(v));
            for (phase, count) in &phases {
                out.push_str(&format!("{}={} ", phase, count));
            }
            out.push('\n');
        }
        out.push('\n');
    }

    // ── Decision Explanations ──
    if !report.latest_explanations.is_empty() {
        out.push_str("=== Latest Decisions ===\n");
        for dec in &report.latest_explanations {
            out.push_str(&format!(
                "  [{}] {} — {}\n",
                dec.outcome, dec.bead_id, dec.summary
            ));
            for f in &dec.top_factors {
                let polarity_marker = match f.polarity.as_str() {
                    "Positive" => "+",
                    "Negative" => "-",
                    _ => " ",
                };
                out.push_str(&format!(
                    "    {} {}: {:.3} — {}\n",
                    polarity_marker, f.dimension, f.value, f.description
                ));
            }
        }
    }

    out
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
    fn loop_metrics_capture_labels_latency_and_throughput() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            metrics: MissionMetricsConfig {
                enabled: true,
                max_samples: 8,
                labels: MissionMetricsLabels {
                    workspace: "ws-main".to_string(),
                    track: "f1-mission".to_string(),
                },
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];
        let ctx = PlannerExtractionContext::default();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        ml.evaluate(7000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let latest = ml.latest_metrics().expect("metrics sample must exist");
        assert_eq!(latest.workspace_label, "ws-main");
        assert_eq!(latest.track_label, "f1-mission");
        assert_eq!(ml.state().metrics_totals.cycles, 2);
        assert!(latest.throughput_assignments_per_minute >= 0.0);
        assert!(!latest.assignments_by_agent.is_empty());
    }

    #[test]
    fn loop_metrics_track_unblock_velocity_from_state_transitions() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let mut issues = vec![
            sample_detail("blocker", BeadStatus::Open, 0, &[]),
            sample_detail("blocked", BeadStatus::Open, 1, &[("blocker", "blocks")]),
        ];
        let agents = vec![ready_agent("agent-a")];
        let ctx = PlannerExtractionContext::default();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        issues[0].status = BeadStatus::Closed;
        ml.evaluate(7000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let latest = ml.latest_metrics().expect("metrics sample must exist");
        assert_eq!(latest.unblocked_transitions, 1);
        assert!(latest.unblock_velocity_per_minute > 0.0);
    }

    #[test]
    fn loop_metrics_capture_conflict_and_policy_deny_rates() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            solver_config: SolverConfig {
                min_score: 0.0,
                max_assignments: 10,
                safety_gates: vec![crate::planner_features::SafetyGate {
                    name: "deny-c".to_string(),
                    denied_bead_ids: vec!["c".to_string()],
                }],
                conflicts: vec![crate::planner_features::ConflictPair {
                    bead_a: "a".to_string(),
                    bead_b: "b".to_string(),
                }],
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
            sample_detail("c", BeadStatus::Open, 2, &[]),
        ];
        let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];
        let ctx = PlannerExtractionContext::default();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let latest = ml.latest_metrics().expect("metrics sample must exist");
        assert!(latest.conflict_rejections >= 1);
        assert!(latest.policy_denials >= 1);
        assert!(latest.conflict_rate > 0.0);
        assert!(latest.policy_deny_rate > 0.0);
    }

    #[test]
    fn loop_metrics_track_planner_churn_when_assignments_change() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = vec![ready_agent("agent-a")];
        let ctx = PlannerExtractionContext::default();

        let cycle_one_issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        ml.evaluate(
            1000,
            MissionTrigger::ManualTrigger {
                reason: "cycle-one".to_string(),
            },
            &cycle_one_issues,
            &agents,
            &ctx,
        );

        let cycle_two_issues = vec![
            sample_detail("a", BeadStatus::Closed, 0, &[]),
            sample_detail("b", BeadStatus::Open, 0, &[]),
        ];
        ml.evaluate(
            7000,
            MissionTrigger::ManualTrigger {
                reason: "cycle-two".to_string(),
            },
            &cycle_two_issues,
            &agents,
            &ctx,
        );

        let latest = ml.latest_metrics().expect("metrics sample must exist");
        assert!(latest.planner_churn_events > 0);
        assert!(latest.planner_churn_rate > 0.0);
    }

    #[test]
    fn loop_metrics_history_is_bounded_by_configured_sampling_limit() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            metrics: MissionMetricsConfig {
                enabled: true,
                max_samples: 2,
                labels: MissionMetricsLabels::default(),
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("agent-a")];
        let ctx = PlannerExtractionContext::default();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        ml.evaluate(7000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        ml.evaluate(13_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        assert_eq!(ml.state().metrics_history.len(), 2);
        assert_eq!(ml.state().metrics_history[0].cycle_id, 2);
        assert_eq!(ml.state().metrics_history[1].cycle_id, 3);
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
        assert_eq!(back.metrics.max_samples, config.metrics.max_samples);
        assert_eq!(
            back.metrics.labels.workspace,
            config.metrics.labels.workspace
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
            metrics_history: vec![MissionCycleMetricsSample {
                cycle_id: 5,
                timestamp_ms: 1000,
                evaluation_latency_ms: 2,
                assignments: 1,
                rejections: 1,
                conflict_rejections: 1,
                policy_denials: 1,
                unblocked_transitions: 0,
                planner_churn_events: 1,
                throughput_assignments_per_minute: 10.0,
                unblock_velocity_per_minute: 0.0,
                conflict_rate: 0.5,
                planner_churn_rate: 1.0,
                policy_deny_rate: 1.0,
                assignments_by_agent: HashMap::from([("agent-a".to_string(), 1)]),
                workspace_label: "default".to_string(),
                track_label: "mission".to_string(),
            }],
            metrics_totals: MissionMetricsTotals {
                cycles: 5,
                assignments: 10,
                rejections: 3,
                conflict_rejections: 1,
                policy_denials: 1,
                unblocked_transitions: 2,
                planner_churn_events: 4,
                assignments_by_agent: HashMap::from([("agent-a".to_string(), 10)]),
            },
            previous_ready_ids: vec!["bead-a".to_string()],
            previous_assignment_by_bead: HashMap::from([(
                "bead-a".to_string(),
                "agent-a".to_string(),
            )]),
            conflict_history: vec![AssignmentConflict {
                conflict_id: "c1".to_string(),
                conflict_type: ConflictType::ConcurrentBeadClaim,
                involved_agents: vec!["a1".to_string(), "a2".to_string()],
                involved_beads: vec!["bead-x".to_string()],
                conflicting_paths: Vec::new(),
                detected_at_ms: 999,
                resolution: ConflictResolution::AutoResolved {
                    winner_agent: "a1".to_string(),
                    loser_agent: "a2".to_string(),
                    strategy: DeconflictionStrategy::PriorityWins,
                },
                reason_code: "concurrent_bead_claim".to_string(),
                error_code: "FTM2002".to_string(),
            }],
            total_conflicts_detected: 1,
            total_conflicts_auto_resolved: 1,
            override_state: OperatorOverrideState::default(),
            last_override_summary: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionLoopState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_count, 5);
        assert_eq!(back.total_assignments_made, 10);
        assert_eq!(back.retry_streaks.get("bead-a"), Some(&2));
        assert_eq!(back.metrics_history.len(), 1);
        assert_eq!(back.metrics_totals.cycles, 5);
        assert_eq!(
            back.previous_assignment_by_bead
                .get("bead-a")
                .map(String::as_str),
            Some("agent-a")
        );
        assert_eq!(back.conflict_history.len(), 1);
        assert_eq!(back.total_conflicts_detected, 1);
        assert_eq!(back.total_conflicts_auto_resolved, 1);
    }

    // ── Conflict detection tests (ft-1i2ge.4.5) ────────────────────────────

    fn make_assignment(bead_id: &str, agent_id: &str, score: f64) -> Assignment {
        Assignment {
            bead_id: bead_id.to_string(),
            agent_id: agent_id.to_string(),
            score,
            rank: 1,
        }
    }

    fn make_assignment_set(assignments: Vec<Assignment>) -> AssignmentSet {
        AssignmentSet {
            assignments,
            rejected: Vec::new(),
            solver_config: SolverConfig::default(),
        }
    }

    fn make_reservation(holder: &str, paths: &[&str], bead_id: Option<&str>) -> KnownReservation {
        KnownReservation {
            holder: holder.to_string(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            exclusive: true,
            bead_id: bead_id.map(|b| b.to_string()),
            expires_at_ms: Some(999_999),
        }
    }

    fn make_resource_reservation(
        holder: &str,
        resources: &[&str],
        bead_id: Option<&str>,
    ) -> KnownResourceReservation {
        KnownResourceReservation {
            holder: holder.to_string(),
            resources: resources
                .iter()
                .map(|resource| resource.to_string())
                .collect(),
            exclusive: true,
            bead_id: bead_id.map(|b| b.to_string()),
            expires_at_ms: Some(999_999),
        }
    }

    fn make_active_claim(bead_id: &str, agent_id: &str) -> ActiveBeadClaim {
        ActiveBeadClaim {
            bead_id: bead_id.to_string(),
            agent_id: agent_id.to_string(),
            claimed_at_ms: 1000,
        }
    }

    #[test]
    fn conflict_detection_disabled_returns_empty() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                enabled: false,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![make_reservation("agent2", &["src/a.rs"], Some("b"))];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &[]);
        assert!(report.conflicts.is_empty());
        assert!(report.messages.is_empty());
        assert_eq!(report.auto_resolved_count, 0);
    }

    #[test]
    fn conflict_detection_no_overlaps_clean() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/a.rs"], Some("a")),
            make_reservation("agent2", &["src/b.rs"], Some("b")),
        ];
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn conflict_detection_reservation_overlap_detected() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        // agent1's bead "a" wants src/plan.rs, but agent2 already has it reserved.
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/plan.rs"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].conflict_type,
            ConflictType::FileReservationOverlap
        );
        assert_eq!(report.conflicts[0].reason_code, "reservation_overlap");
        assert_eq!(report.conflicts[0].error_code, "FTM2001");
        assert!(
            report.conflicts[0]
                .conflicting_paths
                .contains(&"src/plan.rs".to_string())
        );
        assert!(
            report.conflicts[0]
                .involved_agents
                .contains(&"agent1".to_string())
        );
        assert!(
            report.conflicts[0]
                .involved_agents
                .contains(&"agent2".to_string())
        );
    }

    #[test]
    fn conflict_detection_resource_reservation_overlap_detected() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let resource_reservations = vec![
            make_resource_reservation("agent1", &["pane:alpha"], Some("a")),
            make_resource_reservation("agent2", &["pane:alpha"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = ml.detect_conflicts_with_resources(
            &aset,
            &[],
            &resource_reservations,
            &[],
            5000,
            &issues,
        );
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].conflict_type,
            ConflictType::ResourceReservationOverlap
        );
        assert_eq!(
            report.conflicts[0].reason_code,
            "resource_reservation_overlap"
        );
        assert_eq!(report.conflicts[0].error_code, "FTM2004");
        assert!(
            report.conflicts[0]
                .conflicting_paths
                .contains(&"pane:alpha".to_string())
        );
    }

    #[test]
    fn conflict_detection_resource_reservation_wildcard_overlap_detected() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let resource_reservations = vec![
            make_resource_reservation("agent1", &["pane:alpha"], Some("a")),
            make_resource_reservation("agent2", &["pane:*"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = ml.detect_conflicts_with_resources(
            &aset,
            &[],
            &resource_reservations,
            &[],
            5000,
            &issues,
        );
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].conflict_type,
            ConflictType::ResourceReservationOverlap
        );
    }

    #[test]
    fn conflict_detection_reservation_overlap_same_peer_bead_aggregates_once() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/plan.rs"], Some("b")),
            make_reservation("agent2", &["src/*.rs"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];

        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);

        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.messages.len(), 2);
        assert_eq!(
            report.conflicts[0].involved_beads,
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            report.conflicts[0].conflicting_paths,
            vec!["src/plan.rs".to_string()]
        );
        assert_eq!(
            report
                .messages
                .iter()
                .filter(|message| {
                    message.recipient == "agent1"
                        && message.conflict_id == report.conflicts[0].conflict_id
                })
                .count(),
            1
        );
        assert_eq!(
            report
                .messages
                .iter()
                .filter(|message| {
                    message.recipient == "agent2"
                        && message.conflict_id == report.conflicts[0].conflict_id
                })
                .count(),
            1
        );
    }

    #[test]
    fn conflict_detection_reservation_overlap_distinguishes_peer_beads_in_conflict_id() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/plan.rs"], Some("b")),
            make_reservation("agent2", &["src/plan.rs"], Some("c")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
            sample_detail("c", BeadStatus::Open, 2, &[]),
        ];

        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);

        assert_eq!(report.conflicts.len(), 2);
        assert_ne!(
            report.conflicts[0].conflict_id,
            report.conflicts[1].conflict_id
        );
        assert_eq!(
            report.conflicts[0].involved_beads,
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            report.conflicts[1].involved_beads,
            vec!["a".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn conflict_detection_resource_reservation_same_peer_bead_aggregates_once() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let resource_reservations = vec![
            make_resource_reservation("agent1", &["pane:alpha"], Some("a")),
            make_resource_reservation("agent2", &["pane:alpha"], Some("b")),
            make_resource_reservation("agent2", &["pane:*"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];

        let report = ml.detect_conflicts_with_resources(
            &aset,
            &[],
            &resource_reservations,
            &[],
            5000,
            &issues,
        );

        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.messages.len(), 2);
        assert_eq!(
            report.conflicts[0].involved_beads,
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            report.conflicts[0].conflicting_paths,
            vec!["pane:alpha".to_string()]
        );
    }

    #[test]
    fn conflict_detection_expired_resource_reservation_ignored() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let resource_reservations = vec![
            make_resource_reservation("agent1", &["pane:alpha"], Some("a")),
            KnownResourceReservation {
                holder: "agent2".to_string(),
                resources: vec!["pane:alpha".to_string()],
                exclusive: true,
                bead_id: Some("b".to_string()),
                expires_at_ms: Some(4000),
            },
        ];
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts_with_resources(
            &aset,
            &[],
            &resource_reservations,
            &[],
            5000,
            &issues,
        );
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn conflict_detection_expired_reservation_ignored() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            KnownReservation {
                holder: "agent2".to_string(),
                paths: vec!["src/plan.rs".to_string()],
                exclusive: true,
                bead_id: Some("b".to_string()),
                expires_at_ms: Some(4000), // expired before current_ms=5000
            },
        ];
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn conflict_detection_non_exclusive_reservation_ignored() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            KnownReservation {
                holder: "agent2".to_string(),
                paths: vec!["src/plan.rs".to_string()],
                exclusive: false, // not exclusive
                bead_id: Some("b".to_string()),
                expires_at_ms: Some(999_999),
            },
        ];
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn conflict_detection_same_agent_reservation_no_conflict() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        // Same agent holds both reservations — no conflict.
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent1", &["src/plan.rs"], Some("b")),
        ];
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn conflict_detection_concurrent_bead_claim() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        // Two agents assigned to the same bead in one cycle.
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].conflict_type,
            ConflictType::ConcurrentBeadClaim
        );
        assert_eq!(report.conflicts[0].reason_code, "concurrent_bead_claim");
        assert_eq!(report.conflicts[0].error_code, "FTM2002");
    }

    #[test]
    fn conflict_detection_concurrent_claim_auto_resolves_highest_score() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 0.3),
            make_assignment("bead-x", "agent2", 0.9),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        // Same priority, so higher score wins.
        match &report.conflicts[0].resolution {
            ConflictResolution::AutoResolved {
                winner_agent,
                loser_agent,
                ..
            } => {
                assert_eq!(winner_agent, "agent2");
                assert_eq!(loser_agent, "agent1");
            }
            other => panic!("Expected AutoResolved, got {:?}", other),
        }
        assert_eq!(report.auto_resolved_count, 1);
    }

    #[test]
    fn conflict_detection_active_claim_collision() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("bead-x", "agent1", 1.0)]);
        let active = vec![make_active_claim("bead-x", "agent2")];
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &active, 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].conflict_type,
            ConflictType::ActiveClaimCollision
        );
        assert_eq!(report.conflicts[0].reason_code, "active_claim_collision");
        assert_eq!(report.conflicts[0].error_code, "FTM2003");
    }

    #[test]
    fn conflict_detection_active_claim_same_agent_no_conflict() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("bead-x", "agent1", 1.0)]);
        // Same agent already holds the bead — that's fine.
        let active = vec![make_active_claim("bead-x", "agent1")];
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &active, 5000, &issues);
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn conflict_detection_generates_messages() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        assert!(!report.messages.is_empty());
        // Each conflict sends to all involved agents.
        assert_eq!(report.messages.len(), 2); // 1 conflict × 2 agents
        let recipients: Vec<&str> = report
            .messages
            .iter()
            .map(|m| m.recipient.as_str())
            .collect();
        assert!(recipients.contains(&"agent1"));
        assert!(recipients.contains(&"agent2"));
    }

    #[test]
    fn conflict_detection_messages_disabled() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                generate_messages: false,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.messages.is_empty());
    }

    #[test]
    fn conflict_detection_manual_resolution_strategy() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                strategy: DeconflictionStrategy::ManualResolution,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].resolution,
            ConflictResolution::PendingManualResolution
        );
        assert_eq!(report.pending_resolution_count, 1);
        assert_eq!(report.auto_resolved_count, 0);
    }

    #[test]
    fn conflict_detection_first_claim_wins_strategy() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                strategy: DeconflictionStrategy::FirstClaimWins,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/plan.rs"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        // FirstClaimWins: agent2 (existing holder) wins.
        match &report.conflicts[0].resolution {
            ConflictResolution::AutoResolved {
                winner_agent,
                loser_agent,
                ..
            } => {
                assert_eq!(winner_agent, "agent2");
                assert_eq!(loser_agent, "agent1");
            }
            other => panic!("Expected AutoResolved, got {:?}", other),
        }
    }

    #[test]
    fn conflict_detection_priority_wins_lower_value_wins() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                strategy: DeconflictionStrategy::PriorityWins,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 0.5)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/plan.rs"], Some("b")),
        ];
        // "a" has priority 0 (higher), "b" has priority 2 (lower).
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 2, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        match &report.conflicts[0].resolution {
            ConflictResolution::AutoResolved {
                winner_agent,
                loser_agent,
                ..
            } => {
                // agent1's bead "a" has P0, agent2's bead "b" has P2 → agent1 wins.
                assert_eq!(winner_agent, "agent1");
                assert_eq!(loser_agent, "agent2");
            }
            other => panic!("Expected AutoResolved, got {:?}", other),
        }
    }

    #[test]
    fn conflict_detection_max_conflicts_per_cycle_bounded() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                max_conflicts_per_cycle: 1,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        // Create 3 potential conflicts.
        let aset = make_assignment_set(vec![
            make_assignment("a", "agent1", 1.0),
            make_assignment("b", "agent2", 0.9),
            make_assignment("c", "agent3", 0.8),
        ]);
        let active = vec![
            make_active_claim("a", "agent-x"),
            make_active_claim("b", "agent-y"),
            make_active_claim("c", "agent-z"),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
            sample_detail("c", BeadStatus::Open, 2, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &[], &active, 5000, &issues);
        assert_eq!(report.conflicts.len(), 1); // bounded
    }

    #[test]
    fn conflict_detection_updates_state_totals() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        assert_eq!(ml.state().total_conflicts_detected, 0);
        assert_eq!(ml.state().total_conflicts_auto_resolved, 0);

        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

        assert_eq!(ml.state().total_conflicts_detected, 1);
        assert_eq!(ml.state().total_conflicts_auto_resolved, 1);
        assert_eq!(ml.state().conflict_history.len(), 1);
    }

    #[test]
    fn conflict_detection_history_bounded() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                max_conflicts_per_cycle: 2,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        // Run multiple cycles with conflicts.
        for i in 0..10u64 {
            let aset = make_assignment_set(vec![
                make_assignment("bead-x", "agent1", 1.0),
                make_assignment("bead-x", "agent2", 0.5),
            ]);
            ml.state.cycle_count = i;
            ml.detect_conflicts(&aset, &[], &[], (i * 1000) as i64, &issues);
        }
        // History is bounded: max_conflicts_per_cycle * 4 = 8.
        assert!(ml.state().conflict_history.len() <= 8);
    }

    #[test]
    fn conflict_detection_conflict_stats_accessor() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let (detected, resolved) = ml.conflict_stats();
        assert_eq!(detected, 0);
        assert_eq!(resolved, 0);

        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

        let (detected, resolved) = ml.conflict_stats();
        assert_eq!(detected, 1);
        assert_eq!(resolved, 1);
    }

    #[test]
    fn conflict_detection_multiple_types_in_one_cycle() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        // agent1 has reservation overlap AND concurrent bead claim.
        let aset = make_assignment_set(vec![
            make_assignment("a", "agent1", 1.0),
            make_assignment("b", "agent1", 0.8),
            make_assignment("b", "agent3", 0.3),
        ]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/plan.rs"], Some("c")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
            sample_detail("c", BeadStatus::Open, 2, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        // Should detect both: reservation overlap + concurrent bead claim.
        assert!(report.conflicts.len() >= 2);
        let types: Vec<&ConflictType> = report.conflicts.iter().map(|c| &c.conflict_type).collect();
        assert!(types.contains(&&ConflictType::FileReservationOverlap));
        assert!(types.contains(&&ConflictType::ConcurrentBeadClaim));
    }

    #[test]
    fn conflict_detection_wildcard_path_overlap() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/mission_loop.rs"], Some("a")),
            make_reservation("agent2", &["src/*.rs"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].conflict_type,
            ConflictType::FileReservationOverlap
        );
    }

    #[test]
    fn conflict_detection_directory_containment_overlap() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert_eq!(report.conflicts.len(), 1);
    }

    #[test]
    fn conflict_detection_deconfliction_message_content() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let reservations = vec![
            make_reservation("agent1", &["src/plan.rs"], Some("a")),
            make_reservation("agent2", &["src/plan.rs"], Some("b")),
        ];
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        assert!(!report.messages.is_empty());
        let msg = &report.messages[0];
        assert!(msg.subject.contains("reservation_overlap"));
        assert!(msg.body.contains("Conflict detected"));
        assert!(msg.body.contains("FTM2001"));
        assert!(msg.body.contains("Handoff contract"));
        assert_eq!(msg.importance, "high");
    }

    #[test]
    fn conflict_detection_auto_resolved_messages_include_handoff_roles() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

        assert_eq!(report.messages.len(), 2);
        let winner_msg = report
            .messages
            .iter()
            .find(|m| m.recipient == "agent1")
            .expect("winner message");
        let loser_msg = report
            .messages
            .iter()
            .find(|m| m.recipient == "agent2")
            .expect("loser message");

        assert!(winner_msg.body.contains("- role: `winner`"));
        assert!(winner_msg.body.contains("- action: `retain_assignment`"));
        assert!(winner_msg.body.contains("- thread_id: `bead-x`"));
        assert!(loser_msg.body.contains("- role: `loser`"));
        assert!(loser_msg.body.contains("- action: `yield_assignment`"));
        assert!(loser_msg.body.contains("- thread_id: `bead-x`"));
    }

    #[test]
    fn conflict_detection_manual_resolution_message_has_explicit_wait_action() {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                strategy: DeconflictionStrategy::ManualResolution,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

        assert!(!report.messages.is_empty());
        assert!(
            report
                .messages
                .iter()
                .all(|m| m.body.contains("- role: `participant`"))
        );
        assert!(
            report
                .messages
                .iter()
                .all(|m| m.body.contains("- action: `await_manual_resolution`"))
        );
    }

    #[test]
    fn conflict_detection_report_serde_roundtrip() {
        let report = ConflictDetectionReport {
            cycle_id: 5,
            detected_at_ms: 5000,
            conflicts: vec![AssignmentConflict {
                conflict_id: "c1".to_string(),
                conflict_type: ConflictType::FileReservationOverlap,
                involved_agents: vec!["a1".to_string(), "a2".to_string()],
                involved_beads: vec!["bead-a".to_string()],
                conflicting_paths: vec!["src/plan.rs".to_string()],
                detected_at_ms: 5000,
                resolution: ConflictResolution::AutoResolved {
                    winner_agent: "a1".to_string(),
                    loser_agent: "a2".to_string(),
                    strategy: DeconflictionStrategy::PriorityWins,
                },
                reason_code: "reservation_overlap".to_string(),
                error_code: "FTM2001".to_string(),
            }],
            messages: vec![DeconflictionMessage {
                recipient: "a2".to_string(),
                subject: "conflict".to_string(),
                body: "test".to_string(),
                thread_id: "bead-a".to_string(),
                importance: "high".to_string(),
                conflict_id: "c1".to_string(),
            }],
            auto_resolved_count: 1,
            pending_resolution_count: 0,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ConflictDetectionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_id, 5);
        assert_eq!(back.conflicts.len(), 1);
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.auto_resolved_count, 1);
    }

    #[test]
    fn conflict_type_serde_roundtrip() {
        let types = vec![
            ConflictType::FileReservationOverlap,
            ConflictType::ResourceReservationOverlap,
            ConflictType::ConcurrentBeadClaim,
            ConflictType::ActiveClaimCollision,
        ];
        for ct in &types {
            let json = serde_json::to_string(ct).unwrap();
            let back: ConflictType = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, ct);
        }
    }

    #[test]
    fn deconfliction_strategy_serde_roundtrip() {
        let strategies = vec![
            DeconflictionStrategy::PriorityWins,
            DeconflictionStrategy::FirstClaimWins,
            DeconflictionStrategy::ManualResolution,
        ];
        for s in &strategies {
            let json = serde_json::to_string(s).unwrap();
            let back: DeconflictionStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, s);
        }
    }

    #[test]
    fn conflict_resolution_serde_roundtrip() {
        let resolutions = vec![
            ConflictResolution::AutoResolved {
                winner_agent: "w".to_string(),
                loser_agent: "l".to_string(),
                strategy: DeconflictionStrategy::PriorityWins,
            },
            ConflictResolution::Deferred {
                retry_after_ms: 5000,
            },
            ConflictResolution::PendingManualResolution,
        ];
        for r in &resolutions {
            let json = serde_json::to_string(r).unwrap();
            let back: ConflictResolution = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, r);
        }
    }

    #[test]
    fn conflict_detection_config_serde_roundtrip() {
        let config = ConflictDetectionConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: ConflictDetectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.enabled, config.enabled);
        assert_eq!(back.max_conflicts_per_cycle, config.max_conflicts_per_cycle);
        assert_eq!(back.strategy, config.strategy);
        assert_eq!(back.generate_messages, config.generate_messages);
    }

    #[test]
    fn paths_overlap_exact_match() {
        assert!(paths_overlap("src/plan.rs", "src/plan.rs"));
    }

    #[test]
    fn paths_overlap_no_match() {
        assert!(!paths_overlap("src/plan.rs", "src/mission_loop.rs"));
    }

    #[test]
    fn paths_overlap_directory_containment() {
        assert!(paths_overlap("src/", "src/plan.rs"));
        assert!(paths_overlap("src/plan.rs", "src/"));
    }

    #[test]
    fn paths_overlap_wildcard() {
        assert!(paths_overlap("src/*.rs", "src/plan.rs"));
        assert!(paths_overlap("src/plan.rs", "src/*.rs"));
    }

    #[test]
    fn paths_overlap_no_false_prefix() {
        // "src/plan" is a prefix of "src/planner.rs" but NOT a directory boundary.
        assert!(!paths_overlap("src/plan", "src/planner.rs"));
    }

    #[test]
    fn wildcard_match_basic() {
        assert!(wildcard_match("*.rs", "plan.rs"));
        assert!(wildcard_match("src/*", "src/plan.rs"));
        assert!(wildcard_match("src/?.rs", "src/a.rs"));
        assert!(!wildcard_match("src/?.rs", "src/ab.rs"));
    }

    #[test]
    fn wildcard_match_complex() {
        assert!(wildcard_match("**/plan.rs", "crates/core/src/plan.rs"));
        assert!(wildcard_match("src/*.rs", "src/mission_loop.rs"));
        assert!(!wildcard_match("src/*.rs", "tests/foo.rs"));
    }

    #[test]
    fn resource_scopes_overlap_exact_match() {
        assert!(resource_scopes_overlap("pane:alpha", "pane:alpha"));
    }

    #[test]
    fn resource_scopes_overlap_wildcard_match() {
        assert!(resource_scopes_overlap("pane:*", "pane:alpha"));
        assert!(resource_scopes_overlap("pane:alpha", "pane:*"));
    }

    #[test]
    fn resource_scopes_overlap_no_match() {
        assert!(!resource_scopes_overlap("pane:alpha", "pane:beta"));
    }

    #[test]
    fn loop_config_with_conflict_detection_serde_roundtrip() {
        let config = MissionLoopConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: MissionLoopConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.conflict_detection.enabled,
            config.conflict_detection.enabled
        );
        assert_eq!(
            back.conflict_detection.max_conflicts_per_cycle,
            config.conflict_detection.max_conflicts_per_cycle
        );
    }

    #[test]
    fn conflict_detection_three_agents_same_bead() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![
            make_assignment("bead-x", "agent1", 1.0),
            make_assignment("bead-x", "agent2", 0.5),
            make_assignment("bead-x", "agent3", 0.3),
        ]);
        let issues = vec![sample_detail("bead-x", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        // Two conflicts: agent2 vs winner and agent3 vs winner.
        assert_eq!(report.conflicts.len(), 2);
        assert!(
            report
                .conflicts
                .iter()
                .all(|c| c.conflict_type == ConflictType::ConcurrentBeadClaim)
        );
    }

    #[test]
    fn conflict_detection_message_thread_id_uses_bead() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let aset = make_assignment_set(vec![make_assignment("a", "agent1", 1.0)]);
        let active = vec![make_active_claim("a", "agent2")];
        let issues = vec![sample_detail("a", BeadStatus::Open, 0, &[])];
        let report = ml.detect_conflicts(&aset, &[], &active, 5000, &issues);
        assert!(!report.messages.is_empty());
        assert_eq!(report.messages[0].thread_id, "a");
    }

    // ── Operator report tests (ft-1i2ge.5.5) ────────────────────────────

    #[test]
    fn operator_report_idle_state() {
        let ml = MissionLoop::new(MissionLoopConfig::default());
        let report = ml.generate_operator_report(None, None);
        assert_eq!(report.status.cycle_count, 0);
        assert_eq!(report.status.phase_label, "idle");
        assert!(report.assignment_table.is_empty());
        assert_eq!(report.health.overall, "idle");
        assert_eq!(report.conflicts.total_detected, 0);
        assert_eq!(report.event_summary.total_emitted, 0);
        assert!(report.latest_explanations.is_empty());
    }

    #[test]
    fn operator_report_after_evaluation() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let agents = vec![ready_agent("alpha"), ready_agent("beta")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let report = ml.generate_operator_report(None, None);
        assert_eq!(report.status.cycle_count, 1);
        assert_ne!(report.status.phase_label, "idle");
        // At least one agent should appear in assignment table
        // (depends on solver producing assignments)
        assert!(report.status.total_assignments > 0 || report.status.total_rejections > 0);
    }

    #[test]
    fn operator_report_with_event_log() {
        use crate::mission_events::{
            MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
        };

        let ml = MissionLoop::new(MissionLoopConfig::default());
        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 100,
            enabled: true,
        });
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::CycleStarted,
                "mission.lifecycle.cycle_started",
            )
            .correlation("corr-001")
            .cycle(1, 1000)
            .labels("test", "mission"),
        );
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::CycleCompleted,
                "mission.lifecycle.cycle_completed",
            )
            .correlation("corr-001")
            .cycle(1, 1050)
            .labels("test", "mission"),
        );

        let report = ml.generate_operator_report(Some(&log), None);
        assert_eq!(report.event_summary.total_emitted, 2);
        assert_eq!(report.event_summary.retained_events, 2);
        assert!(report.event_summary.by_phase.contains_key("Lifecycle"));
    }

    #[test]
    fn operator_report_with_explainability() {
        use crate::planner_features::{
            DecisionExplanation, DecisionOutcome, ExplainabilityReport, ExplanationFactor,
            FactorPolarity,
        };

        let ml = MissionLoop::new(MissionLoopConfig::default());
        let explain = ExplainabilityReport {
            cycle_id: 1,
            explanations: vec![
                DecisionExplanation {
                    bead_id: "bead-1".to_string(),
                    outcome: DecisionOutcome::Assigned,
                    summary: "Assigned to alpha (rank #1, score 0.850)".to_string(),
                    factors: vec![
                        ExplanationFactor {
                            dimension: "composite_score".to_string(),
                            value: 0.85,
                            description: "Weighted composite".to_string(),
                            polarity: FactorPolarity::Positive,
                        },
                        ExplanationFactor {
                            dimension: "effort_penalty".to_string(),
                            value: 0.1,
                            description: "Low effort".to_string(),
                            polarity: FactorPolarity::Negative,
                        },
                    ],
                },
                DecisionExplanation {
                    bead_id: "bead-2".to_string(),
                    outcome: DecisionOutcome::Rejected,
                    summary: "Rejected (score 0.200): No capacity".to_string(),
                    factors: vec![ExplanationFactor {
                        dimension: "rejection".to_string(),
                        value: 0.0,
                        description: "No agent capacity available".to_string(),
                        polarity: FactorPolarity::Negative,
                    }],
                },
            ],
        };

        let report = ml.generate_operator_report(None, Some(&explain));
        assert_eq!(report.latest_explanations.len(), 2);
        assert_eq!(report.latest_explanations[0].bead_id, "bead-1");
        assert_eq!(report.latest_explanations[0].outcome, "Assigned");
        assert_eq!(report.latest_explanations[0].top_factors.len(), 2);
        assert_eq!(report.latest_explanations[1].bead_id, "bead-2");
        assert_eq!(report.latest_explanations[1].outcome, "Rejected");
    }

    #[test]
    fn operator_report_health_section_degraded() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        // Inject metrics samples with high conflict rate
        ml.state.metrics_history.push(MissionCycleMetricsSample {
            cycle_id: 1,
            timestamp_ms: 1000,
            evaluation_latency_ms: 50,
            assignments: 3,
            rejections: 2,
            conflict_rejections: 2,
            policy_denials: 1,
            unblocked_transitions: 0,
            planner_churn_events: 2,
            throughput_assignments_per_minute: 5.0,
            unblock_velocity_per_minute: 1.0,
            conflict_rate: 0.25,
            planner_churn_rate: 0.35,
            policy_deny_rate: 0.1,
            assignments_by_agent: HashMap::new(),
            workspace_label: "test".to_string(),
            track_label: "mission".to_string(),
        });

        let report = ml.generate_operator_report(None, None);
        assert_eq!(report.health.overall, "degraded");
        assert!((report.health.conflict_rate - 0.25).abs() < 1e-10);
        assert!((report.health.planner_churn_rate - 0.35).abs() < 1e-10);
    }

    #[test]
    fn operator_report_health_section_critical() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.state.metrics_history.push(MissionCycleMetricsSample {
            cycle_id: 1,
            timestamp_ms: 1000,
            evaluation_latency_ms: 100,
            assignments: 1,
            rejections: 5,
            conflict_rejections: 4,
            policy_denials: 3,
            unblocked_transitions: 0,
            planner_churn_events: 0,
            throughput_assignments_per_minute: 1.0,
            unblock_velocity_per_minute: 0.0,
            conflict_rate: 0.5,
            planner_churn_rate: 0.0,
            policy_deny_rate: 0.6,
            assignments_by_agent: HashMap::new(),
            workspace_label: "test".to_string(),
            track_label: "mission".to_string(),
        });

        let report = ml.generate_operator_report(None, None);
        assert_eq!(report.health.overall, "critical");
    }

    #[test]
    fn operator_report_conflict_section() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.state.total_conflicts_detected = 5;
        ml.state.total_conflicts_auto_resolved = 3;
        ml.state.conflict_history.push(AssignmentConflict {
            conflict_id: "c-1".to_string(),
            conflict_type: ConflictType::ConcurrentBeadClaim,
            involved_agents: vec!["a1".to_string(), "a2".to_string()],
            involved_beads: vec!["b1".to_string()],
            conflicting_paths: Vec::new(),
            detected_at_ms: 1000,
            resolution: ConflictResolution::PendingManualResolution,
            reason_code: "concurrent_bead_claim".to_string(),
            error_code: "FTM2002".to_string(),
        });

        let report = ml.generate_operator_report(None, None);
        assert_eq!(report.conflicts.total_detected, 5);
        assert_eq!(report.conflicts.total_auto_resolved, 3);
        assert_eq!(report.conflicts.pending_manual, 1);
        assert_eq!(report.conflicts.recent_conflicts.len(), 1);
        assert_eq!(report.conflicts.recent_conflicts[0].conflict_id, "c-1");
    }

    #[test]
    fn operator_report_assignment_table() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.state
            .metrics_totals
            .assignments_by_agent
            .insert("alpha".to_string(), 10);
        ml.state
            .metrics_totals
            .assignments_by_agent
            .insert("beta".to_string(), 5);
        ml.state.last_decision = Some(MissionDecision {
            cycle_id: 1,
            timestamp_ms: 1000,
            trigger: MissionTrigger::CadenceTick,
            assignment_set: make_assignment_set(vec![
                make_assignment("bead-a", "alpha", 0.9),
                make_assignment("bead-b", "alpha", 0.7),
            ]),
            extraction_summary: ExtractionSummary {
                total_candidates: 3,
                ready_candidates: 2,
                top_impact_bead: Some("bead-a".to_string()),
            },
            scorer_summary: ScorerSummary {
                scored_count: 2,
                above_threshold_count: 2,
                top_scored_bead: Some("bead-a".to_string()),
            },
        });

        let report = ml.generate_operator_report(None, None);
        assert_eq!(report.assignment_table.len(), 2);
        // Sorted by total_assignments descending
        assert_eq!(report.assignment_table[0].agent_id, "alpha");
        assert_eq!(report.assignment_table[0].total_assignments, 10);
        assert_eq!(report.assignment_table[0].active_beads, 2);
        assert!(
            report.assignment_table[0]
                .active_bead_ids
                .contains(&"bead-a".to_string())
        );
    }

    #[test]
    fn operator_report_plain_format_renders() {
        let ml = MissionLoop::new(MissionLoopConfig::default());
        let report = ml.generate_operator_report(None, None);
        let plain = format_operator_report_plain(&report);
        assert!(plain.contains("=== Mission Status ==="));
        assert!(plain.contains("Phase:"));
        assert!(plain.contains("idle"));
        assert!(plain.contains("=== Health ==="));
    }

    #[test]
    fn operator_report_json_roundtrip() {
        let ml = MissionLoop::new(MissionLoopConfig::default());
        let report = ml.generate_operator_report(None, None);
        let json = serde_json::to_string(&report).expect("serialize");
        let deser: OperatorStatusReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deser.status.cycle_count, report.status.cycle_count);
        assert_eq!(deser.status.phase_label, report.status.phase_label);
        assert_eq!(deser.health.overall, report.health.overall);
    }

    // ── Determinism and edge-case tests (ft-1i2ge.5.5 AC5) ─────────────

    #[test]
    fn operator_report_deterministic_repeat() {
        // Generating the report twice from the same state MUST produce identical output.
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("a", BeadStatus::Open, 1, &[]),
            sample_detail("b", BeadStatus::Open, 2, &[]),
        ];
        let agents = vec![ready_agent("agent1")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let r1 = ml.generate_operator_report(None, None);
        let r2 = ml.generate_operator_report(None, None);

        let j1 = serde_json::to_string(&r1).expect("serialize r1");
        let j2 = serde_json::to_string(&r2).expect("serialize r2");
        assert_eq!(
            j1, j2,
            "Reports must be deterministic across repeated calls"
        );
    }

    #[test]
    fn operator_report_plain_format_deterministic() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("x", BeadStatus::Open, 0, &[])];
        let agents = vec![ready_agent("alpha")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(2000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let report = ml.generate_operator_report(None, None);
        let plain1 = format_operator_report_plain(&report);
        let plain2 = format_operator_report_plain(&report);
        assert_eq!(plain1, plain2, "Plain format must be deterministic");
    }

    #[test]
    fn operator_report_with_multiple_rejections() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        // Use solver config that rejects everything (min_score = 1.0).
        ml.config.solver_config.min_score = 1.0;

        let issues = vec![
            sample_detail("r1", BeadStatus::Open, 2, &[]),
            sample_detail("r2", BeadStatus::Open, 3, &[]),
            sample_detail("r3", BeadStatus::Open, 4, &[]),
        ];
        let agents = vec![ready_agent("a")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(3000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let report = ml.generate_operator_report(None, None);
        // All candidates should be rejected (score below 1.0).
        assert_eq!(report.status.total_rejections, 3);
        assert_eq!(report.status.total_assignments, 0);
    }

    #[test]
    fn operator_report_no_agents_available() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("orphan", BeadStatus::Open, 0, &[])];
        let agents: Vec<crate::plan::MissionAgentCapabilityProfile> = Vec::new();
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(4000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let report = ml.generate_operator_report(None, None);
        assert!(report.assignment_table.is_empty());
        assert_eq!(report.status.total_assignments, 0);
    }

    #[test]
    fn operator_report_retry_storm_beads_listed() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.state.retry_streaks.insert("storm-bead-1".to_string(), 5);
        ml.state
            .retry_streaks
            .insert("storm-bead-2".to_string(), 10);
        // Normal bead below threshold
        ml.state.retry_streaks.insert("ok-bead".to_string(), 1);

        let report = ml.generate_operator_report(None, None);
        // Verify report generates without panic; cycle_count is 0 since
        // no evaluate() was called.
        assert_eq!(report.status.cycle_count, 0);
    }

    #[test]
    fn operator_report_json_roundtrip_with_data() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![
            sample_detail("b1", BeadStatus::Open, 1, &[]),
            sample_detail("b2", BeadStatus::Open, 2, &[]),
        ];
        let agents = vec![ready_agent("ag1"), ready_agent("ag2")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(5000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        ml.state.total_conflicts_detected = 3;
        ml.state.total_conflicts_auto_resolved = 2;

        let report = ml.generate_operator_report(None, None);
        let json = serde_json::to_string_pretty(&report).expect("serialize");
        let deser: OperatorStatusReport = serde_json::from_str(&json).expect("deserialize");

        // Full structural comparison
        assert_eq!(deser.status.cycle_count, report.status.cycle_count);
        assert_eq!(
            deser.status.total_assignments,
            report.status.total_assignments
        );
        assert_eq!(
            deser.status.total_rejections,
            report.status.total_rejections
        );
        assert_eq!(deser.health.overall, report.health.overall);
        assert_eq!(
            deser.conflicts.total_detected,
            report.conflicts.total_detected
        );
        assert_eq!(
            deser.conflicts.total_auto_resolved,
            report.conflicts.total_auto_resolved
        );
        assert_eq!(deser.assignment_table.len(), report.assignment_table.len());
    }

    #[test]
    fn operator_report_plain_format_with_full_data() {
        use crate::mission_events::{
            MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
        };
        use crate::planner_features::{
            DecisionExplanation, DecisionOutcome, ExplainabilityReport, ExplanationFactor,
            FactorPolarity,
        };

        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("t1", BeadStatus::Open, 1, &[])];
        let agents = vec![ready_agent("worker")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(6000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 50,
            enabled: true,
        });
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::CycleStarted,
                "mission.lifecycle.cycle_started",
            )
            .correlation("c-001")
            .cycle(1, 6000)
            .labels("test", "mission"),
        );

        let explain = ExplainabilityReport {
            cycle_id: 1,
            explanations: vec![DecisionExplanation {
                bead_id: "t1".to_string(),
                outcome: DecisionOutcome::Assigned,
                summary: "Assigned to worker (rank #1)".to_string(),
                factors: vec![ExplanationFactor {
                    dimension: "composite_score".to_string(),
                    value: 0.7,
                    description: "Strong composite".to_string(),
                    polarity: FactorPolarity::Positive,
                }],
            }],
        };

        let report = ml.generate_operator_report(Some(&log), Some(&explain));
        let plain = format_operator_report_plain(&report);

        // Verify all sections present
        assert!(plain.contains("=== Mission Status ==="));
        assert!(plain.contains("=== Health ==="));
        assert!(plain.contains("=== Agent Assignments ==="));
        assert!(plain.contains("worker"));
        assert!(plain.contains("=== Event Log ==="));
        assert!(plain.contains("=== Latest Decisions ==="));
        assert!(plain.contains("t1"));
        assert!(plain.contains("Assigned"));
    }

    #[test]
    fn operator_report_empty_metrics_window() {
        let ml = MissionLoop::new(MissionLoopConfig::default());
        let report = ml.generate_operator_report(None, None);

        // Health section defaults
        assert_eq!(report.health.overall, "idle");
        assert!(
            report.health.throughput_assignments_per_minute.abs() < f64::EPSILON,
            "throughput should be 0"
        );
        assert!(
            report.health.avg_evaluation_latency_ms.abs() < f64::EPSILON,
            "avg latency should be 0"
        );
        assert!(
            report.health.conflict_rate.abs() < f64::EPSILON,
            "conflict rate should be 0"
        );
    }

    // ── Operator override control tests (ft-1i2ge.5.6) ──────────────────

    fn make_override(id: &str, kind: OperatorOverrideKind) -> OperatorOverride {
        OperatorOverride {
            override_id: id.to_string(),
            kind,
            activated_by: "operator".to_string(),
            reason_code: "test.override".to_string(),
            rationale: "Test override".to_string(),
            activated_at_ms: 0,
            expires_at_ms: None,
            correlation_id: None,
        }
    }

    #[test]
    fn override_state_activate_and_clear() {
        let mut state = OperatorOverrideState::default();
        let ovr = make_override(
            "ovr-1",
            OperatorOverrideKind::Exclude {
                bead_id: "b1".to_string(),
            },
        );
        state.activate(ovr);
        assert_eq!(state.active.len(), 1);
        assert!(state.history.is_empty());

        assert!(state.clear("ovr-1", 1000));
        assert!(state.active.is_empty());
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].override_id, "ovr-1");
        assert_eq!(state.history[0].expires_at_ms, Some(1000));
    }

    #[test]
    fn override_state_clear_nonexistent_returns_false() {
        let mut state = OperatorOverrideState::default();
        assert!(!state.clear("nonexistent", 1000));
    }

    #[test]
    fn override_state_evict_expired() {
        let mut state = OperatorOverrideState::default();
        let mut ovr1 = make_override(
            "exp-1",
            OperatorOverrideKind::Exclude {
                bead_id: "b1".to_string(),
            },
        );
        ovr1.expires_at_ms = Some(500);
        let ovr2 = make_override(
            "perm-1",
            OperatorOverrideKind::Exclude {
                bead_id: "b2".to_string(),
            },
        );
        state.activate(ovr1);
        state.activate(ovr2);
        assert_eq!(state.active.len(), 2);

        state.evict_expired(600);
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].override_id, "perm-1");
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].override_id, "exp-1");
    }

    #[test]
    fn override_state_active_pins() {
        let mut state = OperatorOverrideState::default();
        state.activate(make_override(
            "pin-1",
            OperatorOverrideKind::Pin {
                bead_id: "b1".to_string(),
                target_agent: "agent-x".to_string(),
            },
        ));
        state.activate(make_override(
            "excl-1",
            OperatorOverrideKind::Exclude {
                bead_id: "b2".to_string(),
            },
        ));
        let pins = state.active_pins();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0], ("b1", "agent-x"));
    }

    #[test]
    fn override_state_excluded_bead_and_agent_ids() {
        let mut state = OperatorOverrideState::default();
        state.activate(make_override(
            "excl-b",
            OperatorOverrideKind::Exclude {
                bead_id: "b1".to_string(),
            },
        ));
        state.activate(make_override(
            "excl-a",
            OperatorOverrideKind::ExcludeAgent {
                agent_id: "agent-y".to_string(),
            },
        ));
        assert_eq!(state.excluded_bead_ids(), vec!["b1"]);
        assert_eq!(state.excluded_agent_ids(), vec!["agent-y"]);
    }

    #[test]
    fn override_state_reprioritize_deltas() {
        let mut state = OperatorOverrideState::default();
        state.activate(make_override(
            "rep-1",
            OperatorOverrideKind::Reprioritize {
                bead_id: "b1".to_string(),
                score_delta: 50,
            },
        ));
        state.activate(make_override(
            "rep-2",
            OperatorOverrideKind::Reprioritize {
                bead_id: "b1".to_string(),
                score_delta: -20,
            },
        ));
        state.activate(make_override(
            "rep-3",
            OperatorOverrideKind::Reprioritize {
                bead_id: "b2".to_string(),
                score_delta: 100,
            },
        ));
        let deltas = state.reprioritize_deltas();
        assert_eq!(deltas.get("b1"), Some(&30)); // 50 + (-20) = 30
        assert_eq!(deltas.get("b2"), Some(&100));
    }

    #[test]
    fn override_is_expired() {
        let mut ovr = make_override(
            "e1",
            OperatorOverrideKind::Exclude {
                bead_id: "x".to_string(),
            },
        );
        assert!(!ovr.is_expired(100));

        ovr.expires_at_ms = Some(500);
        assert!(!ovr.is_expired(499));
        assert!(ovr.is_expired(500));
        assert!(ovr.is_expired(1000));
    }

    #[test]
    fn override_target_bead_id() {
        let pin = make_override(
            "p",
            OperatorOverrideKind::Pin {
                bead_id: "b1".to_string(),
                target_agent: "a1".to_string(),
            },
        );
        assert_eq!(pin.target_bead_id(), Some("b1"));

        let excl_agent = make_override(
            "ea",
            OperatorOverrideKind::ExcludeAgent {
                agent_id: "a1".to_string(),
            },
        );
        assert_eq!(excl_agent.target_bead_id(), None);
    }

    #[test]
    fn apply_override_duplicate_id_rejected() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let ovr = make_override(
            "dup",
            OperatorOverrideKind::Exclude {
                bead_id: "b1".to_string(),
            },
        );
        assert!(ml.apply_override(ovr.clone()).is_ok());
        assert!(ml.apply_override(ovr).is_err());
    }

    #[test]
    fn clear_override_moves_to_history() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let ovr = make_override(
            "clr",
            OperatorOverrideKind::Exclude {
                bead_id: "b1".to_string(),
            },
        );
        ml.apply_override(ovr).unwrap();
        assert_eq!(ml.active_overrides().len(), 1);

        assert!(ml.clear_override("clr", 5000));
        assert!(ml.active_overrides().is_empty());
        assert_eq!(ml.state.override_state.history.len(), 1);
    }

    #[test]
    fn evaluate_with_exclude_bead_override() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.apply_override(make_override(
            "excl-a",
            OperatorOverrideKind::Exclude {
                bead_id: "a".to_string(),
            },
        ))
        .unwrap();

        let issues = vec![
            sample_detail("a", BeadStatus::Open, 1, &[]),
            sample_detail("b", BeadStatus::Open, 2, &[]),
        ];
        let agents = vec![ready_agent("alpha")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // Bead "a" should be rejected by safety gate.
        let is_rejected = decision
            .assignment_set
            .rejected
            .iter()
            .any(|r| r.bead_id == "a");
        assert!(is_rejected, "excluded bead 'a' should be rejected");
        // Override summary should record the exclusion.
        let summary = ml.state.last_override_summary.as_ref().unwrap();
        assert!(summary.excluded_beads.contains(&"a".to_string()));
    }

    #[test]
    fn evaluate_with_exclude_agent_override() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.apply_override(make_override(
            "excl-agent",
            OperatorOverrideKind::ExcludeAgent {
                agent_id: "alpha".to_string(),
            },
        ))
        .unwrap();

        let issues = vec![sample_detail("b1", BeadStatus::Open, 1, &[])];
        let agents = vec![ready_agent("alpha")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // With the only agent excluded, nothing can be assigned.
        assert!(
            decision.assignment_set.assignments.is_empty(),
            "no assignments when only agent is excluded"
        );
        let summary = ml.state.last_override_summary.as_ref().unwrap();
        assert!(summary.excluded_agents.contains(&"alpha".to_string()));
    }

    #[test]
    fn evaluate_with_pin_override() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        ml.apply_override(make_override(
            "pin-b1",
            OperatorOverrideKind::Pin {
                bead_id: "b1".to_string(),
                target_agent: "alpha".to_string(),
            },
        ))
        .unwrap();

        let issues = vec![
            sample_detail("b1", BeadStatus::Open, 1, &[]),
            sample_detail("b2", BeadStatus::Open, 2, &[]),
        ];
        let agents = vec![ready_agent("alpha"), ready_agent("beta")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // Pinned bead should be assigned to the pinned agent.
        let pinned = decision
            .assignment_set
            .assignments
            .iter()
            .find(|a| a.bead_id == "b1");
        assert!(pinned.is_some(), "pinned bead should be assigned");
        assert_eq!(pinned.unwrap().agent_id, "alpha");
        assert_eq!(
            pinned.unwrap().rank,
            1,
            "pinned assignment should be rank 1"
        );
    }

    #[test]
    fn evaluate_with_reprioritize_boost() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        // Give "low" bead a massive boost.
        ml.apply_override(make_override(
            "boost-low",
            OperatorOverrideKind::Reprioritize {
                bead_id: "low".to_string(),
                score_delta: 500, // +5.0 added to score
            },
        ))
        .unwrap();

        let issues = vec![
            sample_detail("high", BeadStatus::Open, 5, &[]),
            sample_detail("low", BeadStatus::Open, 0, &[]),
        ];
        let agents = vec![ready_agent("alpha")];
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // With a +5.0 boost, "low" should be assigned first.
        let has_low = decision
            .assignment_set
            .assignments
            .iter()
            .any(|a| a.bead_id == "low");
        assert!(has_low, "boosted bead 'low' should be assigned");

        let summary = ml.state.last_override_summary.as_ref().unwrap();
        assert!(!summary.reprioritized_beads.is_empty());
        let rep = summary
            .reprioritized_beads
            .iter()
            .find(|r| r.bead_id == "low")
            .unwrap();
        assert_eq!(rep.delta, 500);
        assert!(
            rep.adjusted_score > rep.original_score,
            "adjusted score should be higher"
        );
    }

    #[test]
    fn evaluate_with_expired_override_evicted() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let mut ovr = make_override(
            "expiring",
            OperatorOverrideKind::Exclude {
                bead_id: "a".to_string(),
            },
        );
        ovr.expires_at_ms = Some(500);
        ml.apply_override(ovr).unwrap();

        let issues = vec![sample_detail("a", BeadStatus::Open, 1, &[])];
        let agents = vec![ready_agent("alpha")];
        let ctx = PlannerExtractionContext::default();

        // Evaluate at t=1000, override should have expired.
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        // Override expired, so bead "a" should NOT be excluded.
        assert!(
            ml.active_overrides().is_empty(),
            "expired override should be evicted"
        );
        let summary = ml.state.last_override_summary.as_ref().unwrap();
        assert_eq!(summary.expired_overrides, 1);
        // Bead "a" should be assignable.
        let assigned = decision
            .assignment_set
            .assignments
            .iter()
            .any(|a| a.bead_id == "a");
        let is_rejected_by_gate = decision.assignment_set.rejected.iter().any(|r| {
            r.bead_id == "a"
                && r.reasons
                    .iter()
                    .any(|reason| matches!(reason, RejectionReason::SafetyGateDenied { .. }))
        });
        assert!(
            assigned || !is_rejected_by_gate,
            "expired override should not block bead 'a'"
        );
    }

    #[test]
    fn override_state_serde_roundtrip() {
        let mut state = OperatorOverrideState::default();
        state.activate(make_override(
            "s1",
            OperatorOverrideKind::Pin {
                bead_id: "b1".to_string(),
                target_agent: "a1".to_string(),
            },
        ));
        state.activate(make_override(
            "s2",
            OperatorOverrideKind::Reprioritize {
                bead_id: "b2".to_string(),
                score_delta: -30,
            },
        ));
        let json = serde_json::to_string(&state).unwrap();
        let back: OperatorOverrideState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active.len(), 2);
        assert_eq!(back.active[0].override_id, "s1");
        assert_eq!(back.active[1].override_id, "s2");
    }

    #[test]
    fn override_no_overrides_produces_no_summary() {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![sample_detail("b1", BeadStatus::Open, 1, &[])];
        let agents = vec![ready_agent("alpha")];
        let ctx = PlannerExtractionContext::default();
        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

        assert!(
            ml.state.last_override_summary.is_none(),
            "no overrides = no summary"
        );
    }

    #[test]
    fn override_history_bounded() {
        let mut state = OperatorOverrideState::default();
        for i in 0..150 {
            let ovr = make_override(
                &format!("hist-{i}"),
                OperatorOverrideKind::Exclude {
                    bead_id: format!("b{i}"),
                },
            );
            state.activate(ovr);
            state.clear(&format!("hist-{i}"), (i * 10) as i64);
        }
        assert!(
            state.history.len() <= OperatorOverrideState::MAX_HISTORY,
            "history should be bounded to {}",
            OperatorOverrideState::MAX_HISTORY
        );
    }

    #[test]
    fn override_kind_equality() {
        let pin1 = OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a1".to_string(),
        };
        let pin2 = OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a1".to_string(),
        };
        let excl = OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        };
        assert_eq!(pin1, pin2);
        assert_ne!(pin1, excl);
    }
}
