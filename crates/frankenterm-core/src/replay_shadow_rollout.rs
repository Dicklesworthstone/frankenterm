//! Staged shadow rollout with kill switches and rollback drills.
//!
//! Bead: ft-og6q6.7.5
//!
//! Manages the staged rollout of replay gates from shadow mode
//! (informational-only) to full enforcement, with kill switches,
//! flaky test detection, rollback triggers, and monitoring metrics.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::replay_ci_gate::{GateId, GateReport, GateStatus, ALL_GATES};

// ── Constants ────────────────────────────────────────────────────────────────

/// Environment variable to disable gate enforcement.
pub const KILL_SWITCH_ENV: &str = "FT_REPLAY_GATES_ENFORCE";

/// Default shadow period in days.
pub const DEFAULT_SHADOW_DAYS: u64 = 14;

/// Flaky rate warning threshold (3%).
pub const FLAKY_RATE_WARNING: f64 = 0.03;

/// Flaky rate critical threshold (5%).
pub const FLAKY_RATE_CRITICAL: f64 = 0.05;

/// Duration multiplier for rollback trigger.
pub const DURATION_ROLLBACK_MULTIPLIER: f64 = 2.0;

// ── Rollout Stage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutStage {
    /// Stage 1 (Week 1-2): All gates shadow, no blocking.
    Shadow,
    /// Stage 2 (Week 3-4): Gate 1+2 enforced, Gate 3 shadow.
    Partial,
    /// Stage 3 (Week 5+): All gates enforced.
    Full,
}

impl RolloutStage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Partial => "partial",
            Self::Full => "full",
        }
    }

    #[must_use]
    pub fn from_str_stage(s: &str) -> Option<Self> {
        match s {
            "shadow" => Some(Self::Shadow),
            "partial" => Some(Self::Partial),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Whether this gate is enforced (blocking) at this stage.
    #[must_use]
    pub fn is_enforced(self, gate: GateId) -> bool {
        match self {
            Self::Shadow => false,
            Self::Partial => matches!(gate, GateId::Smoke | GateId::TestSuite),
            Self::Full => true,
        }
    }

    /// Stage from elapsed days since rollout start.
    #[must_use]
    pub fn from_elapsed_days(days: u64) -> Self {
        if days < 14 {
            Self::Shadow
        } else if days < 28 {
            Self::Partial
        } else {
            Self::Full
        }
    }
}

// ── Rollout Config ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutConfig {
    pub stage: RolloutStage,
    pub kill_switch: bool,
    pub shadow_days: u64,
    pub flaky_rate_warning: f64,
    pub flaky_rate_critical: f64,
    pub duration_rollback_multiplier: f64,
}

impl Default for RolloutConfig {
    fn default() -> Self {
        Self {
            stage: RolloutStage::Shadow,
            kill_switch: false,
            shadow_days: DEFAULT_SHADOW_DAYS,
            flaky_rate_warning: FLAKY_RATE_WARNING,
            flaky_rate_critical: FLAKY_RATE_CRITICAL,
            duration_rollback_multiplier: DURATION_ROLLBACK_MULTIPLIER,
        }
    }
}

impl RolloutConfig {
    /// Check the kill switch env var (without calling set_var).
    #[must_use]
    pub fn is_enforcement_disabled_by_env(env_value: Option<&str>) -> bool {
        match env_value {
            Some(v) => v == "false" || v == "0" || v == "no",
            None => false,
        }
    }
}

// ── Enforcement Decision ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnforcementDecision {
    pub gate: GateId,
    pub gate_status: GateStatus,
    pub enforced: bool,
    pub pr_blocked: bool,
    pub reason: String,
    pub mode: EnforcementMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementMode {
    Shadow,
    Enforce,
    KillSwitch,
}

/// Evaluate whether a gate result should block a PR.
#[must_use]
pub fn evaluate_enforcement(
    config: &RolloutConfig,
    report: &GateReport,
    kill_switch_env: Option<&str>,
) -> EnforcementDecision {
    let kill_switch_active = config.kill_switch
        || RolloutConfig::is_enforcement_disabled_by_env(kill_switch_env);

    if kill_switch_active {
        return EnforcementDecision {
            gate: report.gate,
            gate_status: report.status,
            enforced: false,
            pr_blocked: false,
            reason: "Kill switch active — enforcement disabled".into(),
            mode: EnforcementMode::KillSwitch,
        };
    }

    let enforced = config.stage.is_enforced(report.gate);
    let pr_blocked = enforced && report.status == GateStatus::Fail;

    let mode = if enforced {
        EnforcementMode::Enforce
    } else {
        EnforcementMode::Shadow
    };

    let reason = if !enforced {
        format!("{} in shadow mode — results informational only", report.gate.display_name())
    } else if pr_blocked {
        format!("{} failed — PR blocked", report.gate.display_name())
    } else {
        format!("{} passed", report.gate.display_name())
    };

    EnforcementDecision {
        gate: report.gate,
        gate_status: report.status,
        enforced,
        pr_blocked,
        reason,
        mode,
    }
}

