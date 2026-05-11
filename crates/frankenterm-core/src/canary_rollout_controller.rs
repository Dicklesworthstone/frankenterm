//! Canary rollout controller for mission dispatch (ft-1i2ge.6.4).
//!
//! Manages staged rollout of mission dispatch through three phases:
//!
//! ```text
//! Shadow ──▶ Canary ──▶ Full
//!    ▲          │         │
//!    └──────────┘─────────┘   (rollback on health failure)
//! ```
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
//!              ↓
//!     CanaryRolloutController.evaluate_health()
//!              ↓
//!     CanaryDecision (hold / advance / rollback)
//!              ↓
//!     CanaryRolloutController.filter_assignments()
//!              ↓
//!     Filtered AssignmentSet (subset for canary phase)
//! ```
//!
//! # Phases
//!
//! - **Shadow**: No assignments dispatched. Collect metrics only.
//! - **Canary**: Dispatch to a configurable percentage of agents.
//! - **Full**: All assignments dispatched normally.
//!
//! Rollback to a previous phase occurs automatically when health checks
//! detect degraded fidelity, excessive safety rejections, or other
//! configurable trigger conditions.

use crate::planner_features::{AssignmentSet, RejectedCandidate};
use crate::shadow_mode_evaluator::{ShadowModeDiff, ShadowModeMetrics};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

// ── Phase state machine ─────────────────────────────────────────────────────

/// Rollout phase for mission dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryPhase {
    /// Observe only — no assignments dispatched.
    Shadow,
    /// Dispatch to a configurable subset of agents.
    Canary,
    /// Full dispatch — all assignments pass through.
    Full,
}

impl CanaryPhase {
    /// Whether this phase dispatches any assignments.
    #[must_use]
    pub fn dispatches(&self) -> bool {
        !matches!(self, Self::Shadow)
    }

    /// The natural next phase in the progression.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Shadow => Some(Self::Canary),
            Self::Canary => Some(Self::Full),
            Self::Full => None,
        }
    }

    /// Whether this phase can transition to `target`.
    #[must_use]
    pub fn can_advance_to(self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Shadow, Self::Canary) | (Self::Canary, Self::Full)
        )
    }

    /// Whether rollback from this phase to `target` is legal.
    #[must_use]
    pub fn can_rollback_to(self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Full, Self::Canary | Self::Shadow) | (Self::Canary, Self::Shadow)
        )
    }
}

// ── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the canary rollout controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryRolloutConfig {
    /// Starting phase (default: Shadow).
    pub initial_phase: CanaryPhase,

    /// Percentage of agents to include during canary phase (0.0–1.0).
    pub canary_agent_fraction: f64,

    /// Minimum fidelity score from shadow evaluator before advancing.
    /// Below this triggers rollback.
    pub fidelity_threshold: f64,

    /// Maximum consecutive unhealthy checks before triggering rollback.
    pub max_consecutive_unhealthy: u32,

    /// Minimum healthy checks required before advancing to next phase.
    pub min_healthy_before_advance: u32,

    /// Minimum cycles the shadow evaluator must have completed (warmup).
    pub min_warmup_cycles: u64,

    /// Maximum safety gate rejections per cycle before marking unhealthy.
    pub max_safety_rejections_per_cycle: usize,

    /// Maximum conflict rate (conflicts / dispatches) before marking unhealthy.
    pub max_conflict_rate: f64,

    /// Enable automatic phase advancement when health criteria are met.
    pub auto_advance: bool,

    /// Enable automatic rollback on health degradation.
    pub auto_rollback: bool,

    /// Optional: specific agent IDs to include in canary (overrides fraction).
    pub canary_agent_allowlist: Vec<String>,
}

impl Default for CanaryRolloutConfig {
    fn default() -> Self {
        Self {
            initial_phase: CanaryPhase::Shadow,
            canary_agent_fraction: 0.2,
            fidelity_threshold: 0.7,
            max_consecutive_unhealthy: 3,
            min_healthy_before_advance: 5,
            min_warmup_cycles: 5,
            max_safety_rejections_per_cycle: 5,
            max_conflict_rate: 0.3,
            auto_advance: true,
            auto_rollback: true,
            canary_agent_allowlist: Vec::new(),
        }
    }
}

/// br-ft-43wis: structured error returned by
/// [`CanaryRolloutConfig::validate`] when a numeric field is
/// non-finite or out of its documented range. Canary rollout is a
/// safety gate for mission dispatch; a malformed threshold can mark
/// a bad cycle healthy and let auto-advance override the operator's
/// rollback intent. Validation is fail-closed: rejected configs are
/// rejected at controller-construction time, not silently coerced.
#[derive(Debug, Clone, PartialEq)]
pub enum CanaryConfigError {
    /// `canary_agent_fraction` is NaN, Inf, negative, or > 1.0.
    InvalidCanaryAgentFraction(f64),
    /// `fidelity_threshold` is NaN, Inf, negative, or > 1.0.
    InvalidFidelityThreshold(f64),
    /// `max_conflict_rate` is NaN, Inf, negative, or > 1.0.
    InvalidMaxConflictRate(f64),
    /// br-ft-ao0i5: an entry in `canary_agent_allowlist` is empty,
    /// blank, or duplicates another entry. Empty / blank entries can
    /// silently expand the cohort by short-circuiting agent_id
    /// equality checks; duplicates inflate cohort size accounting.
    InvalidAllowlistEntry {
        index: usize,
        reason: AllowlistEntryError,
    },
}

/// Why a `canary_agent_allowlist` entry was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowlistEntryError {
    /// Entry is empty after trimming whitespace.
    EmptyOrWhitespace,
    /// Entry exactly matches an earlier entry (case-sensitive).
    Duplicate { first_index: usize },
}

impl std::fmt::Display for CanaryConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCanaryAgentFraction(v) => write!(
                f,
                "br-ft-43wis: canary_agent_fraction {v} is invalid (must be finite in 0.0..=1.0)"
            ),
            Self::InvalidFidelityThreshold(v) => write!(
                f,
                "br-ft-43wis: fidelity_threshold {v} is invalid (must be finite in 0.0..=1.0)"
            ),
            Self::InvalidMaxConflictRate(v) => write!(
                f,
                "br-ft-43wis: max_conflict_rate {v} is invalid (must be finite in 0.0..=1.0)"
            ),
            Self::InvalidAllowlistEntry { index, reason } => match reason {
                AllowlistEntryError::EmptyOrWhitespace => write!(
                    f,
                    "br-ft-ao0i5: canary_agent_allowlist[{index}] is empty/whitespace"
                ),
                AllowlistEntryError::Duplicate { first_index } => write!(
                    f,
                    "br-ft-ao0i5: canary_agent_allowlist[{index}] duplicates index {first_index}"
                ),
            },
        }
    }
}

impl std::error::Error for CanaryConfigError {}

impl CanaryRolloutConfig {
    /// br-ft-43wis: validate numeric fields. Returns `Err` if any
    /// field is non-finite (NaN / +/-Inf) or outside its documented
    /// range. Used by the fallible
    /// [`CanaryRolloutController::try_new`] constructor; the
    /// infallible [`CanaryRolloutController::new`] panics on
    /// invalid input so the legacy callsite contract is preserved
    /// for callers who can guarantee finite inputs at the type
    /// system level.
    pub fn validate(&self) -> std::result::Result<(), CanaryConfigError> {
        if !self.canary_agent_fraction.is_finite()
            || self.canary_agent_fraction < 0.0
            || self.canary_agent_fraction > 1.0
        {
            return Err(CanaryConfigError::InvalidCanaryAgentFraction(
                self.canary_agent_fraction,
            ));
        }
        if !self.fidelity_threshold.is_finite()
            || self.fidelity_threshold < 0.0
            || self.fidelity_threshold > 1.0
        {
            return Err(CanaryConfigError::InvalidFidelityThreshold(
                self.fidelity_threshold,
            ));
        }
        if !self.max_conflict_rate.is_finite()
            || self.max_conflict_rate < 0.0
            || self.max_conflict_rate > 1.0
        {
            return Err(CanaryConfigError::InvalidMaxConflictRate(
                self.max_conflict_rate,
            ));
        }
        // br-ft-ao0i5: allowlist hygiene. Empty / blank entries collapse
        // agent_id equality checks (agent_id == "" is a degenerate
        // match for any agent that omits the field), and duplicates
        // inflate cohort size accounting. Reject both at construction.
        for (i, raw) in self.canary_agent_allowlist.iter().enumerate() {
            if raw.trim().is_empty() {
                return Err(CanaryConfigError::InvalidAllowlistEntry {
                    index: i,
                    reason: AllowlistEntryError::EmptyOrWhitespace,
                });
            }
            if let Some(j) = self.canary_agent_allowlist[..i]
                .iter()
                .position(|e| e == raw)
            {
                return Err(CanaryConfigError::InvalidAllowlistEntry {
                    index: i,
                    reason: AllowlistEntryError::Duplicate { first_index: j },
                });
            }
        }
        Ok(())
    }
}

// ── Health check types ──────────────────────────────────────────────────────

/// Result of a single health check evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryHealthCheck {
    /// Cycle ID this check corresponds to.
    pub cycle_id: u64,
    /// Timestamp of the check.
    pub timestamp_ms: i64,
    /// Whether the check passed.
    pub healthy: bool,
    /// Fidelity score from the shadow diff.
    pub fidelity_score: f64,
    /// Number of safety gate rejections.
    pub safety_rejections: usize,
    /// Conflict rate (conflicts / max(dispatches, 1)).
    pub conflict_rate: f64,
    /// Reasons the check failed (empty if healthy).
    pub failure_reasons: Vec<HealthFailureReason>,
}

