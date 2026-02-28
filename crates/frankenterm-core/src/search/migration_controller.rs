//! Migration controller for legacy path retirement (ft-dr6zv.1.3.D1).
//!
//! Wraps the `ReplayGate` and `SearchFacade` into a runtime state machine
//! that manages incremental migration phases:
//!
//! ```text
//!   PreCheck ──► Shadow ──► Canary ──► Cutover ──► Retired
//!                  ▲          │          │
//!                  └──────── Rollback ◄──┘
//! ```
//!
//! The controller drives phase transitions based on health-check verdicts,
//! automatically rolls back on repeated failures, and produces a
//! `RetirementGateResult` summarising all conditions for go/no-go.

use serde::{Deserialize, Serialize};

use super::regression_diff::{
    RegressionScenario, ReplayGateConfig, ReplayGateVerdict, default_scenarios, run_replay_gate,
};

// ---------------------------------------------------------------------------
// Migration phase
// ---------------------------------------------------------------------------

/// Phase in the legacy-to-orchestrated migration lifecycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    /// Initial state: pre-flight checks not yet run.
    #[default]
    PreCheck,
    /// Shadow mode: both paths run, legacy returned, diffs logged.
    Shadow,
    /// Canary: orchestrated serves a fraction, legacy is fallback.
    Canary,
    /// Full cutover: orchestrated path is primary.
    Cutover,
    /// Reverted to legacy due to health-check failure.
    Rollback,
    /// Terminal: legacy shims safe to remove (retirement gate passed).
    Retired,
}

impl MigrationPhase {
    /// True only for terminal states.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Retired | Self::Rollback)
    }

    /// True when the orchestrated path is serving production traffic.
    #[must_use]
    pub fn is_live_on_orchestrated(self) -> bool {
        matches!(self, Self::Cutover | Self::Retired)
    }

    /// Canonical string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreCheck => "pre_check",
            Self::Shadow => "shadow",
            Self::Canary => "canary",
            Self::Cutover => "cutover",
            Self::Rollback => "rollback",
            Self::Retired => "retired",
        }
    }

    /// Parse from a string (case-insensitive, defaults to PreCheck).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "shadow" => Self::Shadow,
            "canary" => Self::Canary,
            "cutover" => Self::Cutover,
            "rollback" => Self::Rollback,
            "retired" => Self::Retired,
            "pre_check" | "precheck" => Self::PreCheck,
            _ => Self::PreCheck,
        }
    }
}

impl std::fmt::Display for MigrationPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Phase transition error
// ---------------------------------------------------------------------------

/// Error when a requested phase transition is illegal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseTransitionError {
    pub from: MigrationPhase,
    pub to: MigrationPhase,
    pub reason: String,
}

impl std::fmt::Display for PhaseTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot transition {} -> {}: {}",
            self.from, self.to, self.reason
        )
    }
}

// ---------------------------------------------------------------------------
// Health check result
// ---------------------------------------------------------------------------

/// Result of a single runtime probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult {
    /// Name of the check.
    pub name: String,
    /// Whether this check passed.
    pub passed: bool,
    /// Human-readable detail.
    pub detail: String,
}

// ---------------------------------------------------------------------------
// Retirement gate result
// ---------------------------------------------------------------------------

/// Aggregate verdict for legacy path retirement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetirementGateResult {
    /// True = safe to remove legacy shims.
    pub approved: bool,
    /// Phase at evaluation time.
    pub phase: MigrationPhase,
    /// Replay gate verdict.
    pub replay_verdict: ReplayGateVerdict,
    /// Individual health checks.
    pub health_checks: Vec<HealthCheckResult>,
    /// Number of checks that passed.
    pub checks_passed: usize,
    /// Number of checks that failed.
    pub checks_failed: usize,
    /// Human-readable summary.
    pub summary: String,
    /// ISO-8601 timestamp.
    pub evaluated_at: String,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}
fn default_max_failures() -> u32 {
    3
}

