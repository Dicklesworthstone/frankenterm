//! Runtime latency budget enforcer and percentile snapshots.

use serde::{Deserialize, Serialize};

use super::{
    BudgetNode, LatencyLogEntry, LatencyStage, Mitigation, Percentile, PipelineRun, ReasonCode,
    StageBudget, StageObservation, default_budgets, default_pipeline_tree,
};

/// Configuration for the budget enforcer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetEnforcerConfig {
    /// Per-stage budgets. If empty, default_budgets() is used.
    pub stage_budgets: Vec<StageBudget>,
    /// Pipeline composition tree. If None, default_pipeline_tree() is used.
    pub pipeline_tree: Option<BudgetNode>,
    /// Per-stage mitigation policy.
    pub mitigation_policy: Vec<StageMitigationPolicy>,
    /// Window size for percentile estimation (number of observations).
    pub window_size: usize,
    /// Whether to emit structured logs for every observation.
    pub log_all_observations: bool,
    /// Whether to emit structured logs only for overflows.
    pub log_overflows_only: bool,
}

impl Default for BudgetEnforcerConfig {
    fn default() -> Self {
        Self {
            stage_budgets: default_budgets(),
            pipeline_tree: None,
            mitigation_policy: default_mitigation_policies(),
            window_size: 1000,
            log_all_observations: false,
            log_overflows_only: true,
        }
    }
}

/// Mitigation policy for a specific stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageMitigationPolicy {
    pub stage: LatencyStage,
    /// Which mitigation to apply when the stage overflows at p95.
    pub on_p95_overflow: Mitigation,
    /// Which mitigation to apply when the stage overflows at p99.
    pub on_p99_overflow: Mitigation,
    /// Which mitigation to apply when the stage overflows at p999.
    pub on_p999_overflow: Mitigation,
}

/// Default mitigation policies for each stage.
pub fn default_mitigation_policies() -> Vec<StageMitigationPolicy> {
    vec![
        StageMitigationPolicy {
            stage: LatencyStage::PtyCapture,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::Defer,
            on_p999_overflow: Mitigation::Shed,
        },
        StageMitigationPolicy {
            stage: LatencyStage::DeltaExtraction,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::Degrade,
            on_p999_overflow: Mitigation::Degrade,
        },
        StageMitigationPolicy {
            stage: LatencyStage::StorageWrite,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::Defer,
            on_p999_overflow: Mitigation::Defer,
        },
        StageMitigationPolicy {
            stage: LatencyStage::PatternDetection,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::Degrade,
            on_p999_overflow: Mitigation::Skip,
        },
        StageMitigationPolicy {
            stage: LatencyStage::EventEmission,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::None,
            on_p999_overflow: Mitigation::Defer,
        },
        StageMitigationPolicy {
            stage: LatencyStage::WorkflowDispatch,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::Skip,
            on_p999_overflow: Mitigation::Skip,
        },
        StageMitigationPolicy {
            stage: LatencyStage::ActionExecution,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::Degrade,
            on_p999_overflow: Mitigation::Shed,
        },
        StageMitigationPolicy {
            stage: LatencyStage::ApiResponse,
            on_p95_overflow: Mitigation::None,
            on_p99_overflow: Mitigation::None,
            on_p999_overflow: Mitigation::Defer,
        },
    ]
}

/// A sliding window of latency observations for percentile estimation.
#[derive(Debug, Clone)]
pub(super) struct LatencyWindow {
    /// Ring buffer of observations in insertion order.
    samples: Vec<f64>,
    /// Current write position.
    pos: usize,
    /// Number of observations added (may exceed capacity).
    count: u64,
    /// Capacity (window_size).
    capacity: usize,
}

