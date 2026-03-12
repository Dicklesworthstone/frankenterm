//! Runtime SLOs, alerts, and automated gate policies (ft-e34d9.10.7.3).
//!
//! Defines runtime-specific SLOs for the asupersync migration: cancellation
//! latency, queue backlog, task leak rate, and service recovery time.  Alert
//! policies map failure classes to escalation tiers.  Gate policies compose
//! SLO evaluations into automated pass/fail verdicts.
//!
//! # Architecture
//!
//! ```text
//! RuntimeSloSet
//!   ├── cancellation_latency  (p99 < 50ms)
//!   ├── queue_backlog_depth   (< 1000 items)
//!   ├── task_leak_rate        (< 0.001)
//!   ├── recovery_time         (p99 < 5000ms)
//!   ├── capture_loop_latency  (p95 < 20ms)
//!   └── event_delivery_loss   (< 0.0001)
//!
//! AlertPolicy
//!   ├── FailureClass → EscalationTier mapping
//!   └── breach threshold → alert action
//!
//! GatePolicy
//!   ├── evaluate(slo_results) → GateVerdict
//!   └── critical_slo_ids for Go/NoGo decisions
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::runtime_telemetry::FailureClass;
use crate::slo_conformance::{SloComparison, SloDefinition, SloMetric, SloSeverity};

// =============================================================================
// Runtime SLO set
// =============================================================================

/// Standard runtime SLO identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RuntimeSloId {
    /// Task cancellation must complete within latency target.
    CancellationLatency,
    /// Async task queue depth must stay below threshold.
    QueueBacklogDepth,
    /// Ratio of leaked tasks (spawned but never completed/cancelled).
    TaskLeakRate,
    /// Time to recover from degraded state back to healthy.
    ServiceRecoveryTime,
    /// Steady-state capture loop iteration latency.
    CaptureLoopLatency,
    /// Event delivery loss rate (events dropped / events produced).
    EventDeliveryLoss,
    /// Scheduler decision latency.
    SchedulerDecisionLatency,
    /// Runtime startup to ready latency.
    StartupLatency,
}

impl RuntimeSloId {
    /// Canonical string identifier.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CancellationLatency => "rt.cancellation_latency",
            Self::QueueBacklogDepth => "rt.queue_backlog_depth",
            Self::TaskLeakRate => "rt.task_leak_rate",
            Self::ServiceRecoveryTime => "rt.service_recovery_time",
            Self::CaptureLoopLatency => "rt.capture_loop_latency",
            Self::EventDeliveryLoss => "rt.event_delivery_loss",
            Self::SchedulerDecisionLatency => "rt.scheduler_decision_latency",
            Self::StartupLatency => "rt.startup_latency",
        }
    }

    /// All standard SLO IDs.
    #[must_use]
    pub fn all() -> &'static [RuntimeSloId] {
        &[
            Self::CancellationLatency,
            Self::QueueBacklogDepth,
            Self::TaskLeakRate,
            Self::ServiceRecoveryTime,
            Self::CaptureLoopLatency,
            Self::EventDeliveryLoss,
            Self::SchedulerDecisionLatency,
            Self::StartupLatency,
        ]
    }
}

/// A runtime SLO definition with target and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSlo {
    /// SLO identifier.
    pub id: RuntimeSloId,
    /// Human-readable description.
    pub description: String,
    /// Target value.
    pub target: f64,
    /// Comparison operator.
    pub comparison: SloComparisonOp,
    /// Unit of measurement.
    pub unit: String,
    /// Error budget (fraction of window allowed to violate).
    pub error_budget: f64,
    /// Breach severity.
    pub breach_severity: RuntimeAlertTier,
    /// Whether this SLO is critical for gate pass.
    pub critical: bool,
    /// Failure class this SLO detects.
    pub failure_class: FailureClass,
}

/// Comparison operators for SLO evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SloComparisonOp {
    /// Value must be less than or equal to target.
    LessOrEqual,
    /// Value must be less than target.
    LessThan,
    /// Value must be greater than or equal to target.
    GreaterOrEqual,
}

