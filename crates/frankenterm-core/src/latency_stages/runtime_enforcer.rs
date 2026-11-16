//! Runtime budget enforcement wrapper for latency-stage observations.

use serde::{Deserialize, Serialize};

use super::{
    BudgetEnforcer, BudgetEnforcerConfig, CorrelationContext, EnforcerSnapshot, LatencyStage,
    MitigationLevel, PolicyConstraint, ReasonCode, RecoveryProtocol, StageEnforcementState,
    default_policy_constraints,
};

/// Enforcement decision emitted for each stage observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnforcementDecision {
    /// Stage evaluated.
    pub stage: LatencyStage,
    /// Observed latency.
    pub latency_us: f64,
    /// Whether budget was exceeded.
    pub overflow: bool,
    /// Raw mitigation from the enforcer (before policy clamping).
    pub raw_mitigation: MitigationLevel,
    /// Clamped mitigation (after policy constraint).
    pub applied_mitigation: MitigationLevel,
    /// Whether this was a recovery (de-escalation).
    pub recovery: bool,
    /// Reason code.
    pub reason: Option<ReasonCode>,
    /// Whether warmup period is still active (enforcement suppressed).
    pub warmup_active: bool,
}

/// Configuration for the runtime enforcer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEnforcerConfig {
    /// Base enforcer configuration.
    pub enforcer_config: BudgetEnforcerConfig,
    /// Per-stage policy constraints.
    pub policy_constraints: Vec<PolicyConstraint>,
    /// Recovery protocol.
    pub recovery: RecoveryProtocol,
    /// Whether to emit structured decision logs.
    pub log_decisions: bool,
}

impl Default for RuntimeEnforcerConfig {
    fn default() -> Self {
        Self {
            enforcer_config: BudgetEnforcerConfig::default(),
            policy_constraints: default_policy_constraints(),
            recovery: RecoveryProtocol::default(),
            log_decisions: true,
        }
    }
}

/// The runtime budget enforcer with policy constraints and recovery.
///
/// Wraps BudgetEnforcer with:
/// - Policy-safe mitigation clamping
/// - Warmup suppression
/// - Recovery protocol (gradual de-escalation)
/// - Structured decision logging
///
/// # Determinism
/// All decisions are deterministic given the same sequence of observations.
/// No randomness, no system time; callers provide all timestamps.
#[derive(Debug, Clone)]
pub struct RuntimeEnforcer {
    enforcer: BudgetEnforcer,
    config: RuntimeEnforcerConfig,
    states: Vec<(LatencyStage, StageEnforcementState)>,
    stage_observations: Vec<(LatencyStage, u64)>,
    decisions: Vec<EnforcementDecision>,
    observation_count: u64,
}

impl RuntimeEnforcer {
    /// Create a new runtime enforcer with the given configuration.
    pub fn new(config: RuntimeEnforcerConfig) -> Self {
        let enforcer = BudgetEnforcer::new(config.enforcer_config.clone());
        let states = LatencyStage::PIPELINE_STAGES
            .iter()
            .map(|&s| (s, StageEnforcementState::new()))
            .collect();
        let stage_observations = LatencyStage::PIPELINE_STAGES
            .iter()
            .map(|&s| (s, 0))
            .collect();
        Self {
            enforcer,
            config,
            states,
            stage_observations,
            decisions: Vec::new(),
            observation_count: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(RuntimeEnforcerConfig::default())
    }

    fn increment_stage_observations(&mut self, stage: LatencyStage) -> u64 {
        if let Some((_, count)) = self
            .stage_observations
            .iter_mut()
            .find(|(observed_stage, _)| *observed_stage == stage)
        {
            *count = count.saturating_add(1);
            *count
        } else {
            self.stage_observations.push((stage, 1));
            1
        }
    }

    /// Record an observation and produce an enforcement decision.
    ///
    /// This is the main entry point for the critical path. It:
    /// 1. Records the observation in the base enforcer
    /// 2. Determines raw mitigation from overflow severity
    /// 3. Applies policy constraints (clamping)
    /// 4. Checks recovery conditions
    /// 5. Updates enforcement state
    /// 6. Emits a structured decision
    #[allow(clippy::similar_names)]
    pub fn enforce(
        &mut self,
        stage: LatencyStage,
        latency_us: f64,
        correlation_id: &str,
        now_us: u64,
    ) -> EnforcementDecision {
        self.observation_count = self.observation_count.saturating_add(1);

        // Step 1: Record in base enforcer.
        let obs = self.enforcer.record(stage, latency_us, correlation_id);

        // Find enforcement state for this stage.
        let state_index = self
            .states
            .iter()
            .position(|(observed_stage, _)| *observed_stage == stage);

        let Some(state_index) = state_index else {
            // Unknown stage: pass through.
            return EnforcementDecision {
                stage,
                latency_us,
                overflow: false,
                raw_mitigation: MitigationLevel::None,
                applied_mitigation: MitigationLevel::None,
                recovery: false,
                reason: None,
                warmup_active: true,
            };
        };

        let stage_observation_count = self.increment_stage_observations(stage);
        let state = match self.states.get_mut(state_index).map(|(_, st)| st) {
            Some(state) => state,
            None => {
                return EnforcementDecision {
                    stage,
                    latency_us,
                    overflow: false,
                    raw_mitigation: MitigationLevel::None,
                    applied_mitigation: MitigationLevel::None,
                    recovery: false,
                    reason: None,
                    warmup_active: true,
                };
            }
        };

        // Find policy constraint.
        let constraint = self
            .config
            .policy_constraints
            .iter()
            .find(|c| c.stage == stage);

        // Step 2: Check warmup.
        let warmup_active = constraint
            .map(|c| stage_observation_count <= c.warmup_count)
            .unwrap_or(false);

        // Step 3: Determine raw mitigation level.
        let raw_level = MitigationLevel::from_mitigation(obs.recommended_mitigation);

        // Step 4: Apply policy constraint.
        let clamped_level = if warmup_active {
            MitigationLevel::None
        } else {
            constraint.map(|c| c.clamp(raw_level)).unwrap_or(raw_level)
        };

        // Step 5: Recovery check.
        let mut recovery = false;
        if obs.overflow {
            state.consecutive_ok = 0;
            if clamped_level > state.current_level {
                state.current_level = clamped_level;
                state.last_escalation_us = now_us;
                state.escalation_count = state.escalation_count.saturating_add(1);
            }
        } else {
            state.consecutive_ok = state.consecutive_ok.saturating_add(1);

            // Check recovery conditions.
            let cooldown_met = state.consecutive_ok >= self.config.recovery.cooldown_observations;
            let timeout_met = now_us.saturating_sub(state.last_escalation_us)
                >= self.config.recovery.max_degraded_duration_us;

            if state.current_level > MitigationLevel::None && (cooldown_met || timeout_met) {
                recovery = true;
                state.recovery_count = state.recovery_count.saturating_add(1);
                if self.config.recovery.gradual && state.current_level > MitigationLevel::None {
                    // Step down one level.
                    let severity = state.current_level.severity();
                    state.current_level = if severity > 0 {
                        MitigationLevel::ALL[severity as usize - 1]
                    } else {
                        MitigationLevel::None
                    };
                } else {
                    state.current_level = MitigationLevel::None;
                }
                state.consecutive_ok = 0;
            }
        }

        let decision = EnforcementDecision {
            stage,
            latency_us,
            overflow: obs.overflow,
            raw_mitigation: raw_level,
            applied_mitigation: state.current_level,
            recovery,
            reason: obs.reason,
            warmup_active,
        };

        if self.config.log_decisions {
            self.decisions.push(decision.clone());
        }

        decision
    }

    /// Get the current mitigation level for a stage.
    pub fn current_level(&self, stage: LatencyStage) -> MitigationLevel {
        self.states
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st.current_level)
            .unwrap_or(MitigationLevel::None)
    }