impl LatencyWindow {
    pub(super) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: Vec::with_capacity(capacity),
            pos: 0,
            count: 0,
            capacity,
        }
    }

    pub(super) fn push(&mut self, value: f64) {
        if self.samples.len() < self.capacity {
            self.samples.push(value);
        } else {
            self.samples[self.pos] = value;
        }
        self.pos = (self.pos + 1) % self.capacity;
        self.count += 1;
    }

    /// Estimate percentile from the window. Returns None if empty.
    pub(super) fn percentile(&self, p: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64 * p).ceil() as usize).min(sorted.len()) - 1;
        Some(sorted[idx])
    }

    pub(super) fn len(&self) -> usize {
        self.samples.len()
    }

    pub(super) fn total_count(&self) -> u64 {
        self.count
    }

    pub(super) fn mean(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        Some(self.samples.iter().sum::<f64>() / self.samples.len() as f64)
    }
}

/// Per-stage runtime state.
#[derive(Debug, Clone)]
struct StageState {
    budget: StageBudget,
    policy: StageMitigationPolicy,
    window: LatencyWindow,
    overflow_count: u64,
    last_overflow_reason: Option<ReasonCode>,
    last_mitigation: Mitigation,
}

/// Runtime result from recording a stage observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationResult {
    /// The stage that was measured.
    pub stage: LatencyStage,
    /// Observed latency in microseconds.
    pub latency_us: f64,
    /// Whether any percentile budget was exceeded.
    pub overflow: bool,
    /// The most severe violated percentile (if any).
    pub violated_percentile: Option<Percentile>,
    /// Reason code for the violation (if any).
    pub reason: Option<ReasonCode>,
    /// Mitigation recommended by the enforcer.
    pub recommended_mitigation: Mitigation,
    /// Current estimated percentiles for this stage.
    pub current_percentiles: PercentileSnapshot,
}

/// Point-in-time percentile estimates for a stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PercentileSnapshot {
    pub p50_us: Option<f64>,
    pub p95_us: Option<f64>,
    pub p99_us: Option<f64>,
    pub p999_us: Option<f64>,
    pub sample_count: usize,
    pub total_observations: u64,
}

/// Aggregate diagnostic snapshot of the enforcer state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnforcerSnapshot {
    /// Per-stage snapshots.
    pub stages: Vec<StageSnapshot>,
    /// Total observations across all stages.
    pub total_observations: u64,
    /// Total overflows across all stages.
    pub total_overflows: u64,
    /// Aggregate pipeline budget slack at each percentile.
    pub slack: Vec<(Percentile, f64)>,
}

/// Diagnostic snapshot for a single stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageSnapshot {
    pub stage: LatencyStage,
    pub budget: StageBudget,
    pub percentiles: PercentileSnapshot,
    pub overflow_count: u64,
    pub mean_us: Option<f64>,
    pub last_mitigation: Mitigation,
}

/// The budget enforcer tracks per-stage latency observations and
/// detects when budgets are exceeded, recommending mitigations.
///
/// # Determinism
///
/// The enforcer is deterministic for a given sequence of observations.
/// No randomness, no system time — caller provides all timing data.
///
/// # Thread Safety
///
/// This struct is NOT thread-safe. For multi-threaded use, wrap in
/// an appropriate synchronization primitive (Mutex, RwLock).
#[derive(Debug, Clone)]
pub struct BudgetEnforcer {
    config: BudgetEnforcerConfig,
    states: Vec<StageState>,
    pipeline_tree: BudgetNode,
    run_counter: u64,
    log_entries: Vec<LatencyLogEntry>,
}

impl BudgetEnforcer {
    /// Create a new budget enforcer with the given configuration.
    pub fn new(config: BudgetEnforcerConfig) -> Self {
        let pipeline_tree = config
            .pipeline_tree
            .clone()
            .unwrap_or_else(default_pipeline_tree);

        let states = config
            .stage_budgets
            .iter()
            .filter(|b| !b.stage.is_aggregate())
            .map(|budget| {
                let policy = config
                    .mitigation_policy
                    .iter()
                    .find(|p| p.stage == budget.stage)
                    .cloned()
                    .unwrap_or(StageMitigationPolicy {
                        stage: budget.stage,
                        on_p95_overflow: Mitigation::None,
                        on_p99_overflow: Mitigation::None,
                        on_p999_overflow: Mitigation::None,
                    });
                StageState {
                    budget: *budget,
                    policy,
                    window: LatencyWindow::new(config.window_size),
                    overflow_count: 0,
                    last_overflow_reason: None,
                    last_mitigation: Mitigation::None,
                }
            })
            .collect();

        Self {
            config,
            states,
            pipeline_tree,
            run_counter: 0,
            log_entries: Vec::new(),
        }
    }

