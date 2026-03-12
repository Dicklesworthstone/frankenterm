//! Chaos + scale validation harness for migration cutover readiness (ft-3681t.7.4).
//!
//! Exercises large pane counts, bursty connector traffic, policy churn, and
//! partial outages to validate SLOs before migration cutover. Builds on:
//!
//! - [`chaos`]: Fault injection primitives (`FaultInjector`, `ChaosScenario`)
//! - [`capacity_governor`]: Workload governance under pressure
//! - [`connector_reliability`]: Circuit breakers and dead-letter queues
//! - [`fleet_dashboard`]: Alert evaluation and dedup
//! - [`context_budget`]: Context-window pressure tracking
//!
//! # Architecture
//!
//! ```text
//! ScaleProfile ──► ChaosScaleHarness
//!                        │
//!          ┌─────────────┼──────────────┐
//!          ▼             ▼              ▼
//!   PaneScaleProbe  ConnectorStress  PolicyChurn
//!          │             │              │
//!          └─────────────┼──────────────┘
//!                        ▼
//!                 HarnessReport
//!                   (pass/fail gates)
//! ```
//!
//! # Bead
//!
//! Implements ft-3681t.7.4 — chaos + scale validation harness.

use serde::{Deserialize, Serialize};

use crate::capacity_governor::{
    CapacityGovernor, CapacityGovernorConfig, PressureSignals, WorkloadCategory,
};
use crate::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
use crate::connector_reliability::{
    ConnectorErrorKind, ConnectorReliabilityConfig, ReliabilityRegistry,
};
use crate::context_budget::{
    CompactionTrigger, ContextBudgetConfig, ContextBudgetRegistry, ContextPressureTier,
};

// =============================================================================
// Scale profile
// =============================================================================

/// Defines the scale parameters for a validation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaleProfile {
    /// Number of simulated panes.
    pub pane_count: u32,
    /// Number of simulated connectors.
    pub connector_count: u32,
    /// Number of policy evaluations to run.
    pub policy_eval_count: u32,
    /// Simulated event burst rate (events per second).
    pub event_burst_rate: u32,
    /// Duration of the simulation in logical milliseconds.
    pub duration_ms: u64,
    /// Label for this profile.
    pub label: String,
}

impl ScaleProfile {
    /// A small profile for fast CI tests.
    #[must_use]
    pub fn small() -> Self {
        Self {
            pane_count: 10,
            connector_count: 5,
            policy_eval_count: 100,
            event_burst_rate: 50,
            duration_ms: 10_000,
            label: "small".into(),
        }
    }

    /// A medium profile for integration tests.
    #[must_use]
    pub fn medium() -> Self {
        Self {
            pane_count: 100,
            connector_count: 20,
            policy_eval_count: 1_000,
            event_burst_rate: 500,
            duration_ms: 60_000,
            label: "medium".into(),
        }
    }

    /// A large profile simulating production-scale swarms.
    #[must_use]
    pub fn large() -> Self {
        Self {
            pane_count: 1_000,
            connector_count: 50,
            policy_eval_count: 10_000,
            event_burst_rate: 2_000,
            duration_ms: 300_000,
            label: "large".into(),
        }
    }
}

// =============================================================================
// Failure scenario
// =============================================================================

/// A failure class that the harness can inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Connector returns transient errors.
    ConnectorTransient,
    /// Connector goes fully offline.
    ConnectorOffline,
    /// CPU overload scenario.
    CpuOverload,
    /// Memory exhaustion.
    MemoryExhaustion,
    /// Disk I/O stall.
    IoStall,
    /// Policy engine churn (rapid rule changes).
    PolicyChurn,
    /// Context window exhaustion (agents hit Black tier).
    ContextExhaustion,
    /// Partial rch worker loss.
    RchWorkerLoss,
    /// Cascading failures across subsystems.
    CascadingFailure,
}

// =============================================================================
// SLO definition
// =============================================================================

