//! Pipeline observations and validation invariants for latency-stage runs.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{LatencyStage, Mitigation, Percentile, ReasonCode};

/// A single latency observation from one pipeline stage.
///
/// Used for budget accounting, logging, and post-hoc analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageObservation {
    /// Which stage was measured.
    pub stage: LatencyStage,
    /// Observed latency in microseconds.
    pub latency_us: f64,
    /// Correlation ID linking this observation to its pipeline run.
    pub correlation_id: String,
    /// Scenario ID for deterministic replay.
    pub scenario_id: Option<String>,
    /// Absolute timestamp (epoch microseconds) when the stage started.
    pub start_epoch_us: u64,
    /// Absolute timestamp (epoch microseconds) when the stage ended.
    pub end_epoch_us: u64,
    /// Whether the observation exceeded its budget at any percentile.
    pub overflow: bool,
    /// Reason code if overflow occurred.
    pub reason: Option<ReasonCode>,
    /// Mitigation applied (if any).
    pub mitigation: Mitigation,
}

/// A complete pipeline run with per-stage observations.
///
/// # Invariant
/// `stages` is ordered by pipeline position and timestamps are non-decreasing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineRun {
    /// Unique run identifier.
    pub run_id: String,
    /// Correlation ID shared across all stages in this run.
    pub correlation_id: String,
    /// Scenario ID for deterministic replay.
    pub scenario_id: Option<String>,
    /// Per-stage observations in pipeline order.
    pub stages: Vec<StageObservation>,
    /// Aggregate E2E latency in microseconds.
    pub total_latency_us: f64,
    /// Whether any stage overflowed.
    pub has_overflow: bool,
    /// All reason codes emitted during this run.
    pub reasons: Vec<ReasonCode>,
}

impl PipelineRun {
    /// Validate pipeline run invariants.
    ///
    /// # Invariants checked:
    /// 1. Stages are in pipeline order.
    /// 2. Timestamps are non-decreasing.
    /// 3. Total latency matches sum of stage latencies (within tolerance).
    /// 4. has_overflow matches any stage overflow.
    pub fn validate(&self) -> Result<(), Vec<InvariantViolation>> {
        let mut violations = Vec::new();

        // Check stage ordering.
        for window in self.stages.windows(2) {
            if window[0].stage >= window[1].stage && !window[0].stage.is_aggregate() {
                violations.push(InvariantViolation::StageOrdering {
                    expected: window[0].stage,
                    actual: window[1].stage,
                });
            }
        }

        // Check timestamp monotonicity.
        for window in self.stages.windows(2) {
            if window[0].end_epoch_us > window[1].start_epoch_us {
                violations.push(InvariantViolation::TimestampRegression {
                    stage: window[1].stage,
                    previous_end: window[0].end_epoch_us,
                    current_start: window[1].start_epoch_us,
                });
            }
        }

        // Check total latency consistency.
        let sum: f64 = self.stages.iter().map(|s| s.latency_us).sum();
        let tolerance = 100.0; // 100μs tolerance for measurement overhead
        if (self.total_latency_us - sum).abs() > tolerance {
            violations.push(InvariantViolation::TotalMismatch {
                declared: self.total_latency_us,
                computed: sum,
            });
        }

        // Check overflow flag consistency.
        let any_overflow = self.stages.iter().any(|s| s.overflow);
        if self.has_overflow != any_overflow {
            violations.push(InvariantViolation::OverflowFlagMismatch {
                declared: self.has_overflow,
                computed: any_overflow,
            });
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// Invariant violations detected during pipeline run validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InvariantViolation {
    /// Stages not in expected pipeline order.
    StageOrdering {
        expected: LatencyStage,
        actual: LatencyStage,
    },
    /// Timestamp regression between consecutive stages.
    TimestampRegression {
        stage: LatencyStage,
        previous_end: u64,
        current_start: u64,
    },
    /// Declared total doesn't match sum of stages.
    TotalMismatch { declared: f64, computed: f64 },
    /// Overflow flag doesn't match stage overflow states.
    OverflowFlagMismatch { declared: bool, computed: bool },
    /// Budget target is negative.
    NegativeBudget { stage: LatencyStage },
    /// Slack is negative (budget exceeded).
    NegativeSlack {
        percentile: Percentile,
        slack_us: f64,
    },
}

impl fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StageOrdering { expected, actual } => {
                write!(
                    f,
                    "Stage ordering violation: {expected} followed by {actual}"
                )
            }
            Self::TimestampRegression {
                stage,
                previous_end,
                current_start,
            } => write!(
                f,
                "Timestamp regression at {stage}: prev_end={previous_end} > start={current_start}"
            ),
            Self::TotalMismatch { declared, computed } => {
                write!(
                    f,
                    "Total latency mismatch: declared={declared:.1}μs, computed={computed:.1}μs"
                )
            }
            Self::OverflowFlagMismatch { declared, computed } => {
                write!(
                    f,
                    "Overflow flag mismatch: declared={declared}, computed={computed}"
                )
            }
            Self::NegativeBudget { stage } => {
                write!(f, "Negative budget for stage {stage}")
            }
            Self::NegativeSlack {
                percentile,
                slack_us,
            } => {
                write!(f, "Negative slack at {percentile}: {slack_us:.1}μs")
            }
        }
    }
}