impl SloComparisonOp {
    /// Evaluate: does `measured` satisfy the comparison against `target`?
    #[must_use]
    pub fn evaluate(&self, measured: f64, target: f64) -> bool {
        match self {
            Self::LessOrEqual => measured <= target,
            Self::LessThan => measured < target,
            Self::GreaterOrEqual => measured >= target,
        }
    }
}

/// Standard set of runtime SLOs for asupersync.
#[must_use]
pub fn standard_runtime_slos() -> Vec<RuntimeSlo> {
    vec![
        RuntimeSlo {
            id: RuntimeSloId::CancellationLatency,
            description: "Task cancellation completes within p99 target".into(),
            target: 50.0,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "ms".into(),
            error_budget: 0.01,
            breach_severity: RuntimeAlertTier::Critical,
            critical: true,
            failure_class: FailureClass::Timeout,
        },
        RuntimeSlo {
            id: RuntimeSloId::QueueBacklogDepth,
            description: "Async task queue depth below capacity threshold".into(),
            target: 1000.0,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "items".into(),
            error_budget: 0.05,
            breach_severity: RuntimeAlertTier::Warning,
            critical: false,
            failure_class: FailureClass::Overload,
        },
        RuntimeSlo {
            id: RuntimeSloId::TaskLeakRate,
            description: "Leaked task ratio (spawned but never completed/cancelled)".into(),
            target: 0.001,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "ratio".into(),
            error_budget: 0.0,
            breach_severity: RuntimeAlertTier::Critical,
            critical: true,
            failure_class: FailureClass::Deadlock,
        },
        RuntimeSlo {
            id: RuntimeSloId::ServiceRecoveryTime,
            description: "Recovery from degraded state to healthy within target".into(),
            target: 5000.0,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "ms".into(),
            error_budget: 0.02,
            breach_severity: RuntimeAlertTier::Critical,
            critical: true,
            failure_class: FailureClass::Degraded,
        },
        RuntimeSlo {
            id: RuntimeSloId::CaptureLoopLatency,
            description: "Capture loop iteration p95 within budget".into(),
            target: 20.0,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "ms".into(),
            error_budget: 0.05,
            breach_severity: RuntimeAlertTier::Warning,
            critical: false,
            failure_class: FailureClass::Degraded,
        },
        RuntimeSlo {
            id: RuntimeSloId::EventDeliveryLoss,
            description: "Event delivery loss rate below threshold".into(),
            target: 0.0001,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "ratio".into(),
            error_budget: 0.0,
            breach_severity: RuntimeAlertTier::Critical,
            critical: true,
            failure_class: FailureClass::Corruption,
        },
        RuntimeSlo {
            id: RuntimeSloId::SchedulerDecisionLatency,
            description: "Scheduler scale decision within latency budget".into(),
            target: 100.0,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "ms".into(),
            error_budget: 0.02,
            breach_severity: RuntimeAlertTier::Warning,
            critical: false,
            failure_class: FailureClass::Timeout,
        },
        RuntimeSlo {
            id: RuntimeSloId::StartupLatency,
            description: "Runtime startup to ready within target".into(),
            target: 1500.0,
            comparison: SloComparisonOp::LessOrEqual,
            unit: "ms".into(),
            error_budget: 0.01,
            breach_severity: RuntimeAlertTier::Critical,
            critical: true,
            failure_class: FailureClass::Timeout,
        },
    ]
}

// =============================================================================
// Alert tiers and policies
// =============================================================================

/// Alert escalation tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RuntimeAlertTier {
    /// Informational — log only.
    Info,
    /// Warning — approaching threshold, emit advisory.
    Warning,
    /// Critical — breached, requires attention.
    Critical,
    /// Page — breached with user impact, requires human intervention.
    Page,
}

impl RuntimeAlertTier {
    /// Convert to the existing `SloSeverity` type.
    #[must_use]
    pub fn to_slo_severity(&self) -> SloSeverity {
        match self {
            Self::Info => SloSeverity::Info,
            Self::Warning => SloSeverity::Warning,
            Self::Critical => SloSeverity::Critical,
            Self::Page => SloSeverity::Page,
        }
    }
}

/// Alert action to take on SLO breach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertAction {
    /// Log the breach for later analysis.
    Log,
    /// Emit a diagnostic event to the event bus.
    EmitEvent,
    /// Block the gate (prevent cutover/deploy).
    BlockGate,
    /// Trigger automatic remediation.
    AutoRemediate,
}