    /// Create a new enforcer with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(BudgetEnforcerConfig::default())
    }

    /// Record a latency observation for a stage.
    ///
    /// Returns the observation result with overflow detection and
    /// mitigation recommendation.
    ///
    /// # Arguments
    /// - `stage`: which pipeline stage was measured.
    /// - `latency_us`: observed latency in microseconds.
    /// - `correlation_id`: ID linking this to a pipeline run.
    #[allow(clippy::similar_names)]
    pub fn record(
        &mut self,
        stage: LatencyStage,
        latency_us: f64,
        correlation_id: &str,
    ) -> ObservationResult {
        self.run_counter += 1;

        let state = match self.states.iter_mut().find(|s| s.budget.stage == stage) {
            Some(s) => s,
            None => {
                // Unknown stage — return benign result.
                return ObservationResult {
                    stage,
                    latency_us,
                    overflow: false,
                    violated_percentile: None,
                    reason: None,
                    recommended_mitigation: Mitigation::None,
                    current_percentiles: PercentileSnapshot {
                        p50_us: None,
                        p95_us: None,
                        p99_us: None,
                        p999_us: None,
                        sample_count: 0,
                        total_observations: 0,
                    },
                };
            }
        };

        state.window.push(latency_us);

        // Check budget at each percentile level (most severe first).
        let mut violated = None;
        let mut reason = None;
        let mut mitigation = Mitigation::None;

        // Check p999 first (most severe), then p99, p95, p50.
        for &pctl in &[
            Percentile::P999,
            Percentile::P99,
            Percentile::P95,
            Percentile::P50,
        ] {
            if state.budget.exceeds(pctl, latency_us) {
                violated = Some(pctl);
                reason = Some(state.budget.violation_reason(pctl));
                mitigation = match pctl {
                    Percentile::P999 => state.policy.on_p999_overflow,
                    Percentile::P99 => state.policy.on_p99_overflow,
                    Percentile::P95 => state.policy.on_p95_overflow,
                    Percentile::P50 => Mitigation::None,
                };
                break; // Most severe violation wins.
            }
        }

        let overflow = violated.is_some();
        if overflow {
            state.overflow_count += 1;
            state.last_overflow_reason.clone_from(&reason);
            state.last_mitigation = mitigation;
        }

        let percentiles = PercentileSnapshot {
            p50_us: state.window.percentile(0.5),
            p95_us: state.window.percentile(0.95),
            p99_us: state.window.percentile(0.99),
            p999_us: state.window.percentile(0.999),
            sample_count: state.window.len(),
            total_observations: state.window.total_count(),
        };

        // Emit structured log if configured.
        if self.config.log_all_observations || (self.config.log_overflows_only && overflow) {
            self.log_entries.push(LatencyLogEntry {
                timestamp: String::new(), // Caller provides real timestamp.
                subsystem: format!("latency.{}", stage.reason_prefix().to_lowercase()),
                correlation_id: correlation_id.to_string(),
                scenario_id: None,
                inputs: serde_json::json!({
                    "stage": stage.reason_prefix(),
                    "latency_us": latency_us,
                }),
                decision: if overflow {
                    format!("overflow_{}", mitigation)
                } else {
                    "within_budget".to_string()
                },
                outcome: serde_json::json!({
                    "overflow": overflow,
                    "violated_percentile": violated.map(|p| p.to_string()),
                    "mitigation": mitigation.to_string(),
                    "p50_us": percentiles.p50_us,
                    "p95_us": percentiles.p95_us,
                }),
                reason_code: reason.as_ref().map(|r| r.to_string()),
            });
        }

        ObservationResult {
            stage,
            latency_us,
            overflow,
            violated_percentile: violated,
            reason,
            recommended_mitigation: mitigation,
            current_percentiles: percentiles,
        }
    }

    /// Build a complete PipelineRun from accumulated observations.
    ///
    /// Caller provides per-stage observations in pipeline order.
    pub fn build_run(
        &self,
        run_id: &str,
        correlation_id: &str,
        observations: Vec<StageObservation>,
    ) -> PipelineRun {
        let total: f64 = observations.iter().map(|o| o.latency_us).sum();
        let has_overflow = observations.iter().any(|o| o.overflow);
        let reasons: Vec<ReasonCode> = observations
            .iter()
            .filter_map(|o| o.reason.clone())
            .collect();

        PipelineRun {
            run_id: run_id.to_string(),
            correlation_id: correlation_id.to_string(),
            scenario_id: None,
            stages: observations,
            total_latency_us: total,
            has_overflow,
            reasons,
        }
    }

    /// Get a diagnostic snapshot of the enforcer state.
    pub fn snapshot(&self) -> EnforcerSnapshot {
        let stages: Vec<StageSnapshot> = self
            .states
            .iter()
            .map(|s| StageSnapshot {
                stage: s.budget.stage,
                budget: s.budget,
                percentiles: PercentileSnapshot {
                    p50_us: s.window.percentile(0.5),
                    p95_us: s.window.percentile(0.95),
                    p99_us: s.window.percentile(0.99),
                    p999_us: s.window.percentile(0.999),
                    sample_count: s.window.len(),
                    total_observations: s.window.total_count(),
                },
                overflow_count: s.overflow_count,
                mean_us: s.window.mean(),
                last_mitigation: s.last_mitigation,
            })
            .collect();

        let total_observations: u64 = stages
            .iter()
            .map(|s| s.percentiles.total_observations)
            .sum();
        let total_overflows: u64 = stages.iter().map(|s| s.overflow_count).sum();

        // Compute slack at each percentile.
        let slack: Vec<(Percentile, f64)> = Percentile::ALL
            .iter()
            .map(|&p| {
                let agg = self.pipeline_tree.aggregate(p);
                let observed_sum: f64 = stages
                    .iter()
                    .filter_map(|s| match p {
                        Percentile::P50 => s.percentiles.p50_us,
                        Percentile::P95 => s.percentiles.p95_us,
                        Percentile::P99 => s.percentiles.p99_us,
                        Percentile::P999 => s.percentiles.p999_us,
                    })
                    .sum();
                (p, agg - observed_sum)
            })
            .collect();

        EnforcerSnapshot {
            stages,
            total_observations,
            total_overflows,
            slack,
        }
    }

    /// Get the accumulated log entries and clear the buffer.
    pub fn drain_logs(&mut self) -> Vec<LatencyLogEntry> {
        std::mem::take(&mut self.log_entries)
    }

    /// Get the number of accumulated log entries.
    pub fn log_count(&self) -> usize {
        self.log_entries.len()
    }

    /// Get the total number of observations across all stages.
    pub fn total_observations(&self) -> u64 {
        self.states.iter().map(|s| s.window.total_count()).sum()
    }

    /// Get the total number of overflow events across all stages.
    pub fn total_overflows(&self) -> u64 {
        self.states.iter().map(|s| s.overflow_count).sum()
    }

    /// Check if a specific stage has a budget registered.
    pub fn has_stage(&self, stage: LatencyStage) -> bool {
        self.states.iter().any(|s| s.budget.stage == stage)
    }

    /// Get the budget for a specific stage.
    pub fn stage_budget(&self, stage: LatencyStage) -> Option<&StageBudget> {
        self.states
            .iter()
            .find(|s| s.budget.stage == stage)
            .map(|s| &s.budget)
    }

    /// Get the mitigation recommendation for a stage at a given percentile.
    pub fn mitigation_for(&self, stage: LatencyStage, percentile: Percentile) -> Mitigation {
        self.states
            .iter()
            .find(|s| s.budget.stage == stage)
            .map(|s| match percentile {
                Percentile::P999 => s.policy.on_p999_overflow,
                Percentile::P99 => s.policy.on_p99_overflow,
                Percentile::P95 => s.policy.on_p95_overflow,
                Percentile::P50 => Mitigation::None,
            })
            .unwrap_or(Mitigation::None)
    }
}
