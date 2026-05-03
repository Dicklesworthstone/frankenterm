//! Stage budgets and budget-composition algebra.

use super::{BudgetError, LatencyStage, Percentile, ReasonCode};
use serde::{Deserialize, Serialize};

/// Latency budget for a single stage, expressed as microsecond targets
/// at each percentile level.
///
/// # Invariants
/// - All targets are non-negative.
/// - Targets are monotonically non-decreasing: p50 <= p95 <= p99 <= p999.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StageBudget {
    pub stage: LatencyStage,
    /// p50 target in microseconds.
    pub p50_us: f64,
    /// p95 target in microseconds.
    pub p95_us: f64,
    /// p99 target in microseconds.
    pub p99_us: f64,
    /// p999 target in microseconds.
    pub p999_us: f64,
}

impl StageBudget {
    /// Create a new stage budget. Validates invariants.
    ///
    /// # Errors
    /// Returns `BudgetError::NegativeTarget` if any value < 0.
    /// Returns `BudgetError::NonMonotonic` if percentiles aren't ordered.
    #[allow(clippy::similar_names)]
    pub fn new(
        stage: LatencyStage,
        p50_us: f64,
        p95_us: f64,
        p99_us: f64,
        p999_us: f64,
    ) -> Result<Self, BudgetError> {
        if p50_us < 0.0 || p95_us < 0.0 || p99_us < 0.0 || p999_us < 0.0 {
            return Err(BudgetError::NegativeTarget { stage });
        }
        if !(p50_us <= p95_us && p95_us <= p99_us && p99_us <= p999_us) {
            return Err(BudgetError::NonMonotonic {
                stage,
                p50_us,
                p95_us,
                p99_us,
                p999_us,
            });
        }
        Ok(Self {
            stage,
            p50_us,
            p95_us,
            p99_us,
            p999_us,
        })
    }

    /// Get the target for a specific percentile.
    pub fn target(&self, percentile: Percentile) -> f64 {
        match percentile {
            Percentile::P50 => self.p50_us,
            Percentile::P95 => self.p95_us,
            Percentile::P99 => self.p99_us,
            Percentile::P999 => self.p999_us,
        }
    }

    /// Check whether an observed latency exceeds the budget at a given percentile.
    pub fn exceeds(&self, percentile: Percentile, observed_us: f64) -> bool {
        observed_us > self.target(percentile)
    }

    /// Generate the reason code for a budget violation.
    pub fn violation_reason(&self, percentile: Percentile) -> ReasonCode {
        ReasonCode::BudgetExceeded {
            stage: self.stage,
            percentile,
        }
    }
}

/// Default per-stage latency budgets (microseconds).
///
/// These are the initial targets derived from profiling the frankenterm
/// pipeline. They represent the contract that each stage must satisfy.
///
/// | Stage            | p50     | p95      | p99      | p999     |
/// |------------------|---------|----------|----------|----------|
/// | PtyCapture       | 5,000   | 10,000   | 20,000   | 50,000   |
/// | DeltaExtraction  | 200     | 500      | 1,000    | 5,000    |
/// | StorageWrite     | 1,000   | 5,000    | 10,000   | 30,000   |
/// | PatternDetection | 2,000   | 5,000    | 10,000   | 25,000   |
/// | EventEmission    | 500     | 2,000    | 5,000    | 15,000   |
/// | WorkflowDispatch | 1,000   | 3,000    | 8,000    | 20,000   |
/// | ActionExecution  | 10,000  | 50,000   | 100,000  | 500,000  |
/// | ApiResponse      | 500     | 2,000    | 5,000    | 15,000   |
/// | E2E Capture      | 10,000  | 25,000   | 50,000   | 150,000  |
/// | E2E Action       | 25,000  | 80,000   | 150,000  | 700,000  |
pub fn default_budgets() -> Vec<StageBudget> {
    vec![
        StageBudget {
            stage: LatencyStage::PtyCapture,
            p50_us: 5_000.0,
            p95_us: 10_000.0,
            p99_us: 20_000.0,
            p999_us: 50_000.0,
        },
        StageBudget {
            stage: LatencyStage::DeltaExtraction,
            p50_us: 200.0,
            p95_us: 500.0,
            p99_us: 1_000.0,
            p999_us: 5_000.0,
        },
        StageBudget {
            stage: LatencyStage::StorageWrite,
            p50_us: 1_000.0,
            p95_us: 5_000.0,
            p99_us: 10_000.0,
            p999_us: 30_000.0,
        },
        StageBudget {
            stage: LatencyStage::PatternDetection,
            p50_us: 2_000.0,
            p95_us: 5_000.0,
            p99_us: 10_000.0,
            p999_us: 25_000.0,
        },
        StageBudget {
            stage: LatencyStage::EventEmission,
            p50_us: 500.0,
            p95_us: 2_000.0,
            p99_us: 5_000.0,
            p999_us: 15_000.0,
        },
        StageBudget {
            stage: LatencyStage::WorkflowDispatch,
            p50_us: 1_000.0,
            p95_us: 3_000.0,
            p99_us: 8_000.0,
            p999_us: 20_000.0,
        },
        StageBudget {
            stage: LatencyStage::ActionExecution,
            p50_us: 10_000.0,
            p95_us: 50_000.0,
            p99_us: 100_000.0,
            p999_us: 500_000.0,
        },
        StageBudget {
            stage: LatencyStage::ApiResponse,
            p50_us: 500.0,
            p95_us: 2_000.0,
            p99_us: 5_000.0,
            p999_us: 15_000.0,
        },
        StageBudget {
            stage: LatencyStage::EndToEndCapture,
            p50_us: 10_000.0,
            p95_us: 25_000.0,
            p99_us: 50_000.0,
            p999_us: 150_000.0,
        },
        StageBudget {
            stage: LatencyStage::EndToEndAction,
            p50_us: 25_000.0,
            p95_us: 80_000.0,
            p99_us: 150_000.0,
            p999_us: 700_000.0,
        },
    ]
}

