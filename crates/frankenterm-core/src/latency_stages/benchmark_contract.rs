//! Benchmark workload classes and pass/fail criteria for latency budgets.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{LatencyStage, Percentile, default_budgets};

/// Workload class for benchmark scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkloadClass {
    /// Single pane, light output (< 100 bytes/sec).
    LightSingle,
    /// Single pane, heavy output (> 10KB/sec).
    HeavySingle,
    /// 10 panes, mixed output rates.
    MediumSwarm,
    /// 100 panes, sustained output.
    LargeSwarm,
    /// 100 panes, bursty output (10x normal for 1s intervals).
    BurstySwarm,
    /// 100 panes, pattern storm (many simultaneous detections).
    PatternStorm,
    /// Steady state with periodic GC/checkpoint pressure.
    GcPressure,
    /// Degraded storage (WAL checkpoint stall simulation).
    StorageDegraded,
}

impl WorkloadClass {
    /// All workload classes.
    pub const ALL: &[Self] = &[
        Self::LightSingle,
        Self::HeavySingle,
        Self::MediumSwarm,
        Self::LargeSwarm,
        Self::BurstySwarm,
        Self::PatternStorm,
        Self::GcPressure,
        Self::StorageDegraded,
    ];

    /// Whether this workload is adversarial (stress/chaos).
    pub fn is_adversarial(self) -> bool {
        matches!(
            self,
            Self::BurstySwarm | Self::PatternStorm | Self::GcPressure | Self::StorageDegraded
        )
    }

    /// Target percentile that this workload primarily stresses.
    pub fn primary_percentile(self) -> Percentile {
        match self {
            Self::LightSingle | Self::HeavySingle => Percentile::P50,
            Self::MediumSwarm | Self::LargeSwarm => Percentile::P95,
            Self::BurstySwarm | Self::PatternStorm => Percentile::P99,
            Self::GcPressure | Self::StorageDegraded => Percentile::P999,
        }
    }
}

impl fmt::Display for WorkloadClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LightSingle => f.write_str("light_single"),
            Self::HeavySingle => f.write_str("heavy_single"),
            Self::MediumSwarm => f.write_str("medium_swarm"),
            Self::LargeSwarm => f.write_str("large_swarm"),
            Self::BurstySwarm => f.write_str("bursty_swarm"),
            Self::PatternStorm => f.write_str("pattern_storm"),
            Self::GcPressure => f.write_str("gc_pressure"),
            Self::StorageDegraded => f.write_str("storage_degraded"),
        }
    }
}

/// A benchmark pass/fail criterion for a specific workload + stage + percentile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkCriterion {
    pub workload: WorkloadClass,
    pub stage: LatencyStage,
    pub percentile: Percentile,
    /// Maximum allowed latency in microseconds.
    pub max_us: f64,
    /// Maximum allowed overhead as fraction of baseline (e.g., 0.05 = 5%).
    pub max_overhead_fraction: f64,
}

/// The full benchmark contract: all criteria that must pass for the
/// latency budget to be considered satisfied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkContract {
    pub criteria: Vec<BenchmarkCriterion>,
}

impl BenchmarkContract {
    /// Generate the default benchmark contract from stage budgets and workload classes.
    ///
    /// For each (stage, workload, percentile) triple, the criterion is:
    /// - max_us = stage budget x workload multiplier
    /// - max_overhead_fraction = 5% for nominal, 10% for adversarial
    pub fn default_contract() -> Self {
        let budgets = default_budgets();
        let mut criteria = Vec::new();

        for budget in &budgets {
            if budget.stage.is_aggregate() {
                continue;
            }
            for &workload in WorkloadClass::ALL {
                let multiplier = match workload {
                    WorkloadClass::LightSingle => 0.8,
                    WorkloadClass::HeavySingle => 1.0,
                    WorkloadClass::MediumSwarm => 1.2,
                    WorkloadClass::LargeSwarm => 1.5,
                    WorkloadClass::BurstySwarm => 2.0,
                    WorkloadClass::PatternStorm => 2.5,
                    WorkloadClass::GcPressure => 3.0,
                    WorkloadClass::StorageDegraded => 5.0,
                };
                let overhead = if workload.is_adversarial() {
                    0.10
                } else {
                    0.05
                };

                for &percentile in Percentile::ALL {
                    criteria.push(BenchmarkCriterion {
                        workload,
                        stage: budget.stage,
                        percentile,
                        max_us: budget.target(percentile) * multiplier,
                        max_overhead_fraction: overhead,
                    });
                }
            }
        }

        Self { criteria }
    }
}
