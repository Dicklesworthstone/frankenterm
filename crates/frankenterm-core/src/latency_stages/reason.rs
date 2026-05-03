//! Structured reason codes and mitigation identifiers for latency enforcement.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{LatencyStage, Percentile};

/// Structured reason codes for budget violations and mitigation events.
///
/// Every violation or mitigation in the latency pipeline produces a
/// reason code for structured logging, alerting, and post-hoc analysis.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReasonCode {
    /// Stage exceeded its budget at the given percentile.
    BudgetExceeded {
        stage: LatencyStage,
        percentile: Percentile,
    },
    /// Aggregate slack exhausted; no redistribution headroom.
    SlackExhausted,
    /// Stage overflow was isolated; downstream stages unaffected.
    OverflowIsolated { stage: LatencyStage },
    /// Cascade prevented by mitigation (skip, degrade, shed).
    CascadePrevented {
        stage: LatencyStage,
        mitigation: Mitigation,
    },
    /// Budget was redistributed from donor to recipient stage.
    SlackRedistributed {
        donor: LatencyStage,
        recipient: LatencyStage,
        amount_us: u64,
    },
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BudgetExceeded { stage, percentile } => {
                write!(f, "BUDGET_EXCEEDED_{stage}_{percentile}")
            }
            Self::SlackExhausted => f.write_str("SLACK_EXHAUSTED"),
            Self::OverflowIsolated { stage } => {
                write!(f, "OVERFLOW_ISOLATED_{stage}")
            }
            Self::CascadePrevented { stage, mitigation } => {
                write!(f, "CASCADE_PREVENTED_{stage}_{mitigation}")
            }
            Self::SlackRedistributed {
                donor, recipient, ..
            } => {
                write!(f, "SLACK_REDISTRIBUTED_{donor}_TO_{recipient}")
            }
        }
    }
}

/// Mitigation strategies when a stage overflows its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mitigation {
    /// Skip the stage entirely (for example, skip workflow for non-critical events).
    Skip,
    /// Degrade quality (for example, skip regex and use anchor-only detection).
    Degrade,
    /// Shed load (for example, drop low-priority pane captures).
    Shed,
    /// Defer to next cycle (for example, batch storage writes).
    Defer,
    /// No mitigation; propagate the latency.
    None,
}

impl fmt::Display for Mitigation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skip => f.write_str("SKIP"),
            Self::Degrade => f.write_str("DEGRADE"),
            Self::Shed => f.write_str("SHED"),
            Self::Defer => f.write_str("DEFER"),
            Self::None => f.write_str("NONE"),
        }
    }
}