/// Composition mode for combining stage budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompositionMode {
    /// Sequential: budgets add.
    Sequential,
    /// Parallel: take max.
    Parallel,
    /// Conditional: weighted sum.
    Conditional,
}

/// A node in a budget composition tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BudgetNode {
    /// A leaf stage with its own budget.
    Leaf(StageBudget),
    /// Sequential composition of children.
    Seq(Vec<BudgetNode>),
    /// Parallel composition of children (take max).
    Par(Vec<BudgetNode>),
    /// Conditional branch with probability and optional else.
    Cond {
        probability: f64,
        then_branch: Box<BudgetNode>,
        else_branch: Option<Box<BudgetNode>>,
    },
}

impl BudgetNode {
    /// Compute the aggregate budget at a given percentile.
    ///
    /// # Invariants
    /// - Result is always non-negative.
    /// - Sequential: sum of children.
    /// - Parallel: max of children.
    /// - Conditional: weighted sum.
    pub fn aggregate(&self, percentile: Percentile) -> f64 {
        match self {
            Self::Leaf(budget) => budget.target(percentile),
            Self::Seq(children) => children.iter().map(|c| c.aggregate(percentile)).sum(),
            Self::Par(children) => children
                .iter()
                .map(|c| c.aggregate(percentile))
                .fold(0.0_f64, f64::max),
            Self::Cond {
                probability,
                then_branch,
                else_branch,
            } => {
                let then_val = then_branch.aggregate(percentile);
                let else_val = else_branch
                    .as_ref()
                    .map_or(0.0, |e| e.aggregate(percentile));
                (1.0 - probability).mul_add(else_val, probability * then_val)
            }
        }
    }

    /// Compute slack: aggregate ceiling minus sum of leaf budgets.
    ///
    /// Positive slack = headroom. Negative slack = budget violation.
    pub fn slack(&self, percentile: Percentile, ceiling_us: f64) -> f64 {
        ceiling_us - self.aggregate(percentile)
    }

    /// Collect all leaf stages from the tree.
    pub fn leaves(&self) -> Vec<&StageBudget> {
        match self {
            Self::Leaf(b) => vec![b],
            Self::Seq(children) | Self::Par(children) => {
                children.iter().flat_map(BudgetNode::leaves).collect()
            }
            Self::Cond {
                then_branch,
                else_branch,
                ..
            } => {
                let mut v = then_branch.leaves();
                if let Some(e) = else_branch {
                    v.extend(e.leaves());
                }
                v
            }
        }
    }
}

/// Build the default pipeline budget tree.
pub fn default_pipeline_tree() -> BudgetNode {
    let budgets = default_budgets();
    let find = |stage: LatencyStage| -> StageBudget {
        *budgets.iter().find(|b| b.stage == stage).unwrap()
    };

    BudgetNode::Seq(vec![
        BudgetNode::Leaf(find(LatencyStage::PtyCapture)),
        BudgetNode::Leaf(find(LatencyStage::DeltaExtraction)),
        BudgetNode::Leaf(find(LatencyStage::StorageWrite)),
        BudgetNode::Leaf(find(LatencyStage::PatternDetection)),
        BudgetNode::Leaf(find(LatencyStage::EventEmission)),
        BudgetNode::Cond {
            probability: 0.3,
            then_branch: Box::new(BudgetNode::Seq(vec![
                BudgetNode::Leaf(find(LatencyStage::WorkflowDispatch)),
                BudgetNode::Leaf(find(LatencyStage::ActionExecution)),
            ])),
            else_branch: None,
        },
        BudgetNode::Leaf(find(LatencyStage::ApiResponse)),
    ])
}