/// Policy mapping failure classes to alert tiers and actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertPolicy {
    /// Policy identifier.
    pub policy_id: String,
    /// Failure class to alert tier mapping.
    pub escalation_map: BTreeMap<String, AlertEscalation>,
}

/// Escalation configuration for a specific failure class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertEscalation {
    /// Failure class name.
    pub failure_class: String,
    /// Alert tier for first breach.
    pub initial_tier: RuntimeAlertTier,
    /// Alert tier after sustained breach (>= sustained_breach_count).
    pub sustained_tier: RuntimeAlertTier,
    /// Number of consecutive breaches before escalating.
    pub sustained_breach_count: u32,
    /// Actions on initial breach.
    pub initial_actions: Vec<AlertAction>,
    /// Actions on sustained breach.
    pub sustained_actions: Vec<AlertAction>,
}

/// Standard alert policy for asupersync runtime.
#[must_use]
pub fn standard_alert_policy() -> AlertPolicy {
    let mut map = BTreeMap::new();

    map.insert(
        "Timeout".into(),
        AlertEscalation {
            failure_class: "Timeout".into(),
            initial_tier: RuntimeAlertTier::Warning,
            sustained_tier: RuntimeAlertTier::Critical,
            sustained_breach_count: 3,
            initial_actions: vec![AlertAction::Log, AlertAction::EmitEvent],
            sustained_actions: vec![AlertAction::EmitEvent, AlertAction::BlockGate],
        },
    );

    map.insert(
        "Overload".into(),
        AlertEscalation {
            failure_class: "Overload".into(),
            initial_tier: RuntimeAlertTier::Warning,
            sustained_tier: RuntimeAlertTier::Critical,
            sustained_breach_count: 5,
            initial_actions: vec![AlertAction::Log, AlertAction::EmitEvent],
            sustained_actions: vec![AlertAction::EmitEvent, AlertAction::AutoRemediate],
        },
    );

    map.insert(
        "Deadlock".into(),
        AlertEscalation {
            failure_class: "Deadlock".into(),
            initial_tier: RuntimeAlertTier::Critical,
            sustained_tier: RuntimeAlertTier::Page,
            sustained_breach_count: 1,
            initial_actions: vec![AlertAction::EmitEvent, AlertAction::BlockGate],
            sustained_actions: vec![AlertAction::EmitEvent, AlertAction::BlockGate],
        },
    );

    map.insert(
        "Degraded".into(),
        AlertEscalation {
            failure_class: "Degraded".into(),
            initial_tier: RuntimeAlertTier::Info,
            sustained_tier: RuntimeAlertTier::Warning,
            sustained_breach_count: 10,
            initial_actions: vec![AlertAction::Log],
            sustained_actions: vec![AlertAction::EmitEvent],
        },
    );

    map.insert(
        "Corruption".into(),
        AlertEscalation {
            failure_class: "Corruption".into(),
            initial_tier: RuntimeAlertTier::Critical,
            sustained_tier: RuntimeAlertTier::Page,
            sustained_breach_count: 1,
            initial_actions: vec![AlertAction::EmitEvent, AlertAction::BlockGate],
            sustained_actions: vec![AlertAction::EmitEvent, AlertAction::BlockGate],
        },
    );

    AlertPolicy {
        policy_id: "asupersync-runtime-v1".into(),
        escalation_map: map,
    }
}

impl AlertPolicy {
    /// Look up escalation for a failure class.
    #[must_use]
    pub fn escalation_for(&self, failure_class: &FailureClass) -> Option<&AlertEscalation> {
        let key = format!("{:?}", failure_class);
        self.escalation_map.get(&key)
    }

    /// Determine the current alert tier given consecutive breach count.
    #[must_use]
    pub fn effective_tier(
        &self,
        failure_class: &FailureClass,
        consecutive_breaches: u32,
    ) -> RuntimeAlertTier {
        match self.escalation_for(failure_class) {
            Some(esc) => {
                if consecutive_breaches >= esc.sustained_breach_count {
                    esc.sustained_tier
                } else {
                    esc.initial_tier
                }
            }
            None => RuntimeAlertTier::Info,
        }
    }