/// Configuration for the migration controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationControllerConfig {
    /// Replay gate config forwarded to regression_diff.
    pub replay_gate: ReplayGateConfig,
    /// Require replay gate to pass before advancing past Shadow.
    #[serde(default = "default_true")]
    pub require_replay_gate: bool,
    /// Require schema gates to pass before retirement approval.
    #[serde(default = "default_true")]
    pub require_schema_gates: bool,
    /// Auto-advance phase on successful health check.
    #[serde(default)]
    pub auto_advance: bool,
    /// Max consecutive failures before automatic rollback.
    #[serde(default = "default_max_failures")]
    pub max_consecutive_failures: u32,
}

impl Default for MigrationControllerConfig {
    fn default() -> Self {
        Self {
            replay_gate: ReplayGateConfig::default(),
            require_replay_gate: true,
            require_schema_gates: true,
            auto_advance: false,
            max_consecutive_failures: default_max_failures(),
        }
    }
}

// ---------------------------------------------------------------------------
// Transition record (audit log)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransitionRecord {
    from: MigrationPhase,
    to: MigrationPhase,
    reason: String,
    at: String,
}

// ---------------------------------------------------------------------------
// Migration controller
// ---------------------------------------------------------------------------

/// Runtime controller for legacy-to-orchestrated migration phases.
pub struct MigrationController {
    config: MigrationControllerConfig,
    phase: MigrationPhase,
    consecutive_failures: u32,
    transition_log: Vec<TransitionRecord>,
}