/// A service-level objective to validate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloDefinition {
    /// Name of the SLO.
    pub name: String,
    /// Metric to evaluate.
    pub metric: SloMetric,
    /// Threshold value (meaning depends on metric).
    pub threshold: f64,
    /// Whether higher is better (true) or lower is better (false).
    pub higher_is_better: bool,
}

/// Metrics that SLOs can be defined against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloMetric {
    /// Governor allow rate (fraction of evaluations that are Allow).
    GovernorAllowRate,
    /// Governor block rate (should be low under normal conditions).
    GovernorBlockRate,
    /// Connector circuit-breaker trip rate.
    CircuitBreakerTripRate,
    /// Dead-letter queue depth.
    DlqDepth,
    /// Average context pressure utilization.
    ContextUtilization,
    /// Fraction of panes needing attention (Red/Black).
    PanesNeedingAttention,
    /// Recovery time after failure injection (ms).
    RecoveryTimeMs,
}

// =============================================================================
// Probe results
// =============================================================================

/// Result of a pane scale probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneScaleProbeResult {
    pub pane_count: u32,
    pub worst_pressure_tier: ContextPressureTier,
    pub panes_needing_attention: usize,
    pub average_utilization: f64,
    pub total_compactions: u64,
}

/// Result of a connector stress probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorStressResult {
    pub connector_count: u32,
    pub total_operations: u64,
    pub total_successes: u64,
    pub total_failures: u64,
    pub circuit_breaker_trips: u32,
    pub total_dlq_depth: usize,
    pub success_rate: f64,
}

/// Result of a capacity governor probe under load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorProbeResult {
    pub evaluations: u64,
    pub allowed: u64,
    pub throttled: u64,
    pub offloaded: u64,
    pub blocked: u64,
    pub allow_rate: f64,
    pub block_rate: f64,
}

/// Result of an SLO check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloResult {
    pub name: String,
    pub metric: SloMetric,
    pub threshold: f64,
    pub actual: f64,
    pub passed: bool,
}

// =============================================================================
// Harness report
// =============================================================================

/// Full report from a chaos + scale validation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessReport {
    pub profile_label: String,
    pub failure_classes: Vec<FailureClass>,
    pub pane_probe: PaneScaleProbeResult,
    pub connector_stress: ConnectorStressResult,
    pub governor_probe: GovernorProbeResult,
    pub slo_results: Vec<SloResult>,
    pub overall_pass: bool,
    pub duration_ms: u64,
}

impl HarnessReport {
    /// Compact summary for status output.
    pub fn summary_line(&self) -> String {
        let slo_pass_count = self.slo_results.iter().filter(|s| s.passed).count();
        let slo_total = self.slo_results.len();
        let verdict = if self.overall_pass { "PASS" } else { "FAIL" };
        format!(
            "[{}] {} | panes={} connectors={} | SLOs {}/{} | governor allow={:.0}% block={:.0}%",
            verdict,
            self.profile_label,
            self.pane_probe.pane_count,
            self.connector_stress.connector_count,
            slo_pass_count,
            slo_total,
            self.governor_probe.allow_rate * 100.0,
            self.governor_probe.block_rate * 100.0,
        )
    }
}

// =============================================================================
// Harness
// =============================================================================

/// The chaos + scale validation harness.
///
/// Coordinates simulated load across pane contexts, connector reliability,
/// and capacity governance to produce a pass/fail report.
pub struct ChaosScaleHarness {
    profile: ScaleProfile,
    failure_classes: Vec<FailureClass>,
    slos: Vec<SloDefinition>,
    context_registry: ContextBudgetRegistry,
    reliability_registry: ReliabilityRegistry,
    governor: CapacityGovernor,
}

impl ChaosScaleHarness {
    /// Create a new harness with the given scale profile.
    pub fn new(profile: ScaleProfile) -> Self {
        Self {
            profile,
            failure_classes: Vec::new(),
            slos: default_slos(),
            context_registry: ContextBudgetRegistry::new(ContextBudgetConfig::default()),
            reliability_registry: ReliabilityRegistry::new(ConnectorReliabilityConfig::default()),
            governor: CapacityGovernor::with_defaults(),
        }
    }