    /// Determine the applicable actions given consecutive breach count.
    #[must_use]
    pub fn effective_actions(
        &self,
        failure_class: &FailureClass,
        consecutive_breaches: u32,
    ) -> Vec<AlertAction> {
        match self.escalation_for(failure_class) {
            Some(esc) => {
                if consecutive_breaches >= esc.sustained_breach_count {
                    esc.sustained_actions.clone()
                } else {
                    esc.initial_actions.clone()
                }
            }
            None => vec![AlertAction::Log],
        }
    }
}

// =============================================================================
// Gate policy and evaluation
// =============================================================================

/// Result of evaluating a single runtime SLO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloGateResult {
    /// SLO identifier.
    pub slo_id: RuntimeSloId,
    /// Measured value.
    pub measured: f64,
    /// Target value.
    pub target: f64,
    /// Whether the SLO is satisfied.
    pub satisfied: bool,
    /// Budget remaining (1.0 = full budget, 0.0 = exhausted).
    pub budget_remaining: f64,
    /// Alert tier if breached.
    pub alert_tier: Option<RuntimeAlertTier>,
    /// Failure class if breached.
    pub failure_class: Option<FailureClass>,
    /// Whether this SLO is critical for gate pass.
    pub critical: bool,
}

/// Overall gate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateVerdict {
    /// All SLOs pass — safe to proceed.
    Pass,
    /// Non-critical SLOs breached, critical all pass.
    ConditionalPass,
    /// One or more critical SLOs breached — block.
    Fail,
}

/// Complete gate evaluation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    /// Report identifier.
    pub report_id: String,
    /// When evaluation was performed.
    pub evaluated_at_ms: u64,
    /// Alert policy used.
    pub policy_id: String,
    /// Per-SLO results.
    pub results: Vec<SloGateResult>,
    /// Overall verdict.
    pub verdict: GateVerdict,
    /// Highest alert tier across all breaches.
    pub max_alert_tier: Option<RuntimeAlertTier>,
    /// Summary counts.
    pub total_slos: usize,
    pub satisfied_count: usize,
    pub breached_count: usize,
    pub critical_satisfied: usize,
    pub critical_breached: usize,
}

/// A measured sample for gate evaluation.
#[derive(Debug, Clone)]
pub struct RuntimeSloSample {
    /// SLO identifier.
    pub slo_id: RuntimeSloId,
    /// Measured value.
    pub measured: f64,
    /// Good sample count (for error budget).
    pub good_count: u64,
    /// Total sample count.
    pub total_count: u64,
}

impl GateReport {
    /// Evaluate all runtime SLOs against provided samples.
    #[must_use]
    pub fn evaluate(
        slos: &[RuntimeSlo],
        samples: &[RuntimeSloSample],
        policy: &AlertPolicy,
    ) -> Self {
        let sample_map: BTreeMap<String, &RuntimeSloSample> = samples
            .iter()
            .map(|s| (s.slo_id.as_str().to_string(), s))
            .collect();

        let mut results = Vec::new();

        for slo in slos {
            let key = slo.id.as_str().to_string();
            if let Some(sample) = sample_map.get(&key) {
                let satisfied = slo.comparison.evaluate(sample.measured, slo.target);

                let budget_remaining = if sample.total_count > 0 {
                    let good_fraction = sample.good_count as f64 / sample.total_count as f64;
                    let min_good = 1.0 - slo.error_budget;
                    if good_fraction >= min_good {
                        (good_fraction - min_good) / slo.error_budget.max(0.001)
                    } else {
                        0.0
                    }
                } else {
                    1.0
                };

                let (alert_tier, failure_class) = if !satisfied {
                    let tier = policy.effective_tier(&slo.failure_class, 1);
                    (Some(tier), Some(slo.failure_class))
                } else {
                    (None, None)
                };

                results.push(SloGateResult {
                    slo_id: slo.id,
                    measured: sample.measured,
                    target: slo.target,
                    satisfied,
                    budget_remaining,
                    alert_tier,
                    failure_class,
                    critical: slo.critical,
                });
            }
        }

        let total_slos = results.len();
        let satisfied_count = results.iter().filter(|r| r.satisfied).count();
        let breached_count = total_slos - satisfied_count;

        let critical_results: Vec<&SloGateResult> =
            results.iter().filter(|r| r.critical).collect();
        let critical_satisfied = critical_results.iter().filter(|r| r.satisfied).count();
        let critical_breached = critical_results.len() - critical_satisfied;

        let max_alert_tier = results
            .iter()
            .filter_map(|r| r.alert_tier)
            .max();

        let verdict = if breached_count == 0 {
            GateVerdict::Pass
        } else if critical_breached > 0 {
            GateVerdict::Fail
        } else {
            GateVerdict::ConditionalPass
        };

        Self {
            report_id: format!("{}-gate", policy.policy_id),
            evaluated_at_ms: 0,
            policy_id: policy.policy_id.clone(),
            results,
            verdict,
            max_alert_tier,
            total_slos,
            satisfied_count,
            breached_count,
            critical_satisfied,
            critical_breached,
        }
    }