impl MigrationController {
    /// Create a controller starting in PreCheck with default config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: MigrationControllerConfig::default(),
            phase: MigrationPhase::PreCheck,
            consecutive_failures: 0,
            transition_log: Vec::new(),
        }
    }

    /// Create a controller with explicit config.
    #[must_use]
    pub fn with_config(config: MigrationControllerConfig) -> Self {
        Self {
            config,
            phase: MigrationPhase::PreCheck,
            consecutive_failures: 0,
            transition_log: Vec::new(),
        }
    }

    // -- Accessors --

    #[must_use]
    pub fn phase(&self) -> MigrationPhase {
        self.phase
    }

    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    #[must_use]
    pub fn transition_count(&self) -> usize {
        self.transition_log.len()
    }

    #[must_use]
    pub fn is_rolled_back(&self) -> bool {
        self.phase == MigrationPhase::Rollback
    }

    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.phase == MigrationPhase::Retired
    }

    // -- Phase transitions --

    /// Attempt a manual phase transition. Returns error if illegal.
    pub fn advance_to(&mut self, target: MigrationPhase) -> Result<(), PhaseTransitionError> {
        if self.phase == target {
            return Err(PhaseTransitionError {
                from: self.phase,
                to: target,
                reason: "already in target phase".to_string(),
            });
        }

        if !self.is_legal_transition(target) {
            return Err(PhaseTransitionError {
                from: self.phase,
                to: target,
                reason: format!("illegal transition from {} to {}", self.phase, target),
            });
        }

        self.record_transition(target, "manual advance");
        self.phase = target;
        if target != MigrationPhase::Rollback {
            self.consecutive_failures = 0;
        }
        Ok(())
    }

    /// Force rollback to Shadow, resetting failure counter.
    pub fn rollback(&mut self, reason: &str) -> MigrationPhase {
        let target = MigrationPhase::Rollback;
        self.record_transition(target, reason);
        self.phase = target;
        self.consecutive_failures = 0;
        self.phase
    }

    // -- Health checks --

    /// Run a health check with the given regression scenarios.
    pub fn run_health_check(&mut self, scenarios: &[RegressionScenario]) -> RetirementGateResult {
        let verdict = run_replay_gate(scenarios, &self.config.replay_gate);

        // Determine data health (independent of phase_ready).
        let data_healthy = verdict.go;

        if data_healthy {
            self.consecutive_failures = 0;
            if self.config.auto_advance {
                self.try_auto_advance();
            }
        } else {
            self.consecutive_failures += 1;
            if self.consecutive_failures >= self.config.max_consecutive_failures
                && !self.phase.is_terminal()
            {
                self.rollback("max consecutive failures exceeded");
            }
        }

        // Evaluate full retirement gate (including phase_ready) after any phase change.
        self.evaluate_retirement_gate(&verdict)
    }

    /// Run health check with default scenarios.
    pub fn run_retirement_check(&mut self) -> RetirementGateResult {
        self.run_health_check(&default_scenarios())
    }

    /// Pure evaluation without running scenarios (takes pre-computed verdict).
    #[must_use]
    pub fn evaluate_retirement_gate(&self, verdict: &ReplayGateVerdict) -> RetirementGateResult {
        let mut checks = Vec::new();

        // Check 1: Replay gate overall.
        checks.push(HealthCheckResult {
            name: "replay_gate_overall".to_string(),
            passed: verdict.go,
            detail: match &verdict.reason {
                Some(r) => format!("replay gate no-go: {r}"),
                None => "replay gate passed".to_string(),
            },
        });

        // Check 2: Regression pass rate.
        let pass_rate = verdict.regression.pass_rate;
        let rate_ok = pass_rate >= self.config.replay_gate.min_pass_rate;
        checks.push(HealthCheckResult {
            name: "regression_pass_rate".to_string(),
            passed: rate_ok,
            detail: format!(
                "pass rate {:.2} (minimum {:.2})",
                pass_rate, self.config.replay_gate.min_pass_rate
            ),
        });

        // Check 3: Schema fusion.
        if self.config.require_schema_gates {
            checks.push(HealthCheckResult {
                name: "schema_fusion".to_string(),
                passed: verdict.schema_fusion.safe,
                detail: verdict.schema_fusion.summary.clone(),
            });
        }

        // Check 4: Schema orchestration.
        if self.config.require_schema_gates {
            checks.push(HealthCheckResult {
                name: "schema_orchestration".to_string(),
                passed: verdict.schema_orchestration.safe,
                detail: verdict.schema_orchestration.summary.clone(),
            });
        }

        // Check 5: Phase readiness for retirement.
        let phase_ready = matches!(
            self.phase,
            MigrationPhase::Cutover | MigrationPhase::Retired
        );
        checks.push(HealthCheckResult {
            name: "phase_ready".to_string(),
            passed: phase_ready,
            detail: format!("current phase: {} (need cutover or retired)", self.phase),
        });

        let checks_passed = checks.iter().filter(|c| c.passed).count();
        let checks_failed = checks.len() - checks_passed;
        let approved = checks_failed == 0;

        let summary = if approved {
            "retirement approved: all checks passed".to_string()
        } else {
            let failed_names: Vec<&str> = checks
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.name.as_str())
                .collect();
            format!(
                "retirement blocked: {} check(s) failed [{}]",
                checks_failed,
                failed_names.join(", ")
            )
        };

        RetirementGateResult {
            approved,
            phase: self.phase,
            replay_verdict: verdict.clone(),
            health_checks: checks,
            checks_passed,
            checks_failed,
            summary,
            evaluated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    // -- Internals --

    fn is_legal_transition(&self, target: MigrationPhase) -> bool {
        match (self.phase, target) {
            // Forward progression.
            (MigrationPhase::PreCheck, MigrationPhase::Shadow) => true,
            (MigrationPhase::Shadow, MigrationPhase::Canary) => true,
            (MigrationPhase::Canary, MigrationPhase::Cutover) => true,
            (MigrationPhase::Cutover, MigrationPhase::Retired) => true,
            // Rollback from active phases.
            (MigrationPhase::Shadow, MigrationPhase::Rollback) => true,
            (MigrationPhase::Canary, MigrationPhase::Rollback) => true,
            (MigrationPhase::Cutover, MigrationPhase::Rollback) => true,
            // Re-entry after rollback.
            (MigrationPhase::Rollback, MigrationPhase::Shadow) => true,
            _ => false,
        }
    }

    fn try_auto_advance(&mut self) {
        let next = match self.phase {
            MigrationPhase::PreCheck => Some(MigrationPhase::Shadow),
            MigrationPhase::Shadow => Some(MigrationPhase::Canary),
            MigrationPhase::Canary => Some(MigrationPhase::Cutover),
            MigrationPhase::Cutover => Some(MigrationPhase::Retired),
            _ => None,
        };
        if let Some(target) = next {
            let _ = self.advance_to(target);
        }
    }

    fn record_transition(&mut self, to: MigrationPhase, reason: &str) {
        self.transition_log.push(TransitionRecord {
            from: self.phase,
            to,
            reason: reason.to_string(),
            at: chrono::Utc::now().to_rfc3339(),
        });
    }
}