/// Why a health check failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthFailureReason {
    /// Fidelity below threshold.
    LowFidelity,
    /// Too many safety gate rejections.
    ExcessiveSafetyRejections,
    /// Conflict rate too high.
    HighConflictRate,
    /// Shadow evaluator not yet warmed up.
    NotWarmedUp,
    /// Missing execution divergences detected.
    MissingExecutions,
    /// Unexpected executions detected.
    UnexpectedExecutions,
    /// br-ft-q5sn3: Shadow phase observed dispatch / executions /
    /// emissions when the phase is documented as observe-only.
    /// This is the strongest signal that the canary gate leaked
    /// or that telemetry is misattributing a live action — fail
    /// closed regardless of fidelity / conflict / rejection
    /// thresholds.
    ShadowDispatchLeak,
}

// ── Decision types ──────────────────────────────────────────────────────────

/// Decision produced by the controller after evaluating health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryDecision {
    /// Current phase after this decision.
    pub phase: CanaryPhase,
    /// Action taken.
    pub action: CanaryAction,
    /// The health check that informed this decision.
    pub health_check: CanaryHealthCheck,
    /// Phase transition record (if any).
    pub transition: Option<CanaryPhaseTransition>,
}

/// Action taken by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryAction {
    /// Stay in current phase.
    Hold,
    /// Advance to next phase.
    Advance,
    /// Roll back to a previous phase.
    Rollback,
}

/// Record of a phase transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryPhaseTransition {
    /// Phase before the transition.
    pub from: CanaryPhase,
    /// Phase after the transition.
    pub to: CanaryPhase,
    /// Cycle ID when transition occurred.
    pub cycle_id: u64,
    /// Timestamp of transition.
    pub timestamp_ms: i64,
    /// Why the transition happened.
    pub reason: String,
}

// ── Controller metrics ──────────────────────────────────────────────────────

/// Aggregate metrics from the canary controller.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CanaryMetrics {
    /// Total health checks performed.
    pub total_checks: u64,
    /// Total healthy checks.
    pub healthy_checks: u64,
    /// Total unhealthy checks.
    pub unhealthy_checks: u64,
    /// Total phase transitions.
    pub total_transitions: u64,
    /// Total rollbacks triggered.
    pub total_rollbacks: u64,
    /// Total advances performed.
    pub total_advances: u64,
    /// Current consecutive healthy streak.
    pub consecutive_healthy: u32,
    /// Current consecutive unhealthy streak.
    pub consecutive_unhealthy: u32,
    /// Maximum consecutive unhealthy streak observed.
    pub max_consecutive_unhealthy: u32,
}

// ── Controller ──────────────────────────────────────────────────────────────

/// Canary rollout controller for staged mission dispatch.
///
/// Evaluates shadow mode health signals and manages phase transitions
/// (Shadow → Canary → Full) with automatic rollback on degradation.
#[derive(Debug)]
pub struct CanaryRolloutController {
    config: CanaryRolloutConfig,
    phase: CanaryPhase,
    health_history: VecDeque<CanaryHealthCheck>,
    transition_history: VecDeque<CanaryPhaseTransition>,
    metrics: CanaryMetrics,
    /// Set of agent IDs selected for canary dispatch.
    canary_agents: HashSet<String>,
    /// br-ft-ao0i5: allowlist entries that were NOT in the most
    /// recent `update_canary_agents` available_agents slice. Empty
    /// if allowlist mode is unused or every entry intersected.
    stale_allowlist_agents: HashSet<String>,
}

impl CanaryRolloutController {
    /// Create a new controller with the given configuration.
    ///
    /// br-ft-43wis: this is the legacy infallible constructor.
    /// Callers that load `config` from external/untrusted sources
    /// (persisted JSON, IPC payload, env-var-driven overrides)
    /// should prefer [`CanaryRolloutController::try_new`] which
    /// returns `Err` instead of panicking on a non-finite or
    /// out-of-range numeric field. This constructor still calls
    /// `validate()` and panics on invalid input rather than
    /// silently allowing a malformed safety-gate threshold to
    /// reach `compute_health_check`.
    pub fn new(config: CanaryRolloutConfig) -> Self {
        if let Err(err) = config.validate() {
            panic!("CanaryRolloutController::new: {err}");
        }
        let phase = config.initial_phase;
        Self {
            config,
            phase,
            health_history: VecDeque::new(),
            transition_history: VecDeque::new(),
            metrics: CanaryMetrics::default(),
            canary_agents: HashSet::new(),
            stale_allowlist_agents: HashSet::new(),
        }
    }

    /// br-ft-43wis: fallible constructor that validates numeric
    /// fields before installing the config. Returns
    /// [`CanaryConfigError`] on NaN / +/-Inf / out-of-range
    /// `canary_agent_fraction`, `fidelity_threshold`, or
    /// `max_conflict_rate`. Use this from any code path that
    /// loads config from a non-trusted source (persisted JSON,
    /// IPC payload).
    pub fn try_new(config: CanaryRolloutConfig) -> std::result::Result<Self, CanaryConfigError> {
        config.validate()?;
        let phase = config.initial_phase;
        Ok(Self {
            config,
            phase,
            health_history: VecDeque::new(),
            transition_history: VecDeque::new(),
            metrics: CanaryMetrics::default(),
            canary_agents: HashSet::new(),
            stale_allowlist_agents: HashSet::new(),
        })
    }