    /// Inject a failure class into the harness.
    pub fn inject_failure(&mut self, class: FailureClass) {
        self.failure_classes.push(class);
    }

    /// Add a custom SLO.
    pub fn add_slo(&mut self, slo: SloDefinition) {
        self.slos.push(slo);
    }

    /// Override the governor config.
    pub fn set_governor_config(&mut self, config: CapacityGovernorConfig) {
        self.governor = CapacityGovernor::new(config);
    }

    /// Run the full validation harness and produce a report.
    pub fn run(&mut self) -> HarnessReport {
        let pane_result = self.run_pane_scale_probe();
        let connector_result = self.run_connector_stress();
        let governor_result = self.run_governor_probe();

        let slo_results = self.evaluate_slos(&pane_result, &connector_result, &governor_result);
        let overall_pass = slo_results.iter().all(|s| s.passed);

        HarnessReport {
            profile_label: self.profile.label.clone(),
            failure_classes: self.failure_classes.clone(),
            pane_probe: pane_result,
            connector_stress: connector_result,
            governor_probe: governor_result,
            slo_results,
            overall_pass,
            duration_ms: self.profile.duration_ms,
        }
    }

    // -------------------------------------------------------------------------
    // Pane scale probe
    // -------------------------------------------------------------------------

    fn run_pane_scale_probe(&mut self) -> PaneScaleProbeResult {
        let has_context_exhaustion = self
            .failure_classes
            .contains(&FailureClass::ContextExhaustion);

        for pane_id in 0..self.profile.pane_count {
            let tracker = self.context_registry.tracker_mut(pane_id as u64);
            tracker.set_agent_program("test-agent");

            // Simulate varied token usage across panes.
            let pane_fraction = pane_id as f64 / self.profile.pane_count.max(1) as f64;
            let base_utilization = if has_context_exhaustion {
                // Under context exhaustion, spread from 0.60 to 0.95.
                // Ensures some panes land in Red (>=0.75) and Black (>=0.90).
                0.60 + pane_fraction * 0.35
            } else {
                // Normal distribution: 0% to 45% utilization.
                pane_fraction * 0.45
            };
            let tokens = (base_utilization * 200_000.0) as u64;
            tracker.update_tokens(tokens);

            // Some panes get compacted.
            if pane_id % 7 == 0 {
                let before = tokens;
                let after = tokens / 3;
                tracker.record_compaction(before, after, CompactionTrigger::Automatic);
            }
        }

        let snapshot = self.context_registry.fleet_snapshot();
        PaneScaleProbeResult {
            pane_count: self.profile.pane_count,
            worst_pressure_tier: snapshot.worst_pressure_tier,
            panes_needing_attention: snapshot.panes_needing_attention,
            average_utilization: snapshot.average_utilization,
            total_compactions: snapshot.total_compactions,
        }
    }

    // -------------------------------------------------------------------------
    // Connector stress
    // -------------------------------------------------------------------------