    /// Convert runtime SLOs to the generic SloDefinition type for integration.
    #[must_use]
    pub fn to_slo_definitions(slos: &[RuntimeSlo]) -> Vec<SloDefinition> {
        slos.iter()
            .map(|s| {
                let metric = match s.id {
                    RuntimeSloId::TaskLeakRate | RuntimeSloId::EventDeliveryLoss => {
                        SloMetric::ErrorRate
                    }
                    RuntimeSloId::QueueBacklogDepth => SloMetric::QueueDepth,
                    _ => SloMetric::LatencyMs { percentile: 99 },
                };

                let comparison = match s.comparison {
                    SloComparisonOp::LessOrEqual => SloComparison::LessOrEqual,
                    SloComparisonOp::LessThan => SloComparison::LessThan,
                    SloComparisonOp::GreaterOrEqual => SloComparison::GreaterOrEqual,
                };

                SloDefinition {
                    id: s.id.as_str().to_string(),
                    name: s.description.clone(),
                    subsystem: "runtime".into(),
                    metric,
                    target: s.target,
                    comparison,
                    window_ms: 300_000, // 5-minute window
                    error_budget: s.error_budget,
                    breach_severity: s.breach_severity.to_slo_severity(),
                }
            })
            .collect()
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== Runtime SLO Gate: {} ===", self.report_id));
        lines.push(format!("Verdict: {:?}", self.verdict));
        lines.push(format!(
            "SLOs: {}/{} satisfied (critical: {}/{})",
            self.satisfied_count,
            self.total_slos,
            self.critical_satisfied,
            self.critical_satisfied + self.critical_breached,
        ));

        if let Some(tier) = self.max_alert_tier {
            lines.push(format!("Max alert tier: {:?}", tier));
        }

        lines.push(String::new());

        for result in &self.results {
            let status = if result.satisfied { "OK" } else { "BREACH" };
            let tier_label = result
                .alert_tier
                .map(|t| format!(" [{:?}]", t))
                .unwrap_or_default();
            let critical = if result.critical { " (critical)" } else { "" };
            lines.push(format!(
                "  [{}] {}: {:.4} (target: {:.4}){}{}",
                status,
                result.slo_id.as_str(),
                result.measured,
                result.target,
                tier_label,
                critical,
            ));
        }

        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn good_sample(slo_id: RuntimeSloId, measured: f64) -> RuntimeSloSample {
        RuntimeSloSample {
            slo_id,
            measured,
            good_count: 999,
            total_count: 1000,
        }
    }

    fn bad_sample(slo_id: RuntimeSloId, measured: f64) -> RuntimeSloSample {
        RuntimeSloSample {
            slo_id,
            measured,
            good_count: 500,
            total_count: 1000,
        }
    }

    #[test]
    fn standard_slos_cover_all_ids() {
        let slos = standard_runtime_slos();
        let all_ids = RuntimeSloId::all();
        for id in all_ids {
            assert!(
                slos.iter().any(|s| s.id == *id),
                "missing SLO for {:?}",
                id
            );
        }
    }

    #[test]
    fn standard_slos_have_at_least_4_critical() {
        let slos = standard_runtime_slos();
        let critical_count = slos.iter().filter(|s| s.critical).count();
        assert!(critical_count >= 4, "expected >= 4 critical SLOs, got {}", critical_count);
    }

    #[test]
    fn slo_comparison_less_or_equal() {
        assert!(SloComparisonOp::LessOrEqual.evaluate(5.0, 10.0));
        assert!(SloComparisonOp::LessOrEqual.evaluate(10.0, 10.0));
        assert!(!SloComparisonOp::LessOrEqual.evaluate(11.0, 10.0));
    }

    #[test]
    fn slo_comparison_less_than() {
        assert!(SloComparisonOp::LessThan.evaluate(5.0, 10.0));
        assert!(!SloComparisonOp::LessThan.evaluate(10.0, 10.0));
    }

    #[test]
    fn slo_comparison_greater_or_equal() {
        assert!(SloComparisonOp::GreaterOrEqual.evaluate(10.0, 5.0));
        assert!(SloComparisonOp::GreaterOrEqual.evaluate(5.0, 5.0));
        assert!(!SloComparisonOp::GreaterOrEqual.evaluate(4.0, 5.0));
    }

    #[test]
    fn alert_policy_timeout_escalation() {
        let policy = standard_alert_policy();
        let tier = policy.effective_tier(&FailureClass::Timeout, 1);
        assert_eq!(tier, RuntimeAlertTier::Warning);

        let tier = policy.effective_tier(&FailureClass::Timeout, 3);
        assert_eq!(tier, RuntimeAlertTier::Critical);
    }

    #[test]
    fn alert_policy_deadlock_immediate_critical() {
        let policy = standard_alert_policy();
        let tier = policy.effective_tier(&FailureClass::Deadlock, 1);
        assert_eq!(tier, RuntimeAlertTier::Page);
    }

    #[test]
    fn alert_policy_unknown_failure_class_returns_info() {
        let policy = standard_alert_policy();
        let tier = policy.effective_tier(&FailureClass::Safety, 1);
        assert_eq!(tier, RuntimeAlertTier::Info);
    }

    #[test]
    fn alert_actions_escalate() {
        let policy = standard_alert_policy();
        let initial = policy.effective_actions(&FailureClass::Timeout, 1);
        assert!(initial.contains(&AlertAction::Log));

        let sustained = policy.effective_actions(&FailureClass::Timeout, 5);
        assert!(sustained.contains(&AlertAction::BlockGate));
    }

    #[test]
    fn gate_report_all_pass() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();

        let samples: Vec<RuntimeSloSample> = slos
            .iter()
            .map(|s| good_sample(s.id, s.target * 0.5))
            .collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        assert_eq!(report.verdict, GateVerdict::Pass);
        assert_eq!(report.breached_count, 0);
        assert!(report.max_alert_tier.is_none());
    }

