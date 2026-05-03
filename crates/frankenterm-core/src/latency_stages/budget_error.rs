//! Errors from latency budget construction and validation.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{LatencyStage, Percentile};

/// Errors from budget construction or validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BudgetError {
    /// A budget target was negative.
    NegativeTarget { stage: LatencyStage },
    /// Percentile targets are not monotonically non-decreasing.
    NonMonotonic {
        stage: LatencyStage,
        p50_us: f64,
        p95_us: f64,
        p99_us: f64,
        p999_us: f64,
    },
    /// Aggregate budget ceiling exceeded by leaf sum.
    CeilingExceeded {
        percentile: Percentile,
        ceiling_us: f64,
        actual_us: f64,
    },
    /// Unknown stage name in configuration.
    UnknownStage { name: String },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeTarget { stage } => {
                write!(f, "Negative latency target for stage {stage}")
            }
            Self::NonMonotonic { stage, .. } => {
                write!(f, "Non-monotonic percentile targets for stage {stage}")
            }
            Self::CeilingExceeded {
                percentile,
                ceiling_us,
                actual_us,
            } => write!(
                f,
                "Budget ceiling exceeded at {percentile}: ceiling={ceiling_us:.0}μs, actual={actual_us:.0}μs"
            ),
            Self::UnknownStage { name } => write!(f, "Unknown stage: {name}"),
        }
    }
}

impl std::error::Error for BudgetError {}