// ── Flaky Detection ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlakyTestRecord {
    pub check_name: String,
    pub gate: GateId,
    pub first_seen: String,
    pub occurrences: u64,
    pub last_seen: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlakyRateMetrics {
    pub total_runs: u64,
    pub flaky_runs: u64,
    pub flaky_rate: f64,
    pub alert_level: AlertLevel,
    pub flaky_tests: Vec<FlakyTestRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    Normal,
    Warning,
    Critical,
}

/// Calculate flaky rate and alert level from run history.
#[must_use]
pub fn calculate_flaky_rate(
    total_runs: u64,
    flaky_runs: u64,
    config: &RolloutConfig,
    flaky_tests: Vec<FlakyTestRecord>,
) -> FlakyRateMetrics {
    let flaky_rate = if total_runs == 0 {
        0.0
    } else {
        flaky_runs as f64 / total_runs as f64
    };

    let alert_level = if flaky_rate >= config.flaky_rate_critical {
        AlertLevel::Critical
    } else if flaky_rate >= config.flaky_rate_warning {
        AlertLevel::Warning
    } else {
        AlertLevel::Normal
    };

    FlakyRateMetrics {
        total_runs,
        flaky_runs,
        flaky_rate,
        alert_level,
        flaky_tests,
    }
}

// ── Rollback Trigger ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackTrigger {
    pub triggered: bool,
    pub reasons: Vec<String>,
}

/// Evaluate rollback triggers.
#[must_use]
pub fn evaluate_rollback_triggers(
    flaky_metrics: &FlakyRateMetrics,
    gate_durations: &BTreeMap<String, (u64, u64)>, // gate -> (actual_ms, budget_ms)
    config: &RolloutConfig,
) -> RollbackTrigger {
    let mut reasons = Vec::new();

    if flaky_metrics.flaky_rate > config.flaky_rate_critical {
        reasons.push(format!(
            "Flaky rate {:.1}% exceeds critical threshold {:.1}%",
            flaky_metrics.flaky_rate * 100.0,
            config.flaky_rate_critical * 100.0
        ));
    }

    for (gate, (actual, budget)) in gate_durations {
        if *budget > 0 {
            let ratio = *actual as f64 / *budget as f64;
            if ratio > config.duration_rollback_multiplier {
                reasons.push(format!(
                    "Gate {} duration {}ms exceeds {}x budget {}ms (ratio: {:.1}x)",
                    gate, actual, config.duration_rollback_multiplier, budget, ratio
                ));
            }
        }
    }

    RollbackTrigger {
        triggered: !reasons.is_empty(),
        reasons,
    }
}

// ── Rollback Drill ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrillResult {
    pub drill_type: DrillType,
    pub executed_at: String,
    pub success: bool,
    pub steps: Vec<DrillStep>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrillType {
    Rollback,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrillStep {
    pub name: String,
    pub passed: bool,
    pub message: String,
}

/// Execute a rollback drill (dry-run evaluation).
#[must_use]
pub fn evaluate_rollback_drill(
    gates_disabled: bool,
    gates_re_enabled: bool,
    no_breakage: bool,
    duration_ms: u64,
    executed_at: &str,
) -> DrillResult {
    let mut steps = Vec::new();

    steps.push(DrillStep {
        name: "disable_gates".into(),
        passed: gates_disabled,
        message: if gates_disabled {
            "Gates disabled successfully".into()
        } else {
            "Failed to disable gates".into()
        },
    });

    steps.push(DrillStep {
        name: "verify_no_breakage".into(),
        passed: no_breakage,
        message: if no_breakage {
            "No breakage detected with gates disabled".into()
        } else {
            "Breakage detected with gates disabled".into()
        },
    });

    steps.push(DrillStep {
        name: "re_enable_gates".into(),
        passed: gates_re_enabled,
        message: if gates_re_enabled {
            "Gates re-enabled successfully".into()
        } else {
            "Failed to re-enable gates".into()
        },
    });

    let success = gates_disabled && no_breakage && gates_re_enabled;

    DrillResult {
        drill_type: DrillType::Rollback,
        executed_at: executed_at.into(),
        success,
        steps,
        duration_ms,
    }
}

// ── Monitoring Metrics ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutMetrics {
    pub stage: RolloutStage,
    pub gate_pass_rate: BTreeMap<String, f64>,
    pub gate_duration_trend: BTreeMap<String, Vec<u64>>,
    pub flaky_rate: f64,
    pub artifact_count: u64,
    pub days_in_stage: u64,
}