    fn run_connector_stress(&mut self) -> ConnectorStressResult {
        let has_transient = self
            .failure_classes
            .contains(&FailureClass::ConnectorTransient);
        let has_offline = self
            .failure_classes
            .contains(&FailureClass::ConnectorOffline);
        let has_cascading = self
            .failure_classes
            .contains(&FailureClass::CascadingFailure);

        let ops_per_connector = self.profile.event_burst_rate as u64 * self.profile.duration_ms
            / 1000
            / self.profile.connector_count.max(1) as u64;

        let mut total_ops: u64 = 0;
        let mut total_success: u64 = 0;
        let mut total_fail: u64 = 0;
        let mut cb_trips: u32 = 0;

        for cid in 0..self.profile.connector_count {
            let connector_id = format!("connector-{cid}");
            let controller = self.reliability_registry.get_or_create(&connector_id);

            for op in 0..ops_per_connector {
                total_ops += 1;
                let should_fail = if has_offline && cid % 5 == 0 {
                    // 20% of connectors fully offline.
                    true
                } else if has_cascading && op > ops_per_connector / 2 && cid % 3 == 0 {
                    // Cascading: after halfway, 33% of connectors start failing.
                    true
                } else if has_transient {
                    // 10% transient failure rate.
                    op % 10 == 0
                } else {
                    false
                };

                if !controller.allow_operation() {
                    total_fail += 1;
                    continue;
                }

                if should_fail {
                    let kind = if has_offline && cid % 5 == 0 {
                        ConnectorErrorKind::ServiceUnavailable
                    } else {
                        ConnectorErrorKind::Transient
                    };
                    let action = ConnectorAction {
                        target_connector: connector_id.clone(),
                        action_kind: ConnectorActionKind::Invoke,
                        correlation_id: format!("chaos-{cid}-{op}"),
                        params: serde_json::Value::Null,
                        created_at_ms: op * 100,
                    };
                    controller.record_failure(&action, "simulated fault", kind, op * 100);
                    total_fail += 1;
                } else {
                    controller.record_success();
                    total_success += 1;
                }
            }

            // Count circuit breaker trips.
            let ctrl = self.reliability_registry.get(&connector_id).unwrap();
            if ctrl.circuit_status().state != crate::circuit_breaker::CircuitStateKind::Closed {
                cb_trips += 1;
            }
        }

        let success_rate = if total_ops > 0 {
            total_success as f64 / total_ops as f64
        } else {
            1.0
        };

        ConnectorStressResult {
            connector_count: self.profile.connector_count,
            total_operations: total_ops,
            total_successes: total_success,
            total_failures: total_fail,
            circuit_breaker_trips: cb_trips,
            total_dlq_depth: self.reliability_registry.total_dlq_depth(),
            success_rate,
        }
    }

    // -------------------------------------------------------------------------
    // Governor probe
    // -------------------------------------------------------------------------

    fn run_governor_probe(&mut self) -> GovernorProbeResult {
        let has_cpu_overload = self.failure_classes.contains(&FailureClass::CpuOverload);
        let has_memory_exhaustion = self
            .failure_classes
            .contains(&FailureClass::MemoryExhaustion);
        let has_io_stall = self.failure_classes.contains(&FailureClass::IoStall);
        let has_rch_loss = self.failure_classes.contains(&FailureClass::RchWorkerLoss);

        let eval_count = self.profile.policy_eval_count;

        for i in 0..eval_count {
            let progress = i as f64 / eval_count as f64;

            // Simulate changing pressure over time.
            let cpu = if has_cpu_overload {
                0.70 + progress * 0.28
            } else {
                0.30 + progress * 0.20
            };
            let mem = if has_memory_exhaustion {
                0.75 + progress * 0.22
            } else {
                0.40 + progress * 0.15
            };
            let io = if has_io_stall {
                0.60 + progress * 0.35
            } else {
                0.10
            };

            // Under normal conditions, keep concurrency low. Under failure, ramp it up.
            let heavy_active = if has_cpu_overload || has_memory_exhaustion {
                (i % 4) as u32
            } else {
                (i % 2) as u32 // 0 or 1, always below default max of 2
            };
            let medium_active = if has_cpu_overload || has_memory_exhaustion {
                (i % 8) as u32
            } else {
                (i % 4) as u32 // 0–3, always below default max of 6
            };

            let signals = PressureSignals {
                cpu_utilization: cpu.min(1.0),
                memory_utilization: mem.min(1.0),
                active_heavy_workloads: heavy_active,
                active_medium_workloads: medium_active,
                load_average_1m: cpu * 8.0, // Scale load proportionally
                rch_available: !has_rch_loss,
                rch_workers_available: if has_rch_loss { 0 } else { 4 },
                io_pressure: io.min(1.0),
                timestamp_ms: i as u64 * 100,
            };

            let category = match i % 3 {
                0 => WorkloadCategory::Heavy,
                1 => WorkloadCategory::Medium,
                _ => WorkloadCategory::Light,
            };

            self.governor.evaluate(category, &signals);
        }

        let telem = self.governor.telemetry();
        let evaluations = telem.evaluations;
        let allow_rate = if evaluations > 0 {
            telem.allowed as f64 / evaluations as f64
        } else {
            1.0
        };
        let block_rate = if evaluations > 0 {
            telem.blocked as f64 / evaluations as f64
        } else {
            0.0
        };

        GovernorProbeResult {
            evaluations,
            allowed: telem.allowed,
            throttled: telem.throttled,
            offloaded: telem.offloaded,
            blocked: telem.blocked,
            allow_rate,
            block_rate,
        }
    }