impl Default for MigrationController {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Free function
// ---------------------------------------------------------------------------

/// Run the full retirement gate from scratch with default config and scenarios.
#[must_use]
pub fn run_default_retirement_gate() -> RetirementGateResult {
    let mut ctrl = MigrationController::new();
    ctrl.run_retirement_check()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Phase basics --

    #[test]
    fn phase_default_is_precheck() {
        assert_eq!(MigrationPhase::default(), MigrationPhase::PreCheck);
    }

    #[test]
    fn phase_parse_roundtrip() {
        for phase in &[
            MigrationPhase::PreCheck,
            MigrationPhase::Shadow,
            MigrationPhase::Canary,
            MigrationPhase::Cutover,
            MigrationPhase::Rollback,
            MigrationPhase::Retired,
        ] {
            assert_eq!(MigrationPhase::parse(phase.as_str()), *phase);
        }
    }

    #[test]
    fn phase_serde_roundtrip() {
        for phase in &[
            MigrationPhase::PreCheck,
            MigrationPhase::Shadow,
            MigrationPhase::Canary,
            MigrationPhase::Cutover,
            MigrationPhase::Rollback,
            MigrationPhase::Retired,
        ] {
            let json = serde_json::to_string(phase).unwrap();
            let parsed: MigrationPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(*phase, parsed);
        }
    }

    #[test]
    fn phase_is_terminal() {
        assert!(MigrationPhase::Retired.is_terminal());
        assert!(MigrationPhase::Rollback.is_terminal());
        assert!(!MigrationPhase::Shadow.is_terminal());
        assert!(!MigrationPhase::Cutover.is_terminal());
    }

    #[test]
    fn phase_is_live_on_orchestrated() {
        assert!(MigrationPhase::Cutover.is_live_on_orchestrated());
        assert!(MigrationPhase::Retired.is_live_on_orchestrated());
        assert!(!MigrationPhase::Shadow.is_live_on_orchestrated());
        assert!(!MigrationPhase::Canary.is_live_on_orchestrated());
    }

    // -- Phase transitions --

    #[test]
    fn advance_legal_sequence() {
        let mut ctrl = MigrationController::new();
        assert!(ctrl.advance_to(MigrationPhase::Shadow).is_ok());
        assert!(ctrl.advance_to(MigrationPhase::Canary).is_ok());
        assert!(ctrl.advance_to(MigrationPhase::Cutover).is_ok());
        assert!(ctrl.advance_to(MigrationPhase::Retired).is_ok());
        assert_eq!(ctrl.phase(), MigrationPhase::Retired);
    }

    #[test]
    fn advance_illegal_backward() {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        let err = ctrl.advance_to(MigrationPhase::PreCheck).unwrap_err();
        assert_eq!(err.from, MigrationPhase::Shadow);
        assert_eq!(err.to, MigrationPhase::PreCheck);
    }

    #[test]
    fn advance_retired_is_terminal() {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.advance_to(MigrationPhase::Canary).unwrap();
        ctrl.advance_to(MigrationPhase::Cutover).unwrap();
        ctrl.advance_to(MigrationPhase::Retired).unwrap();
        assert!(ctrl.advance_to(MigrationPhase::Shadow).is_err());
    }

    #[test]
    fn advance_same_phase_errors() {
        let mut ctrl = MigrationController::new();
        let err = ctrl.advance_to(MigrationPhase::PreCheck).unwrap_err();
        assert!(err.reason.contains("already in target phase"));
    }

    #[test]
    fn advance_to_rollback_from_canary() {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.advance_to(MigrationPhase::Canary).unwrap();
        assert!(ctrl.advance_to(MigrationPhase::Rollback).is_ok());
        assert_eq!(ctrl.phase(), MigrationPhase::Rollback);
    }

    #[test]
    fn rollback_resets_failure_counter() {
        let mut ctrl = MigrationController::new();
        ctrl.consecutive_failures = 5;
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.rollback("test rollback");
        assert_eq!(ctrl.consecutive_failures(), 0);
    }

    #[test]
    fn rollback_records_transition_log() {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.rollback("integration failure");
        assert!(ctrl.transition_count() >= 2); // advance + rollback
    }

    #[test]
    fn rollback_reentry_to_shadow() {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.rollback("test");
        assert!(ctrl.advance_to(MigrationPhase::Shadow).is_ok());
        assert_eq!(ctrl.phase(), MigrationPhase::Shadow);
    }

    // -- Health checks --

    #[test]
    fn health_check_default_at_precheck() {
        let mut ctrl = MigrationController::new();
        let result = ctrl.run_retirement_check();
        // All data checks pass, but phase_ready fails (not at cutover).
        assert!(!result.approved);
        assert!(result.checks_failed >= 1);
    }

    #[test]
    fn health_check_at_cutover_approves() {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.advance_to(MigrationPhase::Canary).unwrap();
        ctrl.advance_to(MigrationPhase::Cutover).unwrap();
        let result = ctrl.run_retirement_check();
        assert!(
            result.approved,
            "should approve at cutover: {}",
            result.summary
        );
    }

    #[test]
    fn health_check_failure_increments_counter() {
        let config = MigrationControllerConfig {
            replay_gate: ReplayGateConfig {
                min_pass_rate: 1.1, // impossible
                ..ReplayGateConfig::default()
            },
            max_consecutive_failures: 100, // don't auto-rollback
            ..MigrationControllerConfig::default()
        };
        let mut ctrl = MigrationController::with_config(config);
        ctrl.run_retirement_check();
        assert!(ctrl.consecutive_failures() >= 1);
    }

    #[test]
    fn auto_rollback_on_max_failures() {
        let config = MigrationControllerConfig {
            replay_gate: ReplayGateConfig {
                min_pass_rate: 1.1,
                ..ReplayGateConfig::default()
            },
            max_consecutive_failures: 1,
            ..MigrationControllerConfig::default()
        };
        let mut ctrl = MigrationController::with_config(config);
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.run_retirement_check();
        assert_eq!(ctrl.phase(), MigrationPhase::Rollback);
    }

    #[test]
    fn auto_advance_progresses_phase() {
        let config = MigrationControllerConfig {
            auto_advance: true,
            ..MigrationControllerConfig::default()
        };
        let mut ctrl = MigrationController::with_config(config);
        ctrl.run_retirement_check();
        // Should have advanced at least to Shadow.
        assert_ne!(ctrl.phase(), MigrationPhase::PreCheck);
    }

    #[test]
    fn evaluate_retirement_gate_pure() {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        ctrl.advance_to(MigrationPhase::Canary).unwrap();
        ctrl.advance_to(MigrationPhase::Cutover).unwrap();
        ctrl.advance_to(MigrationPhase::Retired).unwrap();

        let verdict = run_replay_gate(&default_scenarios(), &ReplayGateConfig::default());
        let result = ctrl.evaluate_retirement_gate(&verdict);
        assert!(
            result.approved,
            "retired phase should approve: {}",
            result.summary
        );
    }

    #[test]
    fn retirement_gate_result_serde_roundtrip() {
        let mut ctrl = MigrationController::new();
        let result = ctrl.run_retirement_check();
        let json = serde_json::to_string(&result).unwrap();
        let parsed: RetirementGateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.approved, parsed.approved);
        assert_eq!(result.checks_passed, parsed.checks_passed);
    }

    #[test]
    fn run_default_retirement_gate_smoke() {
        let result = run_default_retirement_gate();
        // At PreCheck, phase_ready fails, so not approved.
        assert!(!result.approved);
        assert!(!result.summary.is_empty());
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = MigrationControllerConfig {
            auto_advance: true,
            max_consecutive_failures: 5,
            ..MigrationControllerConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: MigrationControllerConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.auto_advance);
        assert_eq!(parsed.max_consecutive_failures, 5);
    }
}