/// Generate a weekly digest summary.
#[must_use]
pub fn weekly_digest(metrics: &RolloutMetrics) -> String {
    let mut lines = Vec::new();
    lines.push(format!("## Replay Gates Weekly Digest"));
    lines.push(format!("**Stage:** {}", metrics.stage.as_str()));
    lines.push(format!("**Days in stage:** {}", metrics.days_in_stage));
    lines.push(String::new());

    lines.push("### Gate Pass Rates".into());
    for (gate, rate) in &metrics.gate_pass_rate {
        lines.push(format!("- {}: {:.1}%", gate, rate * 100.0));
    }
    lines.push(String::new());

    lines.push(format!("### Flaky Rate: {:.1}%", metrics.flaky_rate * 100.0));
    lines.push(format!("### Artifacts in Library: {}", metrics.artifact_count));

    lines.join("\n")
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_ci_gate::GateCheck;

    fn make_pass_report(gate: GateId) -> GateReport {
        GateReport::new(gate, vec![GateCheck {
            name: "ok".into(),
            passed: true,
            message: "pass".into(),
            duration_ms: None,
            artifact_path: None,
        }], 100, "2026-01-01T00:00:00Z".into())
    }

    fn make_fail_report(gate: GateId) -> GateReport {
        GateReport::new(gate, vec![GateCheck {
            name: "bad".into(),
            passed: false,
            message: "fail".into(),
            duration_ms: None,
            artifact_path: None,
        }], 200, "2026-01-01T00:00:00Z".into())
    }

    // ── Rollout Stage ────────────────────────────────────────────────────

    #[test]
    fn stage_str_roundtrip() {
        for stage in &[RolloutStage::Shadow, RolloutStage::Partial, RolloutStage::Full] {
            let s = stage.as_str();
            let parsed = RolloutStage::from_str_stage(s);
            assert_eq!(parsed, Some(*stage));
        }
    }

    #[test]
    fn stage_unknown_returns_none() {
        assert_eq!(RolloutStage::from_str_stage("unknown"), None);
    }

    #[test]
    fn stage_from_elapsed_days() {
        assert_eq!(RolloutStage::from_elapsed_days(0), RolloutStage::Shadow);
        assert_eq!(RolloutStage::from_elapsed_days(13), RolloutStage::Shadow);
        assert_eq!(RolloutStage::from_elapsed_days(14), RolloutStage::Partial);
        assert_eq!(RolloutStage::from_elapsed_days(27), RolloutStage::Partial);
        assert_eq!(RolloutStage::from_elapsed_days(28), RolloutStage::Full);
        assert_eq!(RolloutStage::from_elapsed_days(100), RolloutStage::Full);
    }

    #[test]
    fn shadow_mode_no_enforcement() {
        assert!(!RolloutStage::Shadow.is_enforced(GateId::Smoke));
        assert!(!RolloutStage::Shadow.is_enforced(GateId::TestSuite));
        assert!(!RolloutStage::Shadow.is_enforced(GateId::Regression));
    }

    #[test]
    fn partial_enforces_gate1_and_gate2() {
        assert!(RolloutStage::Partial.is_enforced(GateId::Smoke));
        assert!(RolloutStage::Partial.is_enforced(GateId::TestSuite));
        assert!(!RolloutStage::Partial.is_enforced(GateId::Regression));
    }

    #[test]
    fn full_enforces_all() {
        for gate in &ALL_GATES {
            assert!(RolloutStage::Full.is_enforced(*gate));
        }
    }

    // ── Rollout Config ───────────────────────────────────────────────────

    #[test]
    fn default_config_values() {
        let config = RolloutConfig::default();
        assert_eq!(config.stage, RolloutStage::Shadow);
        assert!(!config.kill_switch);
        assert_eq!(config.shadow_days, DEFAULT_SHADOW_DAYS);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = RolloutConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: RolloutConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, config);
    }

    #[test]
    fn kill_switch_env_false() {
        assert!(RolloutConfig::is_enforcement_disabled_by_env(Some("false")));
        assert!(RolloutConfig::is_enforcement_disabled_by_env(Some("0")));
        assert!(RolloutConfig::is_enforcement_disabled_by_env(Some("no")));
    }

    #[test]
    fn kill_switch_env_true() {
        assert!(!RolloutConfig::is_enforcement_disabled_by_env(Some("true")));
        assert!(!RolloutConfig::is_enforcement_disabled_by_env(Some("1")));
        assert!(!RolloutConfig::is_enforcement_disabled_by_env(None));
    }

    // ── Enforcement Decision ─────────────────────────────────────────────

    #[test]
    fn shadow_mode_fail_not_blocked() {
        let config = RolloutConfig { stage: RolloutStage::Shadow, ..Default::default() };
        let report = make_fail_report(GateId::Smoke);
        let decision = evaluate_enforcement(&config, &report, None);
        assert!(!decision.pr_blocked);
        assert!(!decision.enforced);
        assert_eq!(decision.mode, EnforcementMode::Shadow);
    }

    #[test]
    fn enforce_mode_fail_blocked() {
        let config = RolloutConfig { stage: RolloutStage::Full, ..Default::default() };
        let report = make_fail_report(GateId::Smoke);
        let decision = evaluate_enforcement(&config, &report, None);
        assert!(decision.pr_blocked);
        assert!(decision.enforced);
        assert_eq!(decision.mode, EnforcementMode::Enforce);
    }

    #[test]
    fn enforce_mode_pass_not_blocked() {
        let config = RolloutConfig { stage: RolloutStage::Full, ..Default::default() };
        let report = make_pass_report(GateId::Smoke);
        let decision = evaluate_enforcement(&config, &report, None);
        assert!(!decision.pr_blocked);
        assert!(decision.enforced);
    }

    #[test]
    fn kill_switch_overrides_enforcement() {
        let config = RolloutConfig { stage: RolloutStage::Full, kill_switch: true, ..Default::default() };
        let report = make_fail_report(GateId::Smoke);
        let decision = evaluate_enforcement(&config, &report, None);
        assert!(!decision.pr_blocked);
        assert!(!decision.enforced);
        assert_eq!(decision.mode, EnforcementMode::KillSwitch);
    }

    #[test]
    fn kill_switch_env_overrides() {
        let config = RolloutConfig { stage: RolloutStage::Full, ..Default::default() };
        let report = make_fail_report(GateId::Smoke);
        let decision = evaluate_enforcement(&config, &report, Some("false"));
        assert!(!decision.pr_blocked);
        assert_eq!(decision.mode, EnforcementMode::KillSwitch);
    }

    #[test]
    fn partial_stage_gate3_shadow() {
        let config = RolloutConfig { stage: RolloutStage::Partial, ..Default::default() };
        let report = make_fail_report(GateId::Regression);
        let decision = evaluate_enforcement(&config, &report, None);
        assert!(!decision.pr_blocked); // Gate 3 is shadow in partial
        assert!(!decision.enforced);
        assert_eq!(decision.mode, EnforcementMode::Shadow);
    }

    #[test]
    fn partial_stage_gate1_enforced() {
        let config = RolloutConfig { stage: RolloutStage::Partial, ..Default::default() };
        let report = make_fail_report(GateId::Smoke);
        let decision = evaluate_enforcement(&config, &report, None);
        assert!(decision.pr_blocked);
        assert!(decision.enforced);
    }

    #[test]
    fn enforcement_decision_serde_roundtrip() {
        let config = RolloutConfig { stage: RolloutStage::Full, ..Default::default() };
        let report = make_fail_report(GateId::Smoke);
        let decision = evaluate_enforcement(&config, &report, None);
        let json = serde_json::to_string(&decision).unwrap();
        let restored: EnforcementDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, decision);
    }

    // ── Flaky Detection ──────────────────────────────────────────────────

    #[test]
    fn flaky_rate_zero() {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, 0, &config, vec![]);
        assert!((metrics.flaky_rate - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.alert_level, AlertLevel::Normal);
    }

    #[test]
    fn flaky_rate_warning() {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, 4, &config, vec![]); // 4%
        assert_eq!(metrics.alert_level, AlertLevel::Warning);
    }

    #[test]
    fn flaky_rate_critical() {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, 6, &config, vec![]); // 6%
        assert_eq!(metrics.alert_level, AlertLevel::Critical);
    }

    #[test]
    fn flaky_rate_empty_runs() {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(0, 0, &config, vec![]);
        assert!((metrics.flaky_rate - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.alert_level, AlertLevel::Normal);
    }

    // ── Rollback Triggers ────────────────────────────────────────────────

    #[test]
    fn rollback_no_triggers() {
        let config = RolloutConfig::default();
        let flaky = calculate_flaky_rate(100, 1, &config, vec![]);
        let durations = BTreeMap::from([
            ("smoke".into(), (25u64, 30u64)),
        ]);
        let trigger = evaluate_rollback_triggers(&flaky, &durations, &config);
        assert!(!trigger.triggered);
        assert!(trigger.reasons.is_empty());
    }

    #[test]
    fn rollback_flaky_trigger() {
        let config = RolloutConfig::default();
        let flaky = calculate_flaky_rate(100, 10, &config, vec![]); // 10%
        let durations = BTreeMap::new();
        let trigger = evaluate_rollback_triggers(&flaky, &durations, &config);
        assert!(trigger.triggered);
        assert!(trigger.reasons[0].contains("Flaky rate"));
    }

    #[test]
    fn rollback_duration_trigger() {
        let config = RolloutConfig::default();
        let flaky = calculate_flaky_rate(100, 0, &config, vec![]);
        let durations = BTreeMap::from([
            ("smoke".into(), (70u64, 30u64)), // 2.33x > 2x
        ]);
        let trigger = evaluate_rollback_triggers(&flaky, &durations, &config);
        assert!(trigger.triggered);
        assert!(trigger.reasons[0].contains("duration"));
    }

    #[test]
    fn rollback_both_triggers() {
        let config = RolloutConfig::default();
        let flaky = calculate_flaky_rate(100, 10, &config, vec![]);
        let durations = BTreeMap::from([
            ("smoke".into(), (70u64, 30u64)),
        ]);
        let trigger = evaluate_rollback_triggers(&flaky, &durations, &config);
        assert!(trigger.triggered);
        assert_eq!(trigger.reasons.len(), 2);
    }

    // ── Rollback Drill ───────────────────────────────────────────────────

    #[test]
    fn drill_all_pass() {
        let result = evaluate_rollback_drill(true, true, true, 5000, "now");
        assert!(result.success);
        assert_eq!(result.steps.len(), 3);
        assert_eq!(result.drill_type, DrillType::Rollback);
    }

    #[test]
    fn drill_breakage_detected() {
        let result = evaluate_rollback_drill(true, true, false, 5000, "now");
        assert!(!result.success);
    }

    #[test]
    fn drill_disable_failed() {
        let result = evaluate_rollback_drill(false, true, true, 5000, "now");
        assert!(!result.success);
    }

    #[test]
    fn drill_serde_roundtrip() {
        let result = evaluate_rollback_drill(true, true, true, 1000, "2026-01-01T00:00:00Z");
        let json = serde_json::to_string(&result).unwrap();
        let restored: DrillResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, result);
    }

    // ── Monitoring ───────────────────────────────────────────────────────

    #[test]
    fn weekly_digest_contains_stage() {
        let metrics = RolloutMetrics {
            stage: RolloutStage::Shadow,
            gate_pass_rate: BTreeMap::from([("smoke".into(), 0.95)]),
            gate_duration_trend: BTreeMap::new(),
            flaky_rate: 0.02,
            artifact_count: 100,
            days_in_stage: 7,
        };
        let digest = weekly_digest(&metrics);
        assert!(digest.contains("shadow"));
        assert!(digest.contains("95.0%"));
        assert!(digest.contains("2.0%"));
    }

    #[test]
    fn weekly_digest_empty_gates() {
        let metrics = RolloutMetrics {
            stage: RolloutStage::Full,
            gate_pass_rate: BTreeMap::new(),
            gate_duration_trend: BTreeMap::new(),
            flaky_rate: 0.0,
            artifact_count: 0,
            days_in_stage: 30,
        };
        let digest = weekly_digest(&metrics);
        assert!(digest.contains("full"));
        assert!(digest.contains("30"));
    }

    #[test]
    fn rollout_metrics_serde_roundtrip() {
        let metrics = RolloutMetrics {
            stage: RolloutStage::Partial,
            gate_pass_rate: BTreeMap::from([("smoke".into(), 1.0)]),
            gate_duration_trend: BTreeMap::new(),
            flaky_rate: 0.01,
            artifact_count: 50,
            days_in_stage: 14,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let restored: RolloutMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, metrics);
    }

    // ── Rollback Trigger serde ───────────────────────────────────────────

    #[test]
    fn rollback_trigger_serde_roundtrip() {
        let trigger = RollbackTrigger {
            triggered: true,
            reasons: vec!["test reason".into()],
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let restored: RollbackTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, trigger);
    }

    // ── Alert Level ──────────────────────────────────────────────────────

    #[test]
    fn alert_level_serde() {
        for level in &[AlertLevel::Normal, AlertLevel::Warning, AlertLevel::Critical] {
            let json = serde_json::to_string(level).unwrap();
            let restored: AlertLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(&restored, level);
        }
    }
}