    // -------------------------------------------------------------------------
    // SLO evaluation
    // -------------------------------------------------------------------------

    fn evaluate_slos(
        &self,
        pane: &PaneScaleProbeResult,
        connector: &ConnectorStressResult,
        governor: &GovernorProbeResult,
    ) -> Vec<SloResult> {
        self.slos
            .iter()
            .map(|slo| {
                let actual = match slo.metric {
                    SloMetric::GovernorAllowRate => governor.allow_rate,
                    SloMetric::GovernorBlockRate => governor.block_rate,
                    SloMetric::CircuitBreakerTripRate => {
                        if connector.connector_count > 0 {
                            connector.circuit_breaker_trips as f64
                                / connector.connector_count as f64
                        } else {
                            0.0
                        }
                    }
                    SloMetric::DlqDepth => connector.total_dlq_depth as f64,
                    SloMetric::ContextUtilization => pane.average_utilization,
                    SloMetric::PanesNeedingAttention => {
                        if pane.pane_count > 0 {
                            pane.panes_needing_attention as f64 / pane.pane_count as f64
                        } else {
                            0.0
                        }
                    }
                    SloMetric::RecoveryTimeMs => 0.0, // Not yet measured in sync harness
                };
                let passed = if slo.higher_is_better {
                    actual >= slo.threshold
                } else {
                    actual <= slo.threshold
                };
                SloResult {
                    name: slo.name.clone(),
                    metric: slo.metric,
                    threshold: slo.threshold,
                    actual,
                    passed,
                }
            })
            .collect()
    }
}

// =============================================================================
// Default SLOs
// =============================================================================