    /// Create a new controller with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(CanaryRolloutConfig::default())
    }

    /// Get the current rollout phase.
    #[must_use]
    pub fn phase(&self) -> CanaryPhase {
        self.phase
    }

    /// Get the controller configuration.
    #[must_use]
    pub fn config(&self) -> &CanaryRolloutConfig {
        &self.config
    }

    /// Get the aggregate metrics.
    #[must_use]
    pub fn metrics(&self) -> &CanaryMetrics {
        &self.metrics
    }

    /// Get the health check history.
    #[must_use]
    pub fn health_history(&self) -> Vec<CanaryHealthCheck> {
        self.health_history.iter().cloned().collect()
    }

    /// Get the phase transition history.
    #[must_use]
    pub fn transition_history(&self) -> Vec<CanaryPhaseTransition> {
        self.transition_history.iter().cloned().collect()
    }

    /// Get the last health check result.
    #[must_use]
    pub fn last_health_check(&self) -> Option<&CanaryHealthCheck> {
        self.health_history.back()
    }

    /// Update the set of agents included in the canary cohort.
    ///
    /// When the allowlist in config is non-empty, it takes precedence.
    /// Otherwise, `available_agents` is sampled at `canary_agent_fraction`.
    ///
    /// br-ft-ao0i5: the allowlist branch now intersects with
    /// `available_agents` rather than blindly trusting the configured
    /// list. Stale entries (configured but not currently available)
    /// are silently dropped from the cohort but returned via
    /// [`CanaryRolloutController::stale_allowlist_agents`] so
    /// operators can audit drift between config and the live
    /// availability set. The previous behavior preserved every
    /// allowlist entry, so a `canary_agents` membership check could
    /// match an agent_id that no longer corresponds to any live
    /// agent — defeating the cohort-as-blast-radius contract.
    pub fn update_canary_agents(&mut self, available_agents: &[String]) {
        self.canary_agents.clear();
        self.stale_allowlist_agents.clear();

        if !self.config.canary_agent_allowlist.is_empty() {
            let available: HashSet<&String> = available_agents.iter().collect();
            for agent in &self.config.canary_agent_allowlist {
                if available.contains(agent) {
                    self.canary_agents.insert(agent.clone());
                } else {
                    self.stale_allowlist_agents.insert(agent.clone());
                }
            }
            return;
        }

        let count = ((available_agents.len() as f64 * self.config.canary_agent_fraction).ceil()
            as usize)
            .max(1)
            .min(available_agents.len());

        for agent in available_agents.iter().take(count) {
            self.canary_agents.insert(agent.clone());
        }
    }

    /// br-ft-ao0i5: agents listed in `canary_agent_allowlist` that
    /// were NOT present in the most recent `update_canary_agents`
    /// call's `available_agents` slice. Operator-facing audit hook
    /// so config drift is visible rather than silently widening the
    /// blast radius.
    #[must_use]
    pub fn stale_allowlist_agents(&self) -> &HashSet<String> {
        &self.stale_allowlist_agents
    }

    /// Get the current canary agent set.
    #[must_use]
    pub fn canary_agents(&self) -> &HashSet<String> {
        &self.canary_agents
    }

    // ── Health evaluation ────────────────────────────────────────────────

    /// Evaluate health from a shadow mode diff and produce a decision.
    ///
    /// This is the main entry point called after each mission cycle.
    pub fn evaluate_health(
        &mut self,
        cycle_id: u64,
        timestamp_ms: i64,
        diff: &ShadowModeDiff,
        shadow_metrics: &ShadowModeMetrics,
    ) -> CanaryDecision {
        let health_check = self.compute_health_check(cycle_id, timestamp_ms, diff, shadow_metrics);
        self.record_health_metrics(&health_check);
        let action = self.decide_action(&health_check);

        let transition = match action {
            CanaryAction::Advance => {
                let next_phase = self.phase.next().unwrap_or(self.phase);
                if next_phase != self.phase {
                    Some(self.transition_to(
                        next_phase,
                        cycle_id,
                        timestamp_ms,
                        "health_criteria_met",
                    ))
                } else {
                    None
                }
            }
            CanaryAction::Rollback => {
                let target = match self.phase {
                    CanaryPhase::Full => CanaryPhase::Canary,
                    CanaryPhase::Canary => CanaryPhase::Shadow,
                    CanaryPhase::Shadow => CanaryPhase::Shadow,
                };
                if target != self.phase {
                    Some(self.transition_to(target, cycle_id, timestamp_ms, "health_degraded"))
                } else {
                    None
                }
            }
            CanaryAction::Hold => None,
        };

        // Store health check (bounded)
        if self.health_history.len() >= 256 {
            self.health_history.pop_front();
        }
        self.health_history.push_back(health_check.clone());

        CanaryDecision {
            phase: self.phase,
            action,
            health_check,
            transition,
        }
    }

    /// Force a phase transition (operator override).
    ///
    /// Validates that the transition is legal (advance or rollback).
    /// Returns `None` if the transition is invalid.
    pub fn force_transition(
        &mut self,
        target: CanaryPhase,
        cycle_id: u64,
        timestamp_ms: i64,
        reason: &str,
    ) -> Option<CanaryPhaseTransition> {
        if target == self.phase {
            return None;
        }
        let is_advance = self.phase.can_advance_to(target);
        let is_rollback = self.phase.can_rollback_to(target);
        if !is_advance && !is_rollback {
            return None;
        }
        Some(self.transition_to(target, cycle_id, timestamp_ms, reason))
    }

    /// Reset the controller to the initial phase, clearing all history.
    pub fn reset(&mut self) {
        self.phase = self.config.initial_phase;
        self.health_history.clear();
        self.transition_history.clear();
        self.metrics = CanaryMetrics::default();
        self.canary_agents.clear();
    }

    // ── Assignment filtering ─────────────────────────────────────────────

    /// Filter an assignment set based on the current rollout phase.
    ///
    /// - **Shadow**: All assignments moved to rejected (none dispatched).
    /// - **Canary**: Only assignments to canary agents pass; rest rejected.
    /// - **Full**: All assignments pass through unchanged.
    #[must_use]
    pub fn filter_assignments(&self, assignment_set: &AssignmentSet) -> AssignmentSet {
        match self.phase {
            CanaryPhase::Shadow => {
                // Move all assignments to rejected
                let mut rejected = assignment_set.rejected.clone();
                for assignment in &assignment_set.assignments {
                    rejected.push(RejectedCandidate {
                        bead_id: assignment.bead_id.clone(),
                        score: assignment.score,
                        reasons: vec![crate::planner_features::RejectionReason::SafetyGateDenied {
                            gate_name: "canary_shadow_phase".to_string(),
                        }],
                    });
                }
                AssignmentSet {
                    assignments: Vec::new(),
                    rejected,
                    solver_config: assignment_set.solver_config.clone(),
                }
            }
            CanaryPhase::Canary => {
                let mut passed = Vec::new();
                let mut rejected = assignment_set.rejected.clone();

                for assignment in &assignment_set.assignments {
                    if self.canary_agents.contains(&assignment.agent_id) {
                        passed.push(assignment.clone());
                    } else {
                        rejected.push(RejectedCandidate {
                            bead_id: assignment.bead_id.clone(),
                            score: assignment.score,
                            reasons: vec![
                                crate::planner_features::RejectionReason::SafetyGateDenied {
                                    gate_name: "canary_agent_not_in_cohort".to_string(),
                                },
                            ],
                        });
                    }
                }

                AssignmentSet {
                    assignments: passed,
                    rejected,
                    solver_config: assignment_set.solver_config.clone(),
                }
            }
            CanaryPhase::Full => assignment_set.clone(),
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────────

    fn compute_health_check(
        &self,
        cycle_id: u64,
        timestamp_ms: i64,
        diff: &ShadowModeDiff,
        shadow_metrics: &ShadowModeMetrics,
    ) -> CanaryHealthCheck {
        let mut failure_reasons = Vec::new();

        // Check warmup
        if shadow_metrics.total_cycles < self.config.min_warmup_cycles {
            failure_reasons.push(HealthFailureReason::NotWarmedUp);
        }

        // Check fidelity. br-ft-43wis: a NaN fidelity_score makes
        // the strict-`<` comparison return `false`, silently passing
        // the gate. Treat any non-finite score as LowFidelity
        // (fail-closed) so a malformed shadow diff cannot mark a
        // bad cycle healthy.
        if !diff.fidelity_score.is_finite() || diff.fidelity_score < self.config.fidelity_threshold
        {
            failure_reasons.push(HealthFailureReason::LowFidelity);
        }

        // Check safety rejections
        if diff.safety_gate_rejections > self.config.max_safety_rejections_per_cycle {
            failure_reasons.push(HealthFailureReason::ExcessiveSafetyRejections);
        }

        // Check conflict rate. br-ft-43wis: same NaN-fail-closed
        // shape — `conflict_rate > threshold` returns false on NaN.
        // The conflict_rate value itself is computed from
        // u64-as-f64 division so it can only be NaN if denominator
        // and numerator are both 0 (handled by .max(1) above) OR
        // if a future schema change makes those fields f64. The
        // threshold side, however, can be NaN if validate() was
        // bypassed via direct field assignment on a deserialized
        // config — guard against both for defense-in-depth.
        let denominator = diff.emissions_count.max(1);
        let conflict_rate = diff.conflicts_detected as f64 / denominator as f64;
        if !conflict_rate.is_finite()
            || !self.config.max_conflict_rate.is_finite()
            || conflict_rate > self.config.max_conflict_rate
        {
            failure_reasons.push(HealthFailureReason::HighConflictRate);
        }

        // Missing executions are only meaningful in canary/full —
        // Shadow-phase has no expected executions to be missing
        // because no work was dispatched.
        if self.phase.dispatches() && !diff.missing_executions.is_empty() {
            failure_reasons.push(HealthFailureReason::MissingExecutions);
        }

        // br-ft-q5sn3: unexpected executions are ALWAYS unhealthy.
        // Pre-fix this was gated behind `self.phase.dispatches()`
        // which excluded Shadow — but unexpected executions in
        // Shadow are the strongest signal that the observe-only
        // contract was violated (canary gate leaked, telemetry is
        // misattributing live actions, or a stale prior-phase
        // dispatch slipped through). Always flag.
        if !diff.unexpected_executions.is_empty() {
            failure_reasons.push(HealthFailureReason::UnexpectedExecutions);
        }

        // br-ft-q5sn3: Shadow-phase additional defense. The
        // observe-only contract permits the shadow evaluator to
        // RECOMMEND dispatches (emissions_count > 0) — that is the
        // entire purpose of Shadow — but it MUST NOT see any
        // EXECUTION evidence. Any non-empty
        // missing_executions / unexpected_executions or any
        // execution_rejections_count > 0 in Shadow signals that
        // something actually dispatched (or believed it
        // dispatched) work despite the contract. Flag as
        // ShadowDispatchLeak so failure_reasons distinguishes
        // "Shadow leaked" from a generic UnexpectedExecutions in
        // Canary/Full.
        if !self.phase.dispatches()
            && (diff.execution_rejections_count > 0
                || !diff.missing_executions.is_empty()
                || !diff.unexpected_executions.is_empty())
        {
            failure_reasons.push(HealthFailureReason::ShadowDispatchLeak);
        }

        let healthy = failure_reasons.is_empty();

        CanaryHealthCheck {
            cycle_id,
            timestamp_ms,
            healthy,
            fidelity_score: diff.fidelity_score,
            safety_rejections: diff.safety_gate_rejections,
            conflict_rate,
            failure_reasons,
        }
    }

    fn decide_action(&self, health_check: &CanaryHealthCheck) -> CanaryAction {
        if health_check.healthy {
            if self.config.auto_advance
                && self.phase.next().is_some()
                && self.metrics.consecutive_healthy >= self.config.min_healthy_before_advance
            {
                return CanaryAction::Advance;
            }
            CanaryAction::Hold
        } else {
            if self.config.auto_rollback
                && self.phase != CanaryPhase::Shadow
                && self.metrics.consecutive_unhealthy >= self.config.max_consecutive_unhealthy
            {
                return CanaryAction::Rollback;
            }
            CanaryAction::Hold
        }
    }

    fn record_health_metrics(&mut self, health_check: &CanaryHealthCheck) {
        self.metrics.total_checks += 1;
        if health_check.healthy {
            self.metrics.healthy_checks += 1;
            self.metrics.consecutive_healthy += 1;
            self.metrics.consecutive_unhealthy = 0;
        } else {
            self.metrics.unhealthy_checks += 1;
            self.metrics.consecutive_unhealthy += 1;
            self.metrics.consecutive_healthy = 0;
            if self.metrics.consecutive_unhealthy > self.metrics.max_consecutive_unhealthy {
                self.metrics.max_consecutive_unhealthy = self.metrics.consecutive_unhealthy;
            }
        }
    }

    fn transition_to(
        &mut self,
        target: CanaryPhase,
        cycle_id: u64,
        timestamp_ms: i64,
        reason: &str,
    ) -> CanaryPhaseTransition {
        let from = self.phase;
        self.phase = target;

        let is_rollback = from.can_rollback_to(target);

        // Update metrics
        self.metrics.total_transitions += 1;
        if is_rollback {
            self.metrics.total_rollbacks += 1;
        } else {
            self.metrics.total_advances += 1;
        }

        // Reset consecutive counters on transition
        self.metrics.consecutive_healthy = 0;
        self.metrics.consecutive_unhealthy = 0;

        let transition = CanaryPhaseTransition {
            from,
            to: target,
            cycle_id,
            timestamp_ms,
            reason: reason.to_string(),
        };

        // Store transition (bounded)
        if self.transition_history.len() >= 256 {
            self.transition_history.pop_front();
        }
        self.transition_history.push_back(transition.clone());

        transition
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_features::{Assignment, SolverConfig};
    use crate::shadow_mode_evaluator::UnexpectedExecution;

    // ── Helpers ──────────────────────────────────────────────────────────

    fn make_healthy_diff(cycle_id: u64) -> ShadowModeDiff {
        ShadowModeDiff {
            cycle_id,
            timestamp_ms: cycle_id as i64 * 1000,
            recommendations_count: 3,
            rejections_count: 0,
            emissions_count: 3,
            execution_rejections_count: 0,
            missing_executions: Vec::new(),
            unexpected_executions: Vec::new(),
            agent_divergences: Vec::new(),
            score_accuracy: Vec::new(),
            low_confidence_recommendations: 0,
            safety_gate_rejections: 0,
            retry_storm_throttles: 0,
            conflicts_detected: 0,
            conflicts_auto_resolved: 0,
            dispatch_rate: 1.0,
            agent_match_rate: 1.0,
            fidelity_score: 0.95,
        }
    }

    fn unexpected_execution(
        bead_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> UnexpectedExecution {
        UnexpectedExecution {
            bead_id: bead_id.into(),
            agent_id: agent_id.into(),
            reason_code: "test-fixture".to_string(),
        }
    }

    fn make_unhealthy_diff(cycle_id: u64) -> ShadowModeDiff {
        ShadowModeDiff {
            cycle_id,
            timestamp_ms: cycle_id as i64 * 1000,
            recommendations_count: 3,
            rejections_count: 0,
            emissions_count: 1,
            execution_rejections_count: 2,
            missing_executions: vec![("b2".to_string(), "a2".to_string())],
            unexpected_executions: Vec::new(),
            agent_divergences: Vec::new(),
            score_accuracy: Vec::new(),
            low_confidence_recommendations: 0,
            safety_gate_rejections: 0,
            retry_storm_throttles: 0,
            conflicts_detected: 0,
            conflicts_auto_resolved: 0,
            dispatch_rate: 0.33,
            agent_match_rate: 1.0,
            fidelity_score: 0.3,
        }
    }

    /// Build a warmed-up metrics snapshot by running the evaluator through
    /// `cycles` perfect evaluation cycles.
    fn make_warmed_metrics(cycles: u64) -> ShadowModeMetrics {
        use crate::shadow_mode_evaluator::{ShadowEvaluationConfig, ShadowModeEvaluator};

        use crate::mission_events::{
            MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
        };

        let mut eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
            warmup_cycles: 0,
            ..Default::default()
        });
        for i in 1..=cycles {
            let recs = AssignmentSet {
                assignments: vec![
                    Assignment {
                        bead_id: format!("b{i}a"),
                        agent_id: "a1".to_string(),
                        score: 0.9,
                        rank: 1,
                    },
                    Assignment {
                        bead_id: format!("b{i}b"),
                        agent_id: "a2".to_string(),
                        score: 0.8,
                        rank: 2,
                    },
                    Assignment {
                        bead_id: format!("b{i}c"),
                        agent_id: "a3".to_string(),
                        score: 0.7,
                        rank: 3,
                    },
                ],
                rejected: Vec::new(),
                solver_config: SolverConfig::default(),
            };
            // Perfect cycle: create matching dispatch events
            let mut cycle_log = MissionEventLog::new(MissionEventLogConfig {
                max_events: 64,
                enabled: true,
            });
            for a in &recs.assignments {
                cycle_log.emit(
                    MissionEventBuilder::new(
                        MissionEventKind::AssignmentEmitted,
                        "mission.dispatch.assignment_emitted",
                    )
                    .cycle(i, i as i64 * 1000)
                    .correlation("corr")
                    .labels("ws", "track")
                    .detail_str("bead_id", &a.bead_id)
                    .detail_str("agent_id", &a.agent_id),
                );
            }
            eval.evaluate_cycle(i, i as i64 * 1000, &recs, cycle_log.events());
        }

        eval.metrics().clone()
    }

    fn make_cold_metrics() -> ShadowModeMetrics {
        // Just return default — total_cycles = 0, below any warmup threshold
        // but set to 2 to show partial warmup
        use crate::shadow_mode_evaluator::{ShadowEvaluationConfig, ShadowModeEvaluator};

        let mut eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
            warmup_cycles: 0,
            ..Default::default()
        });
        let recs = AssignmentSet {
            assignments: Vec::new(),
            rejected: Vec::new(),
            solver_config: SolverConfig::default(),
        };
        let empty: Vec<crate::mission_events::MissionEvent> = Vec::new();
        eval.evaluate_cycle(1, 1000, &recs, &empty);
        eval.evaluate_cycle(2, 2000, &recs, &empty);
        eval.metrics().clone()
    }

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

    // ── Phase state machine tests ────────────────────────────────────────

    #[test]
    fn phase_next_follows_progression() {
        assert_eq!(CanaryPhase::Shadow.next(), Some(CanaryPhase::Canary));
        assert_eq!(CanaryPhase::Canary.next(), Some(CanaryPhase::Full));
        assert_eq!(CanaryPhase::Full.next(), None);
    }

    #[test]
    fn phase_dispatches_only_in_canary_and_full() {
        assert!(!CanaryPhase::Shadow.dispatches());
        assert!(CanaryPhase::Canary.dispatches());
        assert!(CanaryPhase::Full.dispatches());
    }

    #[test]
    fn advance_transitions_are_valid() {
        assert!(CanaryPhase::Shadow.can_advance_to(CanaryPhase::Canary));
        assert!(CanaryPhase::Canary.can_advance_to(CanaryPhase::Full));
        assert!(!CanaryPhase::Full.can_advance_to(CanaryPhase::Shadow));
        assert!(!CanaryPhase::Shadow.can_advance_to(CanaryPhase::Full)); // skip not allowed
    }

    #[test]
    fn rollback_transitions_are_valid() {
        assert!(CanaryPhase::Full.can_rollback_to(CanaryPhase::Canary));
        assert!(CanaryPhase::Full.can_rollback_to(CanaryPhase::Shadow));
        assert!(CanaryPhase::Canary.can_rollback_to(CanaryPhase::Shadow));
        assert!(!CanaryPhase::Shadow.can_rollback_to(CanaryPhase::Canary));
    }

    // ── Controller construction ──────────────────────────────────────────

    #[test]
    fn default_starts_in_shadow() {
        let ctrl = CanaryRolloutController::with_defaults();
        assert_eq!(ctrl.phase(), CanaryPhase::Shadow);
        assert!(ctrl.health_history().is_empty());
        assert!(ctrl.transition_history().is_empty());
    }

    #[test]
    fn custom_initial_phase() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            ..Default::default()
        };
        let ctrl = CanaryRolloutController::new(config);
        assert_eq!(ctrl.phase(), CanaryPhase::Canary);
    }

    // ── Health check computation ─────────────────────────────────────────

    #[test]
    fn healthy_diff_produces_healthy_check() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let diff = make_healthy_diff(1);
        let metrics = make_warmed_metrics(10);

        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);

        assert!(decision.health_check.healthy);
        assert!(decision.health_check.failure_reasons.is_empty());
    }

    #[test]
    fn low_fidelity_makes_unhealthy() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let mut diff = make_healthy_diff(1);
        diff.fidelity_score = 0.5; // below default threshold of 0.7
        let metrics = make_warmed_metrics(10);

        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);

        assert!(!decision.health_check.healthy);
        assert!(
            decision
                .health_check
                .failure_reasons
                .contains(&HealthFailureReason::LowFidelity)
        );
    }

    #[test]
    fn cold_shadow_metrics_not_warmed_up() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let diff = make_healthy_diff(1);
        let metrics = make_cold_metrics(); // only 2 cycles, need 5

        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);

        assert!(!decision.health_check.healthy);
        assert!(
            decision
                .health_check
                .failure_reasons
                .contains(&HealthFailureReason::NotWarmedUp)
        );
    }

    #[test]
    fn excessive_safety_rejections_unhealthy() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let mut diff = make_healthy_diff(1);
        diff.safety_gate_rejections = 10; // above default max of 5
        let metrics = make_warmed_metrics(10);

        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);

        assert!(!decision.health_check.healthy);
        assert!(
            decision
                .health_check
                .failure_reasons
                .contains(&HealthFailureReason::ExcessiveSafetyRejections)
        );
    }

    #[test]
    fn high_conflict_rate_unhealthy() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let mut diff = make_healthy_diff(1);
        diff.conflicts_detected = 3;
        diff.emissions_count = 3; // rate = 1.0, above 0.3
        let metrics = make_warmed_metrics(10);

        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);

        assert!(!decision.health_check.healthy);
        assert!(
            decision
                .health_check
                .failure_reasons
                .contains(&HealthFailureReason::HighConflictRate)
        );
    }

    // ── Auto-advance tests ───────────────────────────────────────────────

    #[test]
    fn advances_after_min_healthy_checks() {
        let config = CanaryRolloutConfig {
            min_healthy_before_advance: 3,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Cycle 1-2: hold
        for i in 1..=2 {
            let diff = make_healthy_diff(i);
            let decision = ctrl.evaluate_health(i, i as i64 * 1000, &diff, &metrics);
            assert_eq!(decision.action, CanaryAction::Hold);
            assert_eq!(decision.phase, CanaryPhase::Shadow);
        }

        // Cycle 3: advance to Canary
        let diff = make_healthy_diff(3);
        let decision = ctrl.evaluate_health(3, 3000, &diff, &metrics);
        assert_eq!(decision.action, CanaryAction::Advance);
        assert_eq!(decision.phase, CanaryPhase::Canary);
        assert!(decision.transition.is_some());
        let trans = decision.transition.unwrap();
        assert_eq!(trans.from, CanaryPhase::Shadow);
        assert_eq!(trans.to, CanaryPhase::Canary);
    }

    #[test]
    fn advances_through_all_phases() {
        let config = CanaryRolloutConfig {
            min_healthy_before_advance: 2,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Shadow → Canary
        for i in 1..=2 {
            ctrl.evaluate_health(i, i as i64 * 1000, &make_healthy_diff(i), &metrics);
        }
        assert_eq!(ctrl.phase(), CanaryPhase::Canary);

        // The transition-triggering Shadow check must not count as
        // the first Canary healthy check.
        let decision = ctrl.evaluate_health(3, 3000, &make_healthy_diff(3), &metrics);
        assert_eq!(decision.action, CanaryAction::Hold);
        assert_eq!(decision.phase, CanaryPhase::Canary);
        assert_eq!(ctrl.phase(), CanaryPhase::Canary);

        // Canary → Full only after two Canary-local healthy checks.
        let decision = ctrl.evaluate_health(4, 4000, &make_healthy_diff(4), &metrics);
        assert_eq!(decision.action, CanaryAction::Advance);
        assert_eq!(decision.phase, CanaryPhase::Full);
        assert_eq!(ctrl.phase(), CanaryPhase::Full);

        // Full stays Full
        let decision = ctrl.evaluate_health(5, 5000, &make_healthy_diff(5), &metrics);
        assert_eq!(decision.action, CanaryAction::Hold);
        assert_eq!(decision.phase, CanaryPhase::Full);
    }

    #[test]
    fn healthy_streak_does_not_carry_across_phase_transition_ft_xm4eb() {
        // br-ft-xm4eb: pin the explicit cycle-by-cycle contract the
        // audit asked for. With min_healthy_before_advance=2:
        //   * cycle 1 holds in Shadow (ch=1)
        //   * cycle 2 advances Shadow→Canary (ch=2 then reset)
        //   * cycle 3 must hold in Canary (ch=1, NOT ch=2)
        //   * cycle 4 advances Canary→Full (ch=2)
        //
        // This test was filed to assert phase dwell time is honored:
        // the trigger check is consumed by the OLD phase and the new
        // phase starts the streak at zero. The current order in
        // evaluate_health (record_health_metrics → decide_action →
        // transition_to) achieves this, but a future refactor that
        // swaps the order would silently shorten dwell.
        let config = CanaryRolloutConfig {
            min_healthy_before_advance: 2,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Cycle 1: Shadow, healthy → Hold.
        let d = ctrl.evaluate_health(1, 1000, &make_healthy_diff(1), &metrics);
        assert_eq!(d.action, CanaryAction::Hold);
        assert_eq!(d.phase, CanaryPhase::Shadow);
        assert_eq!(ctrl.metrics().consecutive_healthy, 1);

        // Cycle 2: Shadow, healthy → Advance to Canary, streak reset.
        let d = ctrl.evaluate_health(2, 2000, &make_healthy_diff(2), &metrics);
        assert_eq!(d.action, CanaryAction::Advance);
        assert_eq!(d.phase, CanaryPhase::Canary);
        assert_eq!(
            ctrl.metrics().consecutive_healthy,
            0,
            "transition must reset streak; check that triggered the transition belongs to the OLD phase"
        );

        // Cycle 3: Canary, healthy → Hold (ch=1, not yet 2).
        let d = ctrl.evaluate_health(3, 3000, &make_healthy_diff(3), &metrics);
        assert_eq!(
            d.action,
            CanaryAction::Hold,
            "Canary must dwell for a full streak before advancing; carry-over would advance here"
        );
        assert_eq!(d.phase, CanaryPhase::Canary);
        assert_eq!(ctrl.metrics().consecutive_healthy, 1);

        // Cycle 4: Canary, healthy → Advance to Full.
        let d = ctrl.evaluate_health(4, 4000, &make_healthy_diff(4), &metrics);
        assert_eq!(d.action, CanaryAction::Advance);
        assert_eq!(d.phase, CanaryPhase::Full);
        assert_eq!(ctrl.metrics().consecutive_healthy, 0);
    }

    #[test]
    fn rollback_streak_does_not_carry_into_canary() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Full,
            max_consecutive_unhealthy: 2,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        let decision = ctrl.evaluate_health(1, 1000, &make_unhealthy_diff(1), &metrics);
        assert_eq!(decision.action, CanaryAction::Hold);
        assert_eq!(decision.phase, CanaryPhase::Full);

        let decision = ctrl.evaluate_health(2, 2000, &make_unhealthy_diff(2), &metrics);
        assert_eq!(decision.action, CanaryAction::Rollback);
        assert_eq!(decision.phase, CanaryPhase::Canary);
        assert_eq!(ctrl.metrics().consecutive_unhealthy, 0);

        let decision = ctrl.evaluate_health(3, 3000, &make_unhealthy_diff(3), &metrics);
        assert_eq!(decision.action, CanaryAction::Hold);
        assert_eq!(decision.phase, CanaryPhase::Canary);
        assert_eq!(ctrl.phase(), CanaryPhase::Canary);
    }

    proptest::proptest! {
        #[test]
        fn phase_transition_streaks_are_phase_local_ft_o09r7(
            healthy_threshold in 1u32..=4,
        ) {
            let config = CanaryRolloutConfig {
                min_healthy_before_advance: healthy_threshold,
                min_warmup_cycles: 0,
                ..Default::default()
            };
            let mut ctrl = CanaryRolloutController::new(config);
            let metrics = make_warmed_metrics(10);
            let mut cycle = 1u64;

            for _ in 0..healthy_threshold {
                ctrl.evaluate_health(cycle, cycle as i64 * 1000, &make_healthy_diff(cycle), &metrics);
                cycle += 1;
            }
            proptest::prop_assert_eq!(ctrl.phase(), CanaryPhase::Canary);
            proptest::prop_assert_eq!(ctrl.metrics().consecutive_healthy, 0);

            for _ in 1..healthy_threshold {
                let decision = ctrl.evaluate_health(
                    cycle,
                    cycle as i64 * 1000,
                    &make_healthy_diff(cycle),
                    &metrics,
                );
                proptest::prop_assert_eq!(decision.action, CanaryAction::Hold);
                proptest::prop_assert_eq!(decision.phase, CanaryPhase::Canary);
                cycle += 1;
            }

            let decision = ctrl.evaluate_health(
                cycle,
                cycle as i64 * 1000,
                &make_healthy_diff(cycle),
                &metrics,
            );
            proptest::prop_assert_eq!(decision.action, CanaryAction::Advance);
            proptest::prop_assert_eq!(decision.phase, CanaryPhase::Full);
        }
    }

    #[test]
    fn no_advance_when_disabled() {
        let config = CanaryRolloutConfig {
            auto_advance: false,
            min_healthy_before_advance: 1,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=5 {
            let diff = make_healthy_diff(i);
            let decision = ctrl.evaluate_health(i, i as i64 * 1000, &diff, &metrics);
            assert_eq!(decision.action, CanaryAction::Hold);
        }
        assert_eq!(ctrl.phase(), CanaryPhase::Shadow);
    }

    // ── Auto-rollback tests ──────────────────────────────────────────────

    #[test]
    fn rollback_after_consecutive_unhealthy() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            max_consecutive_unhealthy: 2,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Unhealthy 1: hold
        let diff1 = make_unhealthy_diff(1);
        let d1 = ctrl.evaluate_health(1, 1000, &diff1, &metrics);
        assert_eq!(d1.action, CanaryAction::Hold);

        // Unhealthy 2: rollback
        let diff2 = make_unhealthy_diff(2);
        let d2 = ctrl.evaluate_health(2, 2000, &diff2, &metrics);
        assert_eq!(d2.action, CanaryAction::Rollback);
        assert_eq!(d2.phase, CanaryPhase::Shadow);

        let trans = d2.transition.unwrap();
        assert_eq!(trans.from, CanaryPhase::Canary);
        assert_eq!(trans.to, CanaryPhase::Shadow);
    }

    #[test]
    fn rollback_from_full_goes_to_canary() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Full,
            max_consecutive_unhealthy: 1,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        let diff = make_unhealthy_diff(1);
        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);

        assert_eq!(decision.action, CanaryAction::Rollback);
        assert_eq!(decision.phase, CanaryPhase::Canary);
    }

    #[test]
    fn no_rollback_when_disabled() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            auto_rollback: false,
            max_consecutive_unhealthy: 1,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=5 {
            let diff = make_unhealthy_diff(i);
            let decision = ctrl.evaluate_health(i, i as i64 * 1000, &diff, &metrics);
            assert_eq!(decision.action, CanaryAction::Hold);
        }
        assert_eq!(ctrl.phase(), CanaryPhase::Canary);
    }

    #[test]
    fn no_rollback_from_shadow() {
        let config = CanaryRolloutConfig {
            max_consecutive_unhealthy: 1,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        let diff = make_unhealthy_diff(1);
        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);

        // Can't rollback from shadow — holds
        assert_eq!(decision.action, CanaryAction::Hold);
        assert_eq!(decision.phase, CanaryPhase::Shadow);
    }

    #[test]
    fn healthy_check_resets_unhealthy_streak() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            max_consecutive_unhealthy: 3,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // 2 unhealthy
        ctrl.evaluate_health(1, 1000, &make_unhealthy_diff(1), &metrics);
        ctrl.evaluate_health(2, 2000, &make_unhealthy_diff(2), &metrics);
        assert_eq!(ctrl.metrics().consecutive_unhealthy, 2);

        // 1 healthy resets
        ctrl.evaluate_health(3, 3000, &make_healthy_diff(3), &metrics);
        assert_eq!(ctrl.metrics().consecutive_unhealthy, 0);
        assert_eq!(ctrl.metrics().consecutive_healthy, 1);

        // 2 more unhealthy — no rollback yet (streak reset)
        ctrl.evaluate_health(4, 4000, &make_unhealthy_diff(4), &metrics);
        ctrl.evaluate_health(5, 5000, &make_unhealthy_diff(5), &metrics);
        assert_eq!(ctrl.phase(), CanaryPhase::Canary); // not rolled back
    }

    // ── Force transition tests ───────────────────────────────────────────

    #[test]
    fn force_advance() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let trans = ctrl.force_transition(CanaryPhase::Canary, 1, 1000, "operator_override");

        assert!(trans.is_some());
        let t = trans.unwrap();
        assert_eq!(t.from, CanaryPhase::Shadow);
        assert_eq!(t.to, CanaryPhase::Canary);
        assert_eq!(ctrl.phase(), CanaryPhase::Canary);
    }

    #[test]
    fn force_rollback() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Full,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let trans = ctrl.force_transition(CanaryPhase::Shadow, 1, 1000, "emergency_stop");

        assert!(trans.is_some());
        assert_eq!(ctrl.phase(), CanaryPhase::Shadow);
    }

    #[test]
    fn force_invalid_transition_rejected() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        // Skip: Shadow → Full is not valid
        let result = ctrl.force_transition(CanaryPhase::Full, 1, 1000, "invalid");
        assert!(result.is_none());
        assert_eq!(ctrl.phase(), CanaryPhase::Shadow);
    }

    #[test]
    fn force_same_phase_no_op() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let result = ctrl.force_transition(CanaryPhase::Shadow, 1, 1000, "no_change");
        assert!(result.is_none());
    }

    // ── Assignment filtering tests ───────────────────────────────────────

    #[test]
    fn shadow_blocks_all_assignments() {
        let ctrl = CanaryRolloutController::with_defaults();
        let set = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.8, 2),
        ]);

        let filtered = ctrl.filter_assignments(&set);

        assert!(filtered.assignments.is_empty());
        assert_eq!(filtered.rejected.len(), 2);
    }

    #[test]
    fn canary_filters_to_canary_agents() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            canary_agent_allowlist: vec!["a1".to_string()],
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&["a1".to_string(), "a2".to_string(), "a3".to_string()]);

        let set = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.8, 2),
            make_assignment("b3", "a3", 0.7, 3),
        ]);

        let filtered = ctrl.filter_assignments(&set);

        assert_eq!(filtered.assignments.len(), 1);
        assert_eq!(filtered.assignments[0].agent_id, "a1");
        assert_eq!(filtered.rejected.len(), 2);
    }

    #[test]
    fn full_passes_all_assignments() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Full,
            ..Default::default()
        };
        let ctrl = CanaryRolloutController::new(config);
        let set = make_assignment_set(vec![
            make_assignment("b1", "a1", 0.9, 1),
            make_assignment("b2", "a2", 0.8, 2),
        ]);

        let filtered = ctrl.filter_assignments(&set);

        assert_eq!(filtered.assignments.len(), 2);
        assert!(filtered.rejected.is_empty());
    }

    // ── Canary agent selection tests ─────────────────────────────────────

    #[test]
    fn canary_agents_from_fraction() {
        let config = CanaryRolloutConfig {
            canary_agent_fraction: 0.5,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&[
            "a1".to_string(),
            "a2".to_string(),
            "a3".to_string(),
            "a4".to_string(),
        ]);

        assert_eq!(ctrl.canary_agents().len(), 2);
    }

    #[test]
    fn canary_agents_minimum_one() {
        let config = CanaryRolloutConfig {
            canary_agent_fraction: 0.01, // very small
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&["a1".to_string(), "a2".to_string()]);

        assert_eq!(ctrl.canary_agents().len(), 1);
    }

    #[test]
    fn allowlist_overrides_fraction() {
        let config = CanaryRolloutConfig {
            canary_agent_fraction: 0.1,
            canary_agent_allowlist: vec!["a2".to_string(), "a3".to_string()],
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&["a1".to_string(), "a2".to_string(), "a3".to_string()]);

        assert_eq!(ctrl.canary_agents().len(), 2);
        assert!(ctrl.canary_agents().contains("a2"));
        assert!(ctrl.canary_agents().contains("a3"));
    }

    // ── br-ft-ao0i5: allowlist hygiene + intersection ───────────────────

    #[test]
    fn allowlist_empty_entry_rejected() {
        let config = CanaryRolloutConfig {
            canary_agent_allowlist: vec!["a1".to_string(), String::new()],
            ..Default::default()
        };
        let err = CanaryRolloutController::try_new(config).expect_err("empty entry must reject");
        assert!(matches!(
            err,
            CanaryConfigError::InvalidAllowlistEntry {
                index: 1,
                reason: AllowlistEntryError::EmptyOrWhitespace,
            }
        ));
        assert!(err.to_string().contains("br-ft-ao0i5"));
    }

    #[test]
    fn allowlist_whitespace_entry_rejected() {
        let config = CanaryRolloutConfig {
            canary_agent_allowlist: vec!["   ".to_string()],
            ..Default::default()
        };
        let err = CanaryRolloutController::try_new(config).expect_err("whitespace must reject");
        assert!(matches!(
            err,
            CanaryConfigError::InvalidAllowlistEntry {
                index: 0,
                reason: AllowlistEntryError::EmptyOrWhitespace,
            }
        ));
    }

    #[test]
    fn allowlist_duplicate_entry_rejected() {
        let config = CanaryRolloutConfig {
            canary_agent_allowlist: vec!["a1".to_string(), "a2".to_string(), "a1".to_string()],
            ..Default::default()
        };
        let err = CanaryRolloutController::try_new(config).expect_err("duplicate must reject");
        match err {
            CanaryConfigError::InvalidAllowlistEntry {
                index: 2,
                reason: AllowlistEntryError::Duplicate { first_index: 0 },
            } => {}
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn allowlist_intersects_with_available_agents() {
        // br-ft-ao0i5: previously every allowlist entry was inserted
        // into canary_agents regardless of whether it was a live
        // agent. Now stale entries are dropped and surfaced via
        // stale_allowlist_agents() so operators can spot drift.
        let config = CanaryRolloutConfig {
            canary_agent_allowlist: vec!["a1".to_string(), "a2".to_string(), "ghost".to_string()],
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&["a1".to_string(), "a2".to_string(), "a3".to_string()]);

        // Live entries are in canary_agents.
        assert_eq!(ctrl.canary_agents().len(), 2);
        assert!(ctrl.canary_agents().contains("a1"));
        assert!(ctrl.canary_agents().contains("a2"));
        assert!(!ctrl.canary_agents().contains("ghost"));

        // Stale entry is surfaced for operator audit.
        assert_eq!(ctrl.stale_allowlist_agents().len(), 1);
        assert!(ctrl.stale_allowlist_agents().contains("ghost"));
    }

    #[test]
    fn allowlist_intersection_clears_on_subsequent_call() {
        // Re-running update_canary_agents must reset stale set to
        // reflect the current available_agents call's drift.
        let config = CanaryRolloutConfig {
            canary_agent_allowlist: vec!["a1".to_string(), "ghost".to_string()],
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&["a1".to_string(), "a2".to_string()]);
        assert!(ctrl.stale_allowlist_agents().contains("ghost"));

        // Second call: ghost still missing → still stale.
        ctrl.update_canary_agents(&["a1".to_string(), "a3".to_string()]);
        assert_eq!(ctrl.stale_allowlist_agents().len(), 1);
        assert!(ctrl.stale_allowlist_agents().contains("ghost"));

        // Third call: ghost finally appears → no longer stale.
        ctrl.update_canary_agents(&["a1".to_string(), "ghost".to_string()]);
        assert_eq!(ctrl.stale_allowlist_agents().len(), 0);
        assert!(ctrl.canary_agents().contains("ghost"));
    }

    #[test]
    fn fraction_mode_does_not_populate_stale_allowlist() {
        // No allowlist configured → fraction-based selection. Stale
        // set must be empty regardless of available_agents content.
        let config = CanaryRolloutConfig {
            canary_agent_fraction: 0.5,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&["a1".to_string(), "a2".to_string()]);
        assert_eq!(ctrl.stale_allowlist_agents().len(), 0);
    }

    #[test]
    fn validate_passes_clean_allowlist() {
        // Sanity: a properly-formed allowlist must validate.
        let config = CanaryRolloutConfig {
            canary_agent_allowlist: vec!["a1".to_string(), "a2".to_string(), "a3".to_string()],
            ..Default::default()
        };
        config
            .validate()
            .expect("clean allowlist must pass validate");
    }

    // ── Metrics tests ────────────────────────────────────────────────────

    #[test]
    fn metrics_accumulate_correctly() {
        let config = CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 100, // prevent advance
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        ctrl.evaluate_health(1, 1000, &make_healthy_diff(1), &metrics);
        ctrl.evaluate_health(2, 2000, &make_healthy_diff(2), &metrics);
        ctrl.evaluate_health(3, 3000, &make_unhealthy_diff(3), &metrics);

        assert_eq!(ctrl.metrics().total_checks, 3);
        assert_eq!(ctrl.metrics().healthy_checks, 2);
        assert_eq!(ctrl.metrics().unhealthy_checks, 1);
        assert_eq!(ctrl.metrics().consecutive_healthy, 0);
        assert_eq!(ctrl.metrics().consecutive_unhealthy, 1);
    }

    #[test]
    fn transition_metrics_tracked() {
        let config = CanaryRolloutConfig {
            min_healthy_before_advance: 1,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Advance Shadow → Canary
        ctrl.evaluate_health(1, 1000, &make_healthy_diff(1), &metrics);
        assert_eq!(ctrl.metrics().total_transitions, 1);
        assert_eq!(ctrl.metrics().total_advances, 1);
        assert_eq!(ctrl.metrics().total_rollbacks, 0);
    }

    // ── History tests ────────────────────────────────────────────────────

    #[test]
    fn health_history_accumulates() {
        let config = CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 100,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=5 {
            ctrl.evaluate_health(i, i as i64 * 1000, &make_healthy_diff(i), &metrics);
        }

        assert_eq!(ctrl.health_history().len(), 5);
        assert_eq!(ctrl.last_health_check().unwrap().cycle_id, 5);
    }

    #[test]
    fn transition_history_records() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        ctrl.force_transition(CanaryPhase::Canary, 1, 1000, "test");
        ctrl.force_transition(CanaryPhase::Full, 2, 2000, "test2");

        assert_eq!(ctrl.transition_history().len(), 2);
        assert_eq!(ctrl.transition_history()[0].from, CanaryPhase::Shadow);
        assert_eq!(ctrl.transition_history()[1].from, CanaryPhase::Canary);
    }

    // ── Reset tests ──────────────────────────────────────────────────────

    #[test]
    fn reset_clears_all_state() {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Shadow,
            min_warmup_cycles: 0,
            min_healthy_before_advance: 1,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        ctrl.evaluate_health(1, 1000, &make_healthy_diff(1), &metrics);
        assert_eq!(ctrl.phase(), CanaryPhase::Canary);
        assert!(!ctrl.health_history().is_empty());

        ctrl.reset();

        assert_eq!(ctrl.phase(), CanaryPhase::Shadow);
        assert!(ctrl.health_history().is_empty());
        assert!(ctrl.transition_history().is_empty());
        assert_eq!(ctrl.metrics().total_checks, 0);
    }

    // ── Serde roundtrip tests ────────────────────────────────────────────

    #[test]
    fn config_serde_roundtrip() {
        let config = CanaryRolloutConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: CanaryRolloutConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.initial_phase, config.initial_phase);
        assert!((restored.canary_agent_fraction - config.canary_agent_fraction).abs() < 1e-10);
        assert!((restored.fidelity_threshold - config.fidelity_threshold).abs() < 1e-10);
    }

    #[test]
    fn health_check_serde_roundtrip() {
        let check = CanaryHealthCheck {
            cycle_id: 42,
            timestamp_ms: 42000,
            healthy: false,
            fidelity_score: 0.5,
            safety_rejections: 3,
            conflict_rate: 0.25,
            failure_reasons: vec![HealthFailureReason::LowFidelity],
        };
        let json = serde_json::to_string(&check).unwrap();
        let restored: CanaryHealthCheck = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.cycle_id, 42);
        assert!(!restored.healthy);
        assert_eq!(restored.failure_reasons.len(), 1);
    }

    #[test]
    fn phase_transition_serde_roundtrip() {
        let trans = CanaryPhaseTransition {
            from: CanaryPhase::Shadow,
            to: CanaryPhase::Canary,
            cycle_id: 10,
            timestamp_ms: 10000,
            reason: "test".to_string(),
        };
        let json = serde_json::to_string(&trans).unwrap();
        let restored: CanaryPhaseTransition = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.from, CanaryPhase::Shadow);
        assert_eq!(restored.to, CanaryPhase::Canary);
    }

    #[test]
    fn metrics_serde_roundtrip() {
        let metrics = CanaryMetrics {
            total_checks: 10,
            healthy_checks: 8,
            unhealthy_checks: 2,
            total_transitions: 2,
            total_rollbacks: 1,
            total_advances: 1,
            consecutive_healthy: 3,
            consecutive_unhealthy: 0,
            max_consecutive_unhealthy: 2,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let restored: CanaryMetrics = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.total_checks, 10);
        assert_eq!(restored.total_rollbacks, 1);
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn zero_emissions_doesnt_panic() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        let diff = ShadowModeDiff {
            cycle_id: 1,
            timestamp_ms: 1000,
            recommendations_count: 0,
            rejections_count: 0,
            emissions_count: 0,
            execution_rejections_count: 0,
            missing_executions: Vec::new(),
            unexpected_executions: Vec::new(),
            agent_divergences: Vec::new(),
            score_accuracy: Vec::new(),
            low_confidence_recommendations: 0,
            safety_gate_rejections: 0,
            retry_storm_throttles: 0,
            conflicts_detected: 0,
            conflicts_auto_resolved: 0,
            dispatch_rate: 1.0,
            agent_match_rate: 1.0,
            fidelity_score: 1.0,
        };
        let metrics = make_warmed_metrics(10);

        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);
        assert!(decision.health_check.healthy);
    }

    #[test]
    fn empty_agent_list_produces_empty_canary_set() {
        let mut ctrl = CanaryRolloutController::with_defaults();
        ctrl.update_canary_agents(&[]);
        assert!(ctrl.canary_agents().is_empty());
    }

    #[test]
    fn filter_with_empty_assignment_set() {
        let ctrl = CanaryRolloutController::with_defaults();
        let set = make_assignment_set(Vec::new());
        let filtered = ctrl.filter_assignments(&set);
        assert!(filtered.assignments.is_empty());
        assert!(filtered.rejected.is_empty());
    }

    #[test]
    fn decision_serde_roundtrip() {
        let decision = CanaryDecision {
            phase: CanaryPhase::Canary,
            action: CanaryAction::Advance,
            health_check: CanaryHealthCheck {
                cycle_id: 1,
                timestamp_ms: 1000,
                healthy: true,
                fidelity_score: 0.95,
                safety_rejections: 0,
                conflict_rate: 0.0,
                failure_reasons: Vec::new(),
            },
            transition: Some(CanaryPhaseTransition {
                from: CanaryPhase::Shadow,
                to: CanaryPhase::Canary,
                cycle_id: 1,
                timestamp_ms: 1000,
                reason: "health_criteria_met".to_string(),
            }),
        };
        let json = serde_json::to_string(&decision).unwrap();
        let restored: CanaryDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.phase, CanaryPhase::Canary);
        assert_eq!(restored.action, CanaryAction::Advance);
    }

    // ── br-ft-43wis: NaN/Inf rejection + fail-closed health gate ─────────

    #[test]
    fn validate_rejects_nan_canary_agent_fraction_ft_43wis() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.canary_agent_fraction = f64::NAN;
        let err = cfg.validate().expect_err("NaN must be rejected");
        assert!(matches!(
            err,
            CanaryConfigError::InvalidCanaryAgentFraction(_)
        ));
    }

    #[test]
    fn validate_rejects_negative_canary_agent_fraction_ft_43wis() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.canary_agent_fraction = -0.1;
        assert!(matches!(
            cfg.validate(),
            Err(CanaryConfigError::InvalidCanaryAgentFraction(_))
        ));
    }

    #[test]
    fn validate_rejects_canary_agent_fraction_above_one_ft_43wis() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.canary_agent_fraction = 1.5;
        assert!(matches!(
            cfg.validate(),
            Err(CanaryConfigError::InvalidCanaryAgentFraction(_))
        ));
    }

    #[test]
    fn validate_rejects_inf_fidelity_threshold_ft_43wis() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.fidelity_threshold = f64::INFINITY;
        assert!(matches!(
            cfg.validate(),
            Err(CanaryConfigError::InvalidFidelityThreshold(_))
        ));
    }

    #[test]
    fn validate_rejects_nan_max_conflict_rate_ft_43wis() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.max_conflict_rate = f64::NAN;
        assert!(matches!(
            cfg.validate(),
            Err(CanaryConfigError::InvalidMaxConflictRate(_))
        ));
    }

    #[test]
    fn validate_rejects_max_conflict_rate_above_one_ft_7a9pd() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.max_conflict_rate = 1.000_001;
        assert!(matches!(
            cfg.validate(),
            Err(CanaryConfigError::InvalidMaxConflictRate(_))
        ));
    }

    #[test]
    fn validate_accepts_default_config_ft_43wis() {
        let cfg = CanaryRolloutConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn try_new_returns_err_on_invalid_config_ft_43wis() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.fidelity_threshold = f64::NAN;
        let err = CanaryRolloutController::try_new(cfg).expect_err("must reject");
        assert!(matches!(
            err,
            CanaryConfigError::InvalidFidelityThreshold(_)
        ));
    }

    #[test]
    fn try_new_succeeds_on_valid_config_ft_43wis() {
        let cfg = CanaryRolloutConfig::default();
        assert!(CanaryRolloutController::try_new(cfg).is_ok());
    }

    #[test]
    #[should_panic(expected = "br-ft-43wis")]
    fn new_panics_on_invalid_config_ft_43wis() {
        let mut cfg = CanaryRolloutConfig::default();
        cfg.canary_agent_fraction = f64::NAN;
        let _ = CanaryRolloutController::new(cfg);
    }

    #[test]
    fn compute_health_check_treats_nan_fidelity_as_unhealthy_ft_43wis() {
        let controller = CanaryRolloutController::with_defaults();
        let mut diff = make_healthy_diff(1);
        diff.fidelity_score = f64::NAN;
        let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
        let check = controller.compute_health_check(1, 1000, &diff, &metrics);
        assert!(
            !check.healthy,
            "br-ft-43wis: NaN fidelity_score must mark cycle unhealthy"
        );
        assert!(
            check
                .failure_reasons
                .contains(&HealthFailureReason::LowFidelity),
            "br-ft-43wis: NaN fidelity_score must trigger LowFidelity"
        );
    }

    #[test]
    fn compute_health_check_treats_inf_fidelity_as_unhealthy_ft_43wis() {
        let controller = CanaryRolloutController::with_defaults();
        let mut diff = make_healthy_diff(1);
        diff.fidelity_score = f64::NEG_INFINITY;
        let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
        let check = controller.compute_health_check(1, 1000, &diff, &metrics);
        assert!(
            check
                .failure_reasons
                .contains(&HealthFailureReason::LowFidelity)
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,
            ..proptest::test_runner::Config::default()
        })]

        /// br-ft-43wis: for any combination of NaN/Inf/in-range
        /// fidelity_score, compute_health_check MUST flag
        /// LowFidelity unless score >= threshold AND score is
        /// finite. Pins fail-closed against fuzzed input.
        #[test]
        fn compute_health_check_fail_closed_on_nonfinite_fidelity_ft_43wis(
            score in proptest::prop_oneof![
                proptest::strategy::Just(f64::NAN),
                proptest::strategy::Just(f64::INFINITY),
                proptest::strategy::Just(f64::NEG_INFINITY),
                0.0_f64..=1.0_f64,
            ],
            threshold in 0.0_f64..=1.0_f64,
        ) {
            let mut cfg = CanaryRolloutConfig::default();
            cfg.fidelity_threshold = threshold;
            let controller = CanaryRolloutController::new(cfg);
            let mut diff = make_healthy_diff(1);
            diff.fidelity_score = score;
            let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
            let check = controller.compute_health_check(1, 1000, &diff, &metrics);
            let should_be_low = !score.is_finite() || score < threshold;
            proptest::prop_assert_eq!(
                check.failure_reasons.contains(&HealthFailureReason::LowFidelity),
                should_be_low,
                "br-ft-43wis: LowFidelity must fire iff score is non-finite or < threshold; score={} threshold={}",
                score, threshold
            );
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,
            ..proptest::test_runner::Config::default()
        })]

        /// ft-7a9pd: the canary conflict threshold is a rate gate.
        /// Non-finite, negative, and above-one values must fail at
        /// construction instead of allowing malformed config to
        /// mark high-conflict health checks as healthy.
        #[test]
        fn validate_rejects_invalid_conflict_rate_thresholds_ft_7a9pd(
            max_conflict_rate in proptest::prop_oneof![
                proptest::strategy::Just(f64::NAN),
                proptest::strategy::Just(f64::INFINITY),
                proptest::strategy::Just(f64::NEG_INFINITY),
                -10_000.0_f64..0.0_f64,
                1.000_000_1_f64..10_000.0_f64,
            ],
        ) {
            let mut cfg = CanaryRolloutConfig::default();
            cfg.max_conflict_rate = max_conflict_rate;

            proptest::prop_assert!(matches!(
                cfg.validate(),
                Err(CanaryConfigError::InvalidMaxConflictRate(_))
            ));
            proptest::prop_assert!(CanaryRolloutController::try_new(cfg).is_err());
        }
    }

    // ── br-ft-q5sn3: Shadow-phase unexpected-execution detection ─────────

    #[test]
    fn shadow_phase_flags_unexpected_executions_as_unhealthy_ft_q5sn3() {
        // Pre-fix: in Shadow phase compute_health_check skipped the
        // unexpected_executions check entirely (gated on
        // self.phase.dispatches() which is false for Shadow). A
        // Shadow cycle with unexpected_executions could pass health
        // and let auto_advance push to Canary on top of evidence
        // that Shadow violated its observe-only contract.
        let controller = CanaryRolloutController::with_defaults();
        assert_eq!(controller.phase(), CanaryPhase::Shadow);

        let mut diff = make_healthy_diff(1);
        diff.unexpected_executions = vec![unexpected_execution("bead-x", "agent-x")];
        let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
        let check = controller.compute_health_check(1, 1000, &diff, &metrics);
        assert!(
            !check.healthy,
            "br-ft-q5sn3: Shadow + unexpected_executions must mark unhealthy"
        );
        assert!(
            check
                .failure_reasons
                .contains(&HealthFailureReason::UnexpectedExecutions),
            "br-ft-q5sn3: UnexpectedExecutions reason must fire in Shadow"
        );
        assert!(
            check
                .failure_reasons
                .contains(&HealthFailureReason::ShadowDispatchLeak),
            "br-ft-q5sn3: ShadowDispatchLeak must fire when Shadow sees unexpected executions"
        );
    }

    #[test]
    fn shadow_phase_flags_execution_rejections_as_dispatch_leak_ft_q5sn3() {
        let controller = CanaryRolloutController::with_defaults();
        let mut diff = make_healthy_diff(1);
        diff.execution_rejections_count = 1;
        let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
        let check = controller.compute_health_check(1, 1000, &diff, &metrics);
        assert!(
            check
                .failure_reasons
                .contains(&HealthFailureReason::ShadowDispatchLeak),
            "br-ft-q5sn3: Shadow + execution_rejections > 0 must trigger ShadowDispatchLeak"
        );
    }

    #[test]
    fn shadow_phase_with_emissions_only_remains_healthy_ft_q5sn3() {
        // emissions_count > 0 in Shadow is normal — the evaluator
        // is recommending what it WOULD dispatch in Canary. Pre-fix
        // make_healthy_diff used emissions_count=3 with Shadow
        // phase and expected healthy; that contract must hold.
        let controller = CanaryRolloutController::with_defaults();
        let diff = make_healthy_diff(1); // emissions_count=3, no execution evidence
        let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
        let check = controller.compute_health_check(1, 1000, &diff, &metrics);
        assert!(
            check.healthy,
            "br-ft-q5sn3: emissions-only Shadow cycle must stay healthy; got reasons {:?}",
            check.failure_reasons
        );
    }

    #[test]
    fn canary_phase_unexpected_executions_does_not_set_shadow_dispatch_leak_ft_q5sn3() {
        // ShadowDispatchLeak is Shadow-specific. In Canary phase,
        // unexpected_executions is flagged via UnexpectedExecutions
        // (the existing reason), not ShadowDispatchLeak.
        let mut cfg = CanaryRolloutConfig::default();
        cfg.initial_phase = CanaryPhase::Canary;
        let controller = CanaryRolloutController::new(cfg);
        let mut diff = make_healthy_diff(1);
        diff.unexpected_executions = vec![unexpected_execution("bead-x", "agent-x")];
        let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
        let check = controller.compute_health_check(1, 1000, &diff, &metrics);
        assert!(
            check
                .failure_reasons
                .contains(&HealthFailureReason::UnexpectedExecutions),
            "Canary + unexpected_executions must trigger UnexpectedExecutions"
        );
        assert!(
            !check
                .failure_reasons
                .contains(&HealthFailureReason::ShadowDispatchLeak),
            "br-ft-q5sn3: ShadowDispatchLeak is Shadow-only; must NOT fire in Canary"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,
            ..proptest::test_runner::Config::default()
        })]

        /// br-ft-q5sn3: for arbitrary execution-evidence inputs in
        /// Shadow phase, ShadowDispatchLeak fires iff at least one
        /// of (execution_rejections_count > 0, missing_executions
        /// non-empty, unexpected_executions non-empty). emissions
        /// alone never triggers it.
        #[test]
        fn shadow_dispatch_leak_fires_iff_execution_evidence_present_ft_q5sn3(
            emissions in 0usize..=10,
            execution_rejections in 0usize..=5,
            missing_count in 0usize..=4,
            unexpected_count in 0usize..=4,
        ) {
            let controller = CanaryRolloutController::with_defaults();
            let mut diff = make_healthy_diff(1);
            diff.emissions_count = emissions;
            diff.execution_rejections_count = execution_rejections;
            diff.missing_executions = (0..missing_count)
                .map(|i| (format!("b{i}"), format!("a{i}")))
                .collect();
            diff.unexpected_executions = (0..unexpected_count)
                .map(|i| unexpected_execution(format!("ub{i}"), format!("ua{i}")))
                .collect();
            let metrics = make_warmed_metrics(controller.config.min_warmup_cycles);
            let check = controller.compute_health_check(1, 1000, &diff, &metrics);
            let has_execution_evidence = execution_rejections > 0
                || missing_count > 0
                || unexpected_count > 0;
            proptest::prop_assert_eq!(
                check.failure_reasons.contains(&HealthFailureReason::ShadowDispatchLeak),
                has_execution_evidence,
                "br-ft-q5sn3: ShadowDispatchLeak must fire iff execution evidence present; emissions={} rejections={} missing={} unexpected={}",
                emissions, execution_rejections, missing_count, unexpected_count
            );
        }
    }
}