    #[test]
    fn gate_report_fail_on_critical_breach() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();

        let mut samples: Vec<RuntimeSloSample> = slos
            .iter()
            .map(|s| good_sample(s.id, s.target * 0.5))
            .collect();

        // Breach the critical cancellation latency SLO.
        if let Some(s) = samples.iter_mut().find(|s| s.slo_id == RuntimeSloId::CancellationLatency) {
            s.measured = 100.0; // target is 50ms
        }

        let report = GateReport::evaluate(&slos, &samples, &policy);
        assert_eq!(report.verdict, GateVerdict::Fail);
        assert!(report.critical_breached > 0);
    }

    #[test]
    fn gate_report_conditional_pass_on_non_critical_breach() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();

        let mut samples: Vec<RuntimeSloSample> = slos
            .iter()
            .map(|s| good_sample(s.id, s.target * 0.5))
            .collect();

        // Breach a non-critical SLO (queue backlog).
        if let Some(s) = samples.iter_mut().find(|s| s.slo_id == RuntimeSloId::QueueBacklogDepth) {
            s.measured = 2000.0; // target is 1000
        }

        let report = GateReport::evaluate(&slos, &samples, &policy);
        assert_eq!(report.verdict, GateVerdict::ConditionalPass);
        assert_eq!(report.critical_breached, 0);
        assert!(report.breached_count > 0);
    }

    #[test]
    fn gate_report_max_alert_tier_tracks_worst() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();

        let mut samples: Vec<RuntimeSloSample> = slos
            .iter()
            .map(|s| good_sample(s.id, s.target * 0.5))
            .collect();

        // Breach task leak (Deadlock class → Page tier).
        if let Some(s) = samples.iter_mut().find(|s| s.slo_id == RuntimeSloId::TaskLeakRate) {
            s.measured = 0.01; // target is 0.001
        }

        let report = GateReport::evaluate(&slos, &samples, &policy);
        assert_eq!(report.max_alert_tier, Some(RuntimeAlertTier::Page));
    }

    #[test]
    fn to_slo_definitions_converts_all() {
        let slos = standard_runtime_slos();
        let defs = GateReport::to_slo_definitions(&slos);
        assert_eq!(defs.len(), slos.len());
        for def in &defs {
            assert_eq!(def.subsystem, "runtime");
        }
    }

    #[test]
    fn render_summary_shows_verdict() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos
            .iter()
            .map(|s| good_sample(s.id, s.target * 0.5))
            .collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        let summary = report.render_summary();
        assert!(summary.contains("Pass"));
        assert!(summary.contains("OK"));
    }

    #[test]
    fn render_summary_shows_breach() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();

        let mut samples: Vec<RuntimeSloSample> = slos
            .iter()
            .map(|s| good_sample(s.id, s.target * 0.5))
            .collect();

        if let Some(s) = samples.iter_mut().find(|s| s.slo_id == RuntimeSloId::CancellationLatency) {
            s.measured = 100.0;
        }

        let report = GateReport::evaluate(&slos, &samples, &policy);
        let summary = report.render_summary();
        assert!(summary.contains("BREACH"));
        assert!(summary.contains("Fail"));
    }

    #[test]
    fn runtime_slo_id_as_str_unique() {
        let all = RuntimeSloId::all();
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a.as_str(), b.as_str());
                }
            }
        }
    }

    #[test]
    fn alert_tier_ordering() {
        assert!(RuntimeAlertTier::Info < RuntimeAlertTier::Warning);
        assert!(RuntimeAlertTier::Warning < RuntimeAlertTier::Critical);
        assert!(RuntimeAlertTier::Critical < RuntimeAlertTier::Page);
    }

    #[test]
    fn serde_roundtrip_gate_report() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples: Vec<RuntimeSloSample> = slos
            .iter()
            .map(|s| good_sample(s.id, s.target * 0.5))
            .collect();

        let report = GateReport::evaluate(&slos, &samples, &policy);
        let json = serde_json::to_string(&report).expect("serialize");
        let restored: GateReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.verdict, GateVerdict::Pass);
        assert_eq!(restored.results.len(), report.results.len());
    }

    #[test]
    fn serde_roundtrip_alert_policy() {
        let policy = standard_alert_policy();
        let json = serde_json::to_string(&policy).expect("serialize");
        let restored: AlertPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.escalation_map.len(), policy.escalation_map.len());
    }

    #[test]
    fn missing_sample_excluded_from_report() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        // Only provide 2 samples.
        let samples = vec![
            good_sample(RuntimeSloId::CancellationLatency, 10.0),
            good_sample(RuntimeSloId::TaskLeakRate, 0.0001),
        ];

        let report = GateReport::evaluate(&slos, &samples, &policy);
        assert_eq!(report.total_slos, 2);
    }

    #[test]
    fn budget_remaining_zero_when_exhausted() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples = vec![bad_sample(RuntimeSloId::CancellationLatency, 100.0)];

        let report = GateReport::evaluate(&slos, &samples, &policy);
        let result = report.results.iter().find(|r| r.slo_id == RuntimeSloId::CancellationLatency).unwrap();
        assert_eq!(result.budget_remaining, 0.0);
    }

    #[test]
    fn budget_remaining_positive_when_good() {
        let slos = standard_runtime_slos();
        let policy = standard_alert_policy();
        let samples = vec![good_sample(RuntimeSloId::CancellationLatency, 10.0)];

        let report = GateReport::evaluate(&slos, &samples, &policy);
        let result = report.results.iter().find(|r| r.slo_id == RuntimeSloId::CancellationLatency).unwrap();
        assert!(result.budget_remaining > 0.0);
    }
}