    /// Get the enforcement state for a stage.
    pub fn stage_state(&self, stage: LatencyStage) -> Option<&StageEnforcementState> {
        self.states
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st)
    }

    /// Get the underlying enforcer.
    pub fn base_enforcer(&self) -> &BudgetEnforcer {
        &self.enforcer
    }

    /// Get accumulated decisions and clear.
    pub fn drain_decisions(&mut self) -> Vec<EnforcementDecision> {
        std::mem::take(&mut self.decisions)
    }

    /// Total observations processed.
    pub fn total_observations(&self) -> u64 {
        self.observation_count
    }

    /// Total escalations across all stages.
    pub fn total_escalations(&self) -> u64 {
        self.states.iter().fold(0, |total, (_, state)| {
            total.saturating_add(state.escalation_count)
        })
    }

    /// Total recoveries across all stages.
    pub fn total_recoveries(&self) -> u64 {
        self.states.iter().fold(0, |total, (_, state)| {
            total.saturating_add(state.recovery_count)
        })
    }

    /// Whether all stages are at MitigationLevel::None.
    pub fn is_fully_recovered(&self) -> bool {
        self.states
            .iter()
            .all(|(_, s)| s.current_level == MitigationLevel::None)
    }

    /// Compact status string.
    pub fn status_line(&self) -> String {
        let degraded: Vec<String> = self
            .states
            .iter()
            .filter(|(_, s)| s.current_level > MitigationLevel::None)
            .map(|(stage, s)| format!("{}={}", stage, s.current_level))
            .collect();
        if degraded.is_empty() {
            format!(
                "enforcement=NOMINAL obs={} esc={} rec={}",
                self.observation_count,
                self.total_escalations(),
                self.total_recoveries()
            )
        } else {
            format!(
                "enforcement=DEGRADED [{}] obs={} esc={} rec={}",
                degraded.join(", "),
                self.observation_count,
                self.total_escalations(),
                self.total_recoveries()
            )
        }
    }

    /// Process a complete CorrelationContext through the enforcer.
    ///
    /// Returns per-stage enforcement decisions.
    pub fn enforce_run(
        &mut self,
        ctx: &CorrelationContext,
        base_time_us: u64,
    ) -> Vec<EnforcementDecision> {
        let mut decisions = Vec::with_capacity(ctx.timings.len());
        for timing in &ctx.timings {
            let d = self.enforce(
                timing.stage,
                timing.latency_us,
                &ctx.correlation_id,
                base_time_us.saturating_add(timing.end_us),
            );
            decisions.push(d);
        }
        decisions
    }

    /// Get a full diagnostic snapshot.
    pub fn diagnostic_snapshot(&self) -> RuntimeEnforcerSnapshot {
        RuntimeEnforcerSnapshot {
            observation_count: self.observation_count,
            total_escalations: self.total_escalations(),
            total_recoveries: self.total_recoveries(),
            fully_recovered: self.is_fully_recovered(),
            stage_states: self.states.iter().map(|(s, st)| (*s, st.clone())).collect(),
            base_snapshot: self.enforcer.snapshot(),
        }
    }
}

/// Full diagnostic snapshot of the runtime enforcer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEnforcerSnapshot {
    pub observation_count: u64,
    pub total_escalations: u64,
    pub total_recoveries: u64,
    pub fully_recovered: bool,
    pub stage_states: Vec<(LatencyStage, StageEnforcementState)>,
    pub base_snapshot: EnforcerSnapshot,
}