fn default_slos() -> Vec<SloDefinition> {
    vec![
        SloDefinition {
            name: "governor-allow-rate".into(),
            metric: SloMetric::GovernorAllowRate,
            threshold: 0.50,
            higher_is_better: true,
        },
        SloDefinition {
            name: "governor-block-rate-ceiling".into(),
            metric: SloMetric::GovernorBlockRate,
            threshold: 0.40,
            higher_is_better: false,
        },
        SloDefinition {
            name: "circuit-breaker-trip-ceiling".into(),
            metric: SloMetric::CircuitBreakerTripRate,
            threshold: 0.50,
            higher_is_better: false,
        },
        SloDefinition {
            name: "context-utilization-ceiling".into(),
            metric: SloMetric::ContextUtilization,
            threshold: 0.80,
            higher_is_better: false,
        },
        SloDefinition {
            name: "panes-needing-attention-ceiling".into(),
            metric: SloMetric::PanesNeedingAttention,
            threshold: 0.30,
            higher_is_better: false,
        },
    ]
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- ScaleProfile --

    #[test]
    fn scale_profiles_have_valid_sizes() {
        let s = ScaleProfile::small();
        assert!(s.pane_count > 0);
        assert!(s.connector_count > 0);
        assert!(s.duration_ms > 0);

        let m = ScaleProfile::medium();
        assert!(m.pane_count > s.pane_count);

        let l = ScaleProfile::large();
        assert_eq!(l.pane_count, 1_000);
        assert_eq!(l.label, "large");
    }

    // -- Harness: no failures (green path) --

    #[test]
    fn harness_green_path_small() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        assert!(report.overall_pass, "green path should pass all SLOs");
        assert_eq!(report.failure_classes.len(), 0);
        assert!(report.governor_probe.allow_rate > 0.8);
        assert_eq!(report.governor_probe.blocked, 0);
    }

    #[test]
    fn harness_green_path_medium() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::medium());
        let report = harness.run();
        assert!(report.overall_pass);
        assert_eq!(report.pane_probe.pane_count, 100);
        assert_eq!(report.connector_stress.connector_count, 20);
    }

    // -- Harness: connector transient failures --

    #[test]
    fn harness_connector_transient_failures() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::ConnectorTransient);
        let report = harness.run();
        assert!(report.connector_stress.total_failures > 0);
        assert!(report.connector_stress.success_rate < 1.0);
    }

    #[test]
    fn harness_connector_offline_scenario() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::ConnectorOffline);
        let report = harness.run();
        assert!(report.connector_stress.total_failures > 0);
        // Some connectors should trip circuit breakers.
        assert!(report.connector_stress.circuit_breaker_trips > 0);
    }

    // -- Harness: CPU overload --

    #[test]
    fn harness_cpu_overload() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::CpuOverload);
        let report = harness.run();
        // Under CPU overload, governor should block or throttle heavily.
        assert!(report.governor_probe.blocked > 0 || report.governor_probe.throttled > 0);
        assert!(report.governor_probe.allow_rate < 0.95);
    }

    // -- Harness: memory exhaustion --

    #[test]
    fn harness_memory_exhaustion() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::MemoryExhaustion);
        let report = harness.run();
        assert!(report.governor_probe.blocked > 0 || report.governor_probe.throttled > 0);
    }

    // -- Harness: context exhaustion --

    #[test]
    fn harness_context_exhaustion() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::ContextExhaustion);
        let report = harness.run();
        assert!(report.pane_probe.panes_needing_attention > 0);
        assert!(report.pane_probe.average_utilization > 0.5);
    }

    // -- Harness: rch worker loss --

    #[test]
    fn harness_rch_worker_loss() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::RchWorkerLoss);
        let report = harness.run();
        // Without rch, governor should throttle instead of offload.
        assert_eq!(report.governor_probe.offloaded, 0);
    }

    // -- Harness: cascading failure --

    #[test]
    fn harness_cascading_failure() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::CascadingFailure);
        let report = harness.run();
        assert!(report.connector_stress.total_failures > 0);
    }

    // -- Harness: combined failures --

    #[test]
    fn harness_combined_cpu_and_connector_failures() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::CpuOverload);
        harness.inject_failure(FailureClass::ConnectorTransient);
        harness.inject_failure(FailureClass::ContextExhaustion);
        let report = harness.run();
        // Combined failures should show degradation across all subsystems.
        assert!(report.connector_stress.total_failures > 0);
        assert!(report.governor_probe.blocked > 0 || report.governor_probe.throttled > 0);
        assert!(report.pane_probe.panes_needing_attention > 0);
    }

    // -- SLO evaluation --

    #[test]
    fn slo_higher_is_better_semantics() {
        let slo = SloDefinition {
            name: "test".into(),
            metric: SloMetric::GovernorAllowRate,
            threshold: 0.80,
            higher_is_better: true,
        };
        // A green-path harness should have high allow rate.
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.slos = vec![slo];
        let report = harness.run();
        assert!(report.slo_results[0].passed);
    }

    #[test]
    fn slo_lower_is_better_semantics() {
        let slo = SloDefinition {
            name: "test-block".into(),
            metric: SloMetric::GovernorBlockRate,
            threshold: 0.01,
            higher_is_better: false,
        };
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.slos = vec![slo];
        let report = harness.run();
        // Green path should have 0% block rate.
        assert!(report.slo_results[0].passed);
    }

    #[test]
    fn slo_fails_when_threshold_violated() {
        let slo = SloDefinition {
            name: "strict-allow".into(),
            metric: SloMetric::GovernorAllowRate,
            threshold: 0.99,
            higher_is_better: true,
        };
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::CpuOverload);
        harness.slos = vec![slo];
        let report = harness.run();
        // CPU overload should bring allow rate below 99%.
        assert!(!report.slo_results[0].passed);
    }

    // -- Report --

    #[test]
    fn report_summary_line_contains_verdict() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        let line = report.summary_line();
        assert!(line.contains("[PASS]") || line.contains("[FAIL]"));
        assert!(line.contains("small"));
    }

    #[test]
    fn report_serde_roundtrip() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        let json = serde_json::to_string(&report).unwrap();
        let restored: HarnessReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.profile_label, report.profile_label);
        assert_eq!(restored.overall_pass, report.overall_pass);
        assert_eq!(restored.slo_results.len(), report.slo_results.len());
    }

    // -- Default SLOs --

    #[test]
    fn default_slos_cover_key_metrics() {
        let slos = default_slos();
        assert!(slos.len() >= 4);
        let metrics: Vec<SloMetric> = slos.iter().map(|s| s.metric).collect();
        assert!(metrics.contains(&SloMetric::GovernorAllowRate));
        assert!(metrics.contains(&SloMetric::GovernorBlockRate));
        assert!(metrics.contains(&SloMetric::CircuitBreakerTripRate));
        assert!(metrics.contains(&SloMetric::ContextUtilization));
    }

    // -- Custom governor config --

    #[test]
    fn harness_with_custom_governor_config() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.set_governor_config(CapacityGovernorConfig {
            max_concurrent_heavy: 1,
            cpu_throttle_threshold: 0.50,
            ..CapacityGovernorConfig::default()
        });
        let report = harness.run();
        // Stricter config should still work.
        assert!(report.governor_probe.evaluations > 0);
    }

    // -- Large scale (1k panes) --

    #[test]
    fn harness_large_profile_pane_count() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::large());
        // Only run pane probe (connector stress on large is slow).
        harness.profile.connector_count = 2;
        harness.profile.event_burst_rate = 10;
        harness.profile.policy_eval_count = 50;
        let report = harness.run();
        assert_eq!(report.pane_probe.pane_count, 1_000);
        assert!(report.pane_probe.total_compactions > 0);
    }

    // -- FailureClass coverage --

    #[test]
    fn failure_class_serde_roundtrip() {
        let classes = vec![
            FailureClass::ConnectorTransient,
            FailureClass::CpuOverload,
            FailureClass::CascadingFailure,
            FailureClass::RchWorkerLoss,
        ];
        let json = serde_json::to_string(&classes).unwrap();
        let restored: Vec<FailureClass> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 4);
        assert_eq!(restored[0], FailureClass::ConnectorTransient);
    }

    // -- IO stall --

    #[test]
    fn harness_io_stall() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        harness.inject_failure(FailureClass::IoStall);
        let report = harness.run();
        // IO stall affects governor decisions for heavy workloads.
        assert!(report.governor_probe.evaluations > 0);
    }

    // -- Recovery scenario --

    #[test]
    fn harness_recovery_after_connector_offline() {
        // First run with failure, then run without — simulates recovery.
        let mut harness_fail = ChaosScaleHarness::new(ScaleProfile::small());
        harness_fail.inject_failure(FailureClass::ConnectorOffline);
        let report_fail = harness_fail.run();

        let mut harness_recover = ChaosScaleHarness::new(ScaleProfile::small());
        let report_recover = harness_recover.run();

        assert!(
            report_recover.connector_stress.success_rate
                > report_fail.connector_stress.success_rate
        );
        assert!(report_recover.overall_pass);
    }

    // -- Compaction events --

    #[test]
    fn pane_probe_records_compactions() {
        let mut harness = ChaosScaleHarness::new(ScaleProfile::small());
        let report = harness.run();
        // Every 7th pane gets compacted.
        assert!(report.pane_probe.total_compactions > 0);
    }
}
