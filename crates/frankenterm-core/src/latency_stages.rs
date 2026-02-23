//! Latency stage decomposition and budget algebra for the AARSP program.
//!
//! This module defines the formal stage decomposition of the input-to-visible-response
//! path, budget algebra for composing per-stage latency targets, and invariants
//! that the system must maintain under all conditions.
//!
//! # Stage Decomposition
//!
//! The critical path from PTY output to visible response traverses these stages:
//!
//! ```text
//! PTY → Capture → Delta → StorageWrite → PatternDetect → EventEmit
//!     → WorkflowDispatch → ActionExecute → ApiResponse
//! ```
//!
//! Each stage has independent p50/p95/p99/p999 budgets. The aggregate budget
//! is computed via composition rules that account for:
//! - Sequential composition (additive)
//! - Parallel fan-out (max of branches)
//! - Conditional paths (weighted by branch probability)
//!
//! # Budget Algebra
//!
//! Budget composition follows these rules:
//! - **Sequential**: B(A → B) = B(A) + B(B)
//! - **Parallel**: B(A ∥ B) = max(B(A), B(B))
//! - **Conditional**: B(A | p) = p·B(A) + (1-p)·B(skip)
//! - **Slack**: S = B(aggregate) - Σ B(stage_i) — must be ≥ 0
//!
//! # Invariants
//!
//! 1. **Monotonic sequencing**: Segment seq numbers are strictly increasing per pane.
//! 2. **Budget non-negative**: No stage budget can be negative.
//! 3. **Aggregate ceiling**: Sum of stage budgets ≤ aggregate budget at each percentile.
//! 4. **Slack conservation**: Redistributing slack preserves total budget.
//! 5. **Overflow isolation**: A stage exceeding its budget triggers overflow, not cascade.
//! 6. **Deterministic replay**: Same input + seed + config → same stage timings.
//!
//! # Reason Codes
//!
//! Every budget violation produces a structured reason code:
//! - `BUDGET_EXCEEDED_<STAGE>_<PERCENTILE>`: Stage exceeded its target at given percentile.
//! - `SLACK_EXHAUSTED`: Aggregate slack consumed, no redistribution possible.
//! - `OVERFLOW_ISOLATED`: Stage overflow contained, downstream unaffected.
//! - `CASCADE_PREVENTED`: Overflow mitigation activated (skip, degrade, shed).
//!
//! # AARSP Bead: ft-2p9cb.1.1.1

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Stage Definitions ──────────────────────────────────────────────

/// All stages on the critical path from PTY output to visible response.
///
/// Stages are ordered by their position in the pipeline. Each stage
/// represents a distinct latency-contributing operation with its own
/// budget, failure modes, and measurement points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum LatencyStage {
    /// PTY read → raw bytes available.
    /// Dominated by kernel scheduling and PTY buffer flush timing.
    PtyCapture,

    /// Raw snapshot → delta extraction via overlap matching.
    /// CPU-bound: string comparison against previous snapshot.
    DeltaExtraction,

    /// Delta → persisted segment in SQLite.
    /// I/O-bound: WAL write + FTS trigger indexing.
    StorageWrite,

    /// Persisted segment → pattern detection results.
    /// CPU-bound: Bloom filter → Aho-Corasick → regex extraction.
    PatternDetection,

    /// Detection → event record persisted + bus fanout.
    /// Mixed: SQLite INSERT + broadcast channel send.
    EventEmission,

    /// Event → workflow plan generated.
    /// CPU-bound: descriptor matching + plan construction.
    WorkflowDispatch,

    /// Workflow step → action executed (send-text, wait-for, etc.).
    /// Variable: depends on action type and external I/O.
    ActionExecution,

    /// Request received → JSON response serialized.
    /// Mixed: data fetch + serde serialization.
    ApiResponse,

    /// End-to-end: PTY output to detection event recorded.
    /// Aggregate of PtyCapture through EventEmission.
    EndToEndCapture,

    /// End-to-end: PTY output to workflow action complete.
    /// Aggregate of all stages.
    EndToEndAction,
}

impl LatencyStage {
    /// All stages in pipeline order (excluding aggregates).
    pub const PIPELINE_STAGES: &[Self] = &[
        Self::PtyCapture,
        Self::DeltaExtraction,
        Self::StorageWrite,
        Self::PatternDetection,
        Self::EventEmission,
        Self::WorkflowDispatch,
        Self::ActionExecution,
        Self::ApiResponse,
    ];

    /// Stages that compose the capture path (PTY → event recorded).
    pub const CAPTURE_PATH: &[Self] = &[
        Self::PtyCapture,
        Self::DeltaExtraction,
        Self::StorageWrite,
        Self::PatternDetection,
        Self::EventEmission,
    ];

    /// Stages that compose the action path (event → action complete).
    pub const ACTION_PATH: &[Self] = &[Self::WorkflowDispatch, Self::ActionExecution];

    /// Whether this stage is an aggregate (not a leaf stage).
    pub fn is_aggregate(self) -> bool {
        matches!(self, Self::EndToEndCapture | Self::EndToEndAction)
    }

    /// The short identifier for structured logging.
    pub fn reason_prefix(self) -> &'static str {
        match self {
            Self::PtyCapture => "PTY_CAPTURE",
            Self::DeltaExtraction => "DELTA_EXTRACT",
            Self::StorageWrite => "STORAGE_WRITE",
            Self::PatternDetection => "PATTERN_DETECT",
            Self::EventEmission => "EVENT_EMIT",
            Self::WorkflowDispatch => "WORKFLOW_DISPATCH",
            Self::ActionExecution => "ACTION_EXEC",
            Self::ApiResponse => "API_RESPONSE",
            Self::EndToEndCapture => "E2E_CAPTURE",
            Self::EndToEndAction => "E2E_ACTION",
        }
    }
}

impl fmt::Display for LatencyStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason_prefix())
    }
}

// ── Percentile Targets ─────────────────────────────────────────────

/// Percentile levels for latency budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Percentile {
    P50,
    P95,
    P99,
    P999,
}

impl Percentile {
    /// All percentile levels in ascending order.
    pub const ALL: &[Self] = &[Self::P50, Self::P95, Self::P99, Self::P999];

    /// The numeric percentile value (e.g., 0.999 for P999).
    pub fn value(self) -> f64 {
        match self {
            Self::P50 => 0.50,
            Self::P95 => 0.95,
            Self::P99 => 0.99,
            Self::P999 => 0.999,
        }
    }
}

impl fmt::Display for Percentile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::P50 => f.write_str("p50"),
            Self::P95 => f.write_str("p95"),
            Self::P99 => f.write_str("p99"),
            Self::P999 => f.write_str("p999"),
        }
    }
}

// ── Stage Budget ───────────────────────────────────────────────────

/// Latency budget for a single stage, expressed as microsecond targets
/// at each percentile level.
///
/// # Invariants
/// - All targets are non-negative.
/// - Targets are monotonically non-decreasing: p50 ≤ p95 ≤ p99 ≤ p999.
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

// ── Default Budget Table ───────────────────────────────────────────

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

// ── Budget Algebra ─────────────────────────────────────────────────

/// Composition mode for combining stage budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompositionMode {
    /// Sequential: budgets add. B(A → B) = B(A) + B(B).
    Sequential,
    /// Parallel: take max. B(A ∥ B) = max(B(A), B(B)).
    Parallel,
    /// Conditional: weighted sum. B(A | p) = p·B(A) + (1-p)·B(skip).
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
                probability * then_val + (1.0 - probability) * else_val
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
///
/// ```text
/// Seq [
///   PtyCapture,
///   DeltaExtraction,
///   StorageWrite,
///   PatternDetection,
///   EventEmission,
///   Cond(0.3) [         // ~30% of events trigger workflows
///     Seq [
///       WorkflowDispatch,
///       ActionExecution,
///     ]
///   ],
///   ApiResponse,
/// ]
/// ```
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

// ── Reason Codes ───────────────────────────────────────────────────

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
    /// Aggregate slack exhausted — no redistribution headroom.
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
    /// Skip the stage entirely (e.g., skip workflow for non-critical events).
    Skip,
    /// Degrade quality (e.g., skip regex, use anchor-only detection).
    Degrade,
    /// Shed load (e.g., drop low-priority pane captures).
    Shed,
    /// Defer to next cycle (e.g., batch storage writes).
    Defer,
    /// No mitigation — propagate the latency.
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

// ── Stage Measurement ──────────────────────────────────────────────

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

// ── Invariant Violations ───────────────────────────────────────────

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

// ── Error Types ────────────────────────────────────────────────────

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

// ── Structured Logging Contract ────────────────────────────────────

/// Required fields for every latency log entry.
///
/// This struct defines the structured logging contract for the AARSP
/// latency pipeline. Every log entry at critical decision points and
/// stage boundaries must include these fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyLogEntry {
    /// ISO-8601 timestamp with microsecond precision.
    pub timestamp: String,
    /// Subsystem identifier (e.g., "latency.pty_capture").
    pub subsystem: String,
    /// Correlation ID linking all stages of a single pipeline run.
    pub correlation_id: String,
    /// Scenario ID for deterministic replay (set in test/bench).
    pub scenario_id: Option<String>,
    /// Input description (pane_id, content_len, etc.).
    pub inputs: serde_json::Value,
    /// Decision made at this point (e.g., "delta_extracted", "bloom_rejected").
    pub decision: String,
    /// Outcome (latency_us, overflow, mitigation).
    pub outcome: serde_json::Value,
    /// Reason code or error code.
    pub reason_code: Option<String>,
}

// ── Benchmark Contract ─────────────────────────────────────────────

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
    /// - max_us = stage budget × workload multiplier
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

// ── Verification Matrix ────────────────────────────────────────────

/// Test scenario category for the verification matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TestCategory {
    /// Unit tests for individual functions.
    Unit,
    /// Property-based tests (proptest/quickcheck).
    Property,
    /// Integration tests across module boundaries.
    Integration,
    /// End-to-end pipeline tests.
    EndToEnd,
    /// Chaos/fault injection tests.
    Chaos,
    /// Sustained load (soak) tests.
    Soak,
}

/// A single entry in the verification matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationEntry {
    /// Test scenario name.
    pub name: String,
    /// Which category this test belongs to.
    pub category: TestCategory,
    /// Which stage(s) this test covers.
    pub stages: Vec<LatencyStage>,
    /// Conditions: nominal, degraded, failure, recovery, etc.
    pub conditions: Vec<String>,
    /// Expected invariants that must hold.
    pub invariants: Vec<String>,
    /// Minimum sample count for statistical significance.
    pub min_samples: u32,
}

/// The complete verification matrix for the latency stages module.
pub fn verification_matrix() -> Vec<VerificationEntry> {
    vec![
        // ── Unit tests ──
        VerificationEntry {
            name: "stage_budget_construction_valid".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec![
                "non-negative targets".into(),
                "monotonic percentiles".into(),
            ],
            min_samples: 1,
        },
        VerificationEntry {
            name: "stage_budget_rejects_negative".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["error".into()],
            invariants: vec!["NegativeTarget error returned".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "stage_budget_rejects_nonmonotonic".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["error".into()],
            invariants: vec!["NonMonotonic error returned".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "budget_tree_sequential_composition".into(),
            category: TestCategory::Unit,
            stages: LatencyStage::CAPTURE_PATH.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec!["aggregate equals sum of leaves".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "budget_tree_parallel_composition".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["aggregate equals max of branches".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "budget_tree_conditional_composition".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["aggregate equals weighted sum".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "slack_conservation".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["slack = ceiling - aggregate".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "reason_code_display".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["formatted reason matches expected pattern".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "pipeline_run_validation_happy".into(),
            category: TestCategory::Unit,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec![
                "stage order correct".into(),
                "timestamps non-decreasing".into(),
                "total matches sum".into(),
                "overflow flag consistent".into(),
            ],
            min_samples: 1,
        },
        VerificationEntry {
            name: "pipeline_run_validation_rejects_misordered".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["error".into()],
            invariants: vec!["StageOrdering violation".into()],
            min_samples: 1,
        },
        // ── Property tests ──
        VerificationEntry {
            name: "proptest_budget_monotonicity".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec![
                "p50 ≤ p95 ≤ p99 ≤ p999".into(),
                "all targets non-negative".into(),
            ],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_sequential_composition_additive".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["Seq aggregate = sum of leaf targets".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_parallel_composition_max".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["Par aggregate = max of branch targets".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_conditional_weighted".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["Cond aggregate = p*then + (1-p)*else".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_slack_conservation".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["slack = ceiling - aggregate (exact)".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_pipeline_run_roundtrip".into(),
            category: TestCategory::Property,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["random".into()],
            invariants: vec!["serde roundtrip preserves all fields".into()],
            min_samples: 1000,
        },
        // ── Integration tests ──
        VerificationEntry {
            name: "integration_default_budgets_consistency".into(),
            category: TestCategory::Integration,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec![
                "all stages have budgets".into(),
                "aggregate fits within E2E budget".into(),
            ],
            min_samples: 1,
        },
        VerificationEntry {
            name: "integration_benchmark_contract_coverage".into(),
            category: TestCategory::Integration,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec![
                "every non-aggregate stage has criteria".into(),
                "every workload class covered".into(),
            ],
            min_samples: 1,
        },
        // ── E2E tests ──
        VerificationEntry {
            name: "e2e_capture_path_within_budget".into(),
            category: TestCategory::EndToEnd,
            stages: LatencyStage::CAPTURE_PATH.to_vec(),
            conditions: vec!["light_single".into(), "medium_swarm".into()],
            invariants: vec!["total capture latency within E2E budget at p99".into()],
            min_samples: 100,
        },
        VerificationEntry {
            name: "e2e_action_path_within_budget".into(),
            category: TestCategory::EndToEnd,
            stages: LatencyStage::ACTION_PATH.to_vec(),
            conditions: vec!["light_single".into()],
            invariants: vec!["action completion within E2E budget at p99".into()],
            min_samples: 100,
        },
        // ── Chaos tests ──
        VerificationEntry {
            name: "chaos_storage_stall_overflow_isolated".into(),
            category: TestCategory::Chaos,
            stages: vec![LatencyStage::StorageWrite],
            conditions: vec!["storage_degraded".into()],
            invariants: vec![
                "overflow emitted for StorageWrite".into(),
                "downstream stages unaffected".into(),
                "reason code = OVERFLOW_ISOLATED".into(),
            ],
            min_samples: 10,
        },
        VerificationEntry {
            name: "chaos_pattern_storm_shed".into(),
            category: TestCategory::Chaos,
            stages: vec![LatencyStage::PatternDetection],
            conditions: vec!["pattern_storm".into()],
            invariants: vec![
                "detection latency bounded at p999".into(),
                "low-priority detections shed under pressure".into(),
            ],
            min_samples: 10,
        },
        // ── Soak tests ──
        VerificationEntry {
            name: "soak_24h_budget_drift".into(),
            category: TestCategory::Soak,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["large_swarm".into()],
            invariants: vec![
                "no percentile drift > 10% over 24h".into(),
                "no monotonic latency increase trend".into(),
            ],
            min_samples: 10000,
        },
    ]
}

// ── Runtime Budget Enforcer ─────────────────────────────────────────

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
struct LatencyWindow {
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
    fn new(capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
            pos: 0,
            count: 0,
            capacity,
        }
    }

    fn push(&mut self, value: f64) {
        if self.samples.len() < self.capacity {
            self.samples.push(value);
        } else {
            self.samples[self.pos] = value;
        }
        self.pos = (self.pos + 1) % self.capacity;
        self.count += 1;
    }

    /// Estimate percentile from the window. Returns None if empty.
    fn percentile(&self, p: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64 * p).ceil() as usize).min(sorted.len()) - 1;
        Some(sorted[idx])
    }

    fn len(&self) -> usize {
        self.samples.len()
    }

    fn total_count(&self) -> u64 {
        self.count
    }

    fn mean(&self) -> Option<f64> {
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
                    _ => Mitigation::None,
                };
                break; // Most severe violation wins.
            }
        }

        let overflow = violated.is_some();
        if overflow {
            state.overflow_count += 1;
            state.last_overflow_reason = reason.clone();
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
                    .filter_map(|s| {
                        let pctl_val = match p {
                            Percentile::P50 => s.percentiles.p50_us,
                            Percentile::P95 => s.percentiles.p95_us,
                            Percentile::P99 => s.percentiles.p99_us,
                            Percentile::P999 => s.percentiles.p999_us,
                        };
                        pctl_val
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

// ── Instrumentation Probes ─────────────────────────────────────────

/// A correlation context that propagates across async boundaries.
///
/// Created at the start of a pipeline run and threaded through all
/// stages. Each stage records its start/end timestamps and the
/// context carries accumulated timing data.
///
/// # AARSP Bead: ft-2p9cb.1.2
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrelationContext {
    /// Unique run identifier.
    pub run_id: String,
    /// Correlation ID (same as run_id unless explicitly set).
    pub correlation_id: String,
    /// Scenario ID for deterministic replay.
    pub scenario_id: Option<String>,
    /// Accumulated per-stage timing entries.
    pub timings: Vec<StageTiming>,
    /// The next expected stage in the pipeline.
    pub next_expected: Option<LatencyStage>,
    /// Whether the context was propagated correctly (no gaps).
    pub propagation_intact: bool,
    /// Creation timestamp (epoch microseconds, provided by caller).
    pub created_at_us: u64,
}

impl CorrelationContext {
    /// Create a new correlation context for a pipeline run.
    pub fn new(run_id: &str, created_at_us: u64) -> Self {
        Self {
            run_id: run_id.to_string(),
            correlation_id: run_id.to_string(),
            scenario_id: None,
            timings: Vec::with_capacity(LatencyStage::PIPELINE_STAGES.len()),
            next_expected: Some(LatencyStage::PIPELINE_STAGES[0]),
            propagation_intact: true,
            created_at_us,
        }
    }

    /// Create with an explicit correlation ID.
    pub fn with_correlation(run_id: &str, correlation_id: &str, created_at_us: u64) -> Self {
        Self {
            run_id: run_id.to_string(),
            correlation_id: correlation_id.to_string(),
            scenario_id: None,
            timings: Vec::with_capacity(LatencyStage::PIPELINE_STAGES.len()),
            next_expected: Some(LatencyStage::PIPELINE_STAGES[0]),
            propagation_intact: true,
            created_at_us,
        }
    }

    /// Record the start of a stage. Returns a StageProbe for timing.
    ///
    /// # Propagation Check
    /// If the stage doesn't match `next_expected`, a gap is recorded
    /// and `propagation_intact` is set to false.
    pub fn begin_stage(&mut self, stage: LatencyStage, start_us: u64) -> StageProbe {
        // Check propagation integrity.
        if let Some(expected) = self.next_expected {
            if stage != expected {
                self.propagation_intact = false;
            }
        }

        StageProbe {
            stage,
            start_us,
            correlation_id: self.correlation_id.clone(),
        }
    }

    /// Record the completion of a stage.
    ///
    /// Computes latency and updates the correlation chain.
    pub fn end_stage(&mut self, probe: StageProbe, end_us: u64) {
        let latency_us = if end_us >= probe.start_us {
            (end_us - probe.start_us) as f64
        } else {
            0.0 // Clock regression — treat as zero.
        };

        self.timings.push(StageTiming {
            stage: probe.stage,
            start_us: probe.start_us,
            end_us,
            latency_us,
        });

        // Advance expected stage.
        self.next_expected = Self::next_stage_after(probe.stage);
    }

    /// Convert to a PipelineRun for the BudgetEnforcer.
    pub fn to_pipeline_run(&self) -> PipelineRun {
        let observations: Vec<StageObservation> = self
            .timings
            .iter()
            .map(|t| StageObservation {
                stage: t.stage,
                latency_us: t.latency_us,
                correlation_id: self.correlation_id.clone(),
                scenario_id: self.scenario_id.clone(),
                start_epoch_us: t.start_us,
                end_epoch_us: t.end_us,
                overflow: false, // Will be filled by enforcer.
                reason: None,
                mitigation: Mitigation::None,
            })
            .collect();

        let total: f64 = observations.iter().map(|o| o.latency_us).sum();
        PipelineRun {
            run_id: self.run_id.clone(),
            correlation_id: self.correlation_id.clone(),
            scenario_id: self.scenario_id.clone(),
            stages: observations,
            total_latency_us: total,
            has_overflow: false,
            reasons: vec![],
        }
    }

    /// Get total elapsed time from first stage start to last stage end.
    pub fn total_elapsed_us(&self) -> u64 {
        if self.timings.is_empty() {
            return 0;
        }
        let first_start = self.timings.first().map(|t| t.start_us).unwrap_or(0);
        let last_end = self.timings.last().map(|t| t.end_us).unwrap_or(0);
        last_end.saturating_sub(first_start)
    }

    /// Get the number of stages recorded.
    pub fn stage_count(&self) -> usize {
        self.timings.len()
    }

    /// Check for missing stages in the pipeline.
    pub fn missing_stages(&self) -> Vec<LatencyStage> {
        let recorded: std::collections::HashSet<LatencyStage> =
            self.timings.iter().map(|t| t.stage).collect();
        LatencyStage::PIPELINE_STAGES
            .iter()
            .filter(|s| !recorded.contains(s))
            .copied()
            .collect()
    }

    fn next_stage_after(stage: LatencyStage) -> Option<LatencyStage> {
        let stages = LatencyStage::PIPELINE_STAGES;
        let pos = stages.iter().position(|&s| s == stage)?;
        stages.get(pos + 1).copied()
    }
}

/// A timing probe for a single stage.
///
/// Created by `CorrelationContext::begin_stage()`, consumed by `end_stage()`.
/// Carries the stage identity and start timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageProbe {
    /// Which stage is being timed.
    pub stage: LatencyStage,
    /// Start timestamp in epoch microseconds.
    pub start_us: u64,
    /// Correlation ID from the context.
    pub correlation_id: String,
}

/// Timing data for a completed stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: LatencyStage,
    pub start_us: u64,
    pub end_us: u64,
    pub latency_us: f64,
}

/// Overhead tracker for instrumentation itself.
///
/// Measures how much time the instrumentation probes add to the pipeline.
/// This is essential for proving the "bounded overhead" acceptance criterion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentationOverhead {
    /// Cumulative overhead from begin_stage/end_stage calls (microseconds).
    pub total_overhead_us: f64,
    /// Number of probe pairs measured.
    pub probe_count: u64,
    /// Mean overhead per probe pair.
    pub mean_overhead_us: f64,
    /// Maximum observed overhead.
    pub max_overhead_us: f64,
    /// Budget: maximum allowed overhead per probe pair (default 1μs).
    pub budget_per_probe_us: f64,
    /// Whether overhead is within budget.
    pub within_budget: bool,
}

impl InstrumentationOverhead {
    /// Create a new overhead tracker with default 1μs per-probe budget.
    pub fn new() -> Self {
        Self {
            total_overhead_us: 0.0,
            probe_count: 0,
            mean_overhead_us: 0.0,
            max_overhead_us: 0.0,
            budget_per_probe_us: 1.0,
            within_budget: true,
        }
    }

    /// Record a probe's overhead.
    pub fn record(&mut self, overhead_us: f64) {
        self.total_overhead_us += overhead_us;
        self.probe_count += 1;
        self.mean_overhead_us = self.total_overhead_us / self.probe_count as f64;
        if overhead_us > self.max_overhead_us {
            self.max_overhead_us = overhead_us;
        }
        self.within_budget = self.max_overhead_us <= self.budget_per_probe_us;
    }

    /// Get the overhead as a fraction of total pipeline time.
    pub fn overhead_fraction(&self, total_pipeline_us: f64) -> f64 {
        if total_pipeline_us <= 0.0 {
            return 0.0;
        }
        self.total_overhead_us / total_pipeline_us
    }
}

impl Default for InstrumentationOverhead {
    fn default() -> Self {
        Self::new()
    }
}

/// Extended enforcer that combines budget enforcement with correlation tracking.
///
/// Provides a high-level API for instrumenting pipeline runs end-to-end.
#[derive(Debug, Clone)]
pub struct InstrumentedEnforcer {
    enforcer: BudgetEnforcer,
    overhead: InstrumentationOverhead,
    completed_runs: u64,
    overflow_runs: u64,
}

impl InstrumentedEnforcer {
    /// Create with default configuration.
    pub fn new() -> Self {
        Self {
            enforcer: BudgetEnforcer::with_defaults(),
            overhead: InstrumentationOverhead::new(),
            completed_runs: 0,
            overflow_runs: 0,
        }
    }

    /// Create with custom configuration.
    pub fn with_config(config: BudgetEnforcerConfig) -> Self {
        Self {
            enforcer: BudgetEnforcer::new(config),
            overhead: InstrumentationOverhead::new(),
            completed_runs: 0,
            overflow_runs: 0,
        }
    }

    /// Process a completed correlation context through the enforcer.
    ///
    /// Records each stage timing and returns per-stage results.
    pub fn process_run(
        &mut self,
        ctx: &CorrelationContext,
    ) -> Vec<ObservationResult> {
        let mut results = Vec::with_capacity(ctx.timings.len());
        let mut any_overflow = false;

        for timing in &ctx.timings {
            let result = self.enforcer.record(
                timing.stage,
                timing.latency_us,
                &ctx.correlation_id,
            );
            if result.overflow {
                any_overflow = true;
            }
            results.push(result);
        }

        self.completed_runs += 1;
        if any_overflow {
            self.overflow_runs += 1;
        }

        results
    }

    /// Record instrumentation overhead for a probe pair.
    pub fn record_overhead(&mut self, overhead_us: f64) {
        self.overhead.record(overhead_us);
    }

    /// Get the underlying enforcer for snapshot/diagnostics.
    pub fn enforcer(&self) -> &BudgetEnforcer {
        &self.enforcer
    }

    /// Get the overhead tracker.
    pub fn overhead(&self) -> &InstrumentationOverhead {
        &self.overhead
    }

    /// Get statistics.
    pub fn completed_runs(&self) -> u64 {
        self.completed_runs
    }

    pub fn overflow_runs(&self) -> u64 {
        self.overflow_runs
    }

    /// Overflow rate as fraction of completed runs.
    pub fn overflow_rate(&self) -> f64 {
        if self.completed_runs == 0 {
            return 0.0;
        }
        self.overflow_runs as f64 / self.completed_runs as f64
    }
}

impl Default for InstrumentedEnforcer {
    fn default() -> Self {
        Self::new()
    }
}

// ── Guardrails ─────────────────────────────────────────────────────

/// Validation errors for instrumentation inputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InstrumentationError {
    /// Stage was started but never ended.
    UnterminatedProbe { stage: LatencyStage, start_us: u64 },
    /// Stage was ended without a matching begin.
    OrphanedEnd { stage: LatencyStage },
    /// Clock regression detected (end < start).
    ClockRegression {
        stage: LatencyStage,
        start_us: u64,
        end_us: u64,
    },
    /// Duplicate stage in a single run.
    DuplicateStage { stage: LatencyStage },
    /// Empty run (no stages recorded).
    EmptyRun { run_id: String },
    /// Overhead budget exceeded.
    OverheadBudgetExceeded {
        max_observed_us: f64,
        budget_us: f64,
    },
}

impl fmt::Display for InstrumentationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedProbe { stage, start_us } => {
                write!(f, "Unterminated probe for {stage} started at {start_us}μs")
            }
            Self::OrphanedEnd { stage } => {
                write!(f, "Orphaned end_stage for {stage} (no matching begin)")
            }
            Self::ClockRegression {
                stage,
                start_us,
                end_us,
            } => write!(
                f,
                "Clock regression at {stage}: start={start_us}μs > end={end_us}μs"
            ),
            Self::DuplicateStage { stage } => {
                write!(f, "Duplicate stage {stage} in single run")
            }
            Self::EmptyRun { run_id } => {
                write!(f, "Empty run {run_id} has no stages")
            }
            Self::OverheadBudgetExceeded {
                max_observed_us,
                budget_us,
            } => write!(
                f,
                "Overhead budget exceeded: observed={max_observed_us:.2}μs > budget={budget_us:.2}μs"
            ),
        }
    }
}

impl std::error::Error for InstrumentationError {}

/// Degradation level for instrumentation failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InstrumentationDegradation {
    /// Full instrumentation active.
    Full,
    /// Overhead tracking disabled to reduce cost.
    SkipOverhead,
    /// Correlation propagation disabled.
    SkipCorrelation,
    /// All instrumentation disabled — raw enforcer only.
    Passthrough,
}

impl fmt::Display for InstrumentationDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("FULL"),
            Self::SkipOverhead => f.write_str("SKIP_OVERHEAD"),
            Self::SkipCorrelation => f.write_str("SKIP_CORRELATION"),
            Self::Passthrough => f.write_str("PASSTHROUGH"),
        }
    }
}

// ── Validated Correlation Context ──────────────────────────────────

impl CorrelationContext {
    /// Validate the completed context for correctness.
    ///
    /// Returns a list of all detected issues. Empty list means valid.
    pub fn validate(&self) -> Vec<InstrumentationError> {
        let mut errors = Vec::new();

        if self.timings.is_empty() {
            errors.push(InstrumentationError::EmptyRun {
                run_id: self.run_id.clone(),
            });
            return errors;
        }

        // Check for duplicate stages.
        let mut seen = std::collections::HashSet::new();
        for timing in &self.timings {
            if !seen.insert(timing.stage) {
                errors.push(InstrumentationError::DuplicateStage {
                    stage: timing.stage,
                });
            }
        }

        // Check for clock regressions.
        for timing in &self.timings {
            if timing.end_us < timing.start_us {
                errors.push(InstrumentationError::ClockRegression {
                    stage: timing.stage,
                    start_us: timing.start_us,
                    end_us: timing.end_us,
                });
            }
        }

        // Check timestamp ordering between stages.
        for window in self.timings.windows(2) {
            if window[1].start_us < window[0].end_us {
                // Overlap detected — could indicate a gap in propagation
                // but not necessarily an error (parallel stages).
                // We only flag clock regression within a single stage.
            }
        }

        errors
    }

    /// Validate and return Ok(self) or Err(errors).
    pub fn validated(self) -> Result<Self, Vec<InstrumentationError>> {
        let errors = self.validate();
        if errors.is_empty() {
            Ok(self)
        } else {
            Err(errors)
        }
    }
}

// ── Diagnostic Dump ─────────────────────────────────────────────────

/// Full diagnostic snapshot of the instrumented pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentationDiagnostic {
    /// Current degradation level.
    pub degradation: InstrumentationDegradation,
    /// Enforcer snapshot (per-stage percentiles, slack, overflow counts).
    pub enforcer: EnforcerSnapshot,
    /// Overhead tracker state.
    pub overhead: InstrumentationOverhead,
    /// Total completed runs.
    pub completed_runs: u64,
    /// Total runs with at least one overflow.
    pub overflow_runs: u64,
    /// Overflow rate.
    pub overflow_rate: f64,
    /// Validation errors from the most recent run (if any).
    pub last_validation_errors: Vec<InstrumentationError>,
}

impl InstrumentedEnforcer {
    /// Get full diagnostic snapshot.
    pub fn diagnostic(&self) -> InstrumentationDiagnostic {
        InstrumentationDiagnostic {
            degradation: self.current_degradation(),
            enforcer: self.enforcer.snapshot(),
            overhead: self.overhead.clone(),
            completed_runs: self.completed_runs,
            overflow_runs: self.overflow_runs,
            overflow_rate: self.overflow_rate(),
            last_validation_errors: Vec::new(),
        }
    }

    /// Determine current degradation level based on overhead.
    pub fn current_degradation(&self) -> InstrumentationDegradation {
        if !self.overhead.within_budget {
            if self.overhead.max_overhead_us > self.overhead.budget_per_probe_us * 10.0 {
                InstrumentationDegradation::Passthrough
            } else if self.overhead.max_overhead_us > self.overhead.budget_per_probe_us * 5.0 {
                InstrumentationDegradation::SkipCorrelation
            } else {
                InstrumentationDegradation::SkipOverhead
            }
        } else {
            InstrumentationDegradation::Full
        }
    }

    /// Process a run with validation. Returns results and any validation errors.
    pub fn process_validated_run(
        &mut self,
        ctx: &CorrelationContext,
    ) -> (Vec<ObservationResult>, Vec<InstrumentationError>) {
        let validation_errors = ctx.validate();
        let results = self.process_run(ctx);
        (results, validation_errors)
    }

    /// Health check: returns true if instrumentation is healthy.
    ///
    /// Healthy means: overhead within budget, degradation is Full,
    /// and overflow rate is below 10%.
    pub fn is_healthy(&self) -> bool {
        self.overhead.within_budget
            && self.current_degradation() == InstrumentationDegradation::Full
            && self.overflow_rate() < 0.10
    }

    /// Get a compact status string for operator dashboards.
    pub fn status_line(&self) -> String {
        format!(
            "degradation={} runs={} overflows={} rate={:.1}% overhead_max={:.2}μs",
            self.current_degradation(),
            self.completed_runs,
            self.overflow_runs,
            self.overflow_rate() * 100.0,
            self.overhead.max_overhead_us,
        )
    }
}

// ── Fast Path Probe ─────────────────────────────────────────────────

/// Lightweight probe for the fast path — no allocation, no correlation.
///
/// For high-frequency stages where full correlation context is too expensive.
/// Simply records a start timestamp and stage identity. Use `elapsed_us()` to
/// compute latency without any heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastProbe {
    pub stage: LatencyStage,
    pub start_us: u64,
}

impl FastProbe {
    /// Create a fast probe (zero allocation).
    pub fn begin(stage: LatencyStage, start_us: u64) -> Self {
        Self { stage, start_us }
    }

    /// Compute elapsed time. Returns 0 on clock regression.
    pub fn elapsed_us(self, end_us: u64) -> f64 {
        if end_us >= self.start_us {
            (end_us - self.start_us) as f64
        } else {
            0.0
        }
    }
}

// ── Runtime Enforcement ─────────────────────────────────────────────

/// AARSP Bead: ft-2p9cb.1.3 — Runtime Budget Enforcement
///
/// This section implements the enforcement guards that sit on the critical path,
/// applying deterministic mitigation when budgets are exceeded.

/// Mitigation ladder with ordered escalation levels.
///
/// The ladder defines a strict partial order of increasingly aggressive
/// mitigation actions. The enforcer escalates monotonically (never
/// de-escalates within a single stage evaluation).
///
/// # Ladder ordering (least to most aggressive):
/// ```text
/// None(0) → Defer(1) → Degrade(2) → Shed(3) → Skip(4)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MitigationLevel {
    /// No mitigation needed.
    None = 0,
    /// Defer to next cycle.
    Defer = 1,
    /// Degrade quality.
    Degrade = 2,
    /// Shed load.
    Shed = 3,
    /// Skip entirely.
    Skip = 4,
}

impl MitigationLevel {
    /// Convert from Mitigation enum.
    pub fn from_mitigation(m: Mitigation) -> Self {
        match m {
            Mitigation::None => Self::None,
            Mitigation::Defer => Self::Defer,
            Mitigation::Degrade => Self::Degrade,
            Mitigation::Shed => Self::Shed,
            Mitigation::Skip => Self::Skip,
        }
    }

    /// Convert back to Mitigation enum.
    pub fn to_mitigation(self) -> Mitigation {
        match self {
            Self::None => Mitigation::None,
            Self::Defer => Mitigation::Defer,
            Self::Degrade => Mitigation::Degrade,
            Self::Shed => Mitigation::Shed,
            Self::Skip => Mitigation::Skip,
        }
    }

    /// All levels in escalation order.
    pub const ALL: &[Self] = &[Self::None, Self::Defer, Self::Degrade, Self::Shed, Self::Skip];

    /// Numeric severity (0-4).
    pub fn severity(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for MitigationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("NONE"),
            Self::Defer => f.write_str("DEFER"),
            Self::Degrade => f.write_str("DEGRADE"),
            Self::Shed => f.write_str("SHED"),
            Self::Skip => f.write_str("SKIP"),
        }
    }
}

/// Policy constraint that limits which mitigations can be applied to a stage.
///
/// # Safety Contract
/// Some stages are critical and must never be skipped. Others can tolerate
/// degradation but not shedding. PolicyConstraint makes these rules explicit
/// and machine-enforceable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConstraint {
    /// Stage this policy applies to.
    pub stage: LatencyStage,
    /// Maximum allowed mitigation level.
    pub max_level: MitigationLevel,
    /// Whether this stage is critical (violations generate alerts).
    pub critical: bool,
    /// Minimum observations before enforcement kicks in (warmup).
    pub warmup_count: u64,
}

impl PolicyConstraint {
    /// Check if a proposed mitigation level is allowed.
    pub fn allows(&self, level: MitigationLevel) -> bool {
        level <= self.max_level
    }

    /// Clamp a proposed level to the maximum allowed.
    pub fn clamp(&self, level: MitigationLevel) -> MitigationLevel {
        if level <= self.max_level {
            level
        } else {
            self.max_level
        }
    }
}

/// Default policy constraints for all pipeline stages.
pub fn default_policy_constraints() -> Vec<PolicyConstraint> {
    vec![
        PolicyConstraint {
            stage: LatencyStage::PtyCapture,
            max_level: MitigationLevel::Shed,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::DeltaExtraction,
            max_level: MitigationLevel::Degrade,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::StorageWrite,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::PatternDetection,
            max_level: MitigationLevel::Skip,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::EventEmission,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::WorkflowDispatch,
            max_level: MitigationLevel::Skip,
            critical: false,
            warmup_count: 5,
        },
        PolicyConstraint {
            stage: LatencyStage::ActionExecution,
            max_level: MitigationLevel::Shed,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::ApiResponse,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
    ]
}

/// Recovery protocol for stepping back from degraded to full quality.
///
/// After mitigation is applied, the system should recover once latency
/// returns to acceptable levels. RecoveryProtocol defines how quickly
/// and under what conditions recovery occurs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryProtocol {
    /// Number of consecutive within-budget observations before de-escalating.
    pub cooldown_observations: u64,
    /// Maximum time in degraded state before forced recovery attempt (μs).
    pub max_degraded_duration_us: u64,
    /// Whether to step down one level at a time or jump to full.
    pub gradual: bool,
}

impl Default for RecoveryProtocol {
    fn default() -> Self {
        Self {
            cooldown_observations: 20,
            max_degraded_duration_us: 30_000_000, // 30 seconds
            gradual: true,
        }
    }
}

/// Per-stage enforcement state tracking mitigation and recovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageEnforcementState {
    /// Current active mitigation level for this stage.
    pub current_level: MitigationLevel,
    /// Consecutive within-budget observations since last overflow.
    pub consecutive_ok: u64,
    /// Timestamp of last escalation (epoch μs, 0 if never escalated).
    pub last_escalation_us: u64,
    /// Total escalation count.
    pub escalation_count: u64,
    /// Total recovery count.
    pub recovery_count: u64,
}

impl StageEnforcementState {
    fn new() -> Self {
        Self {
            current_level: MitigationLevel::None,
            consecutive_ok: 0,
            last_escalation_us: 0,
            escalation_count: 0,
            recovery_count: 0,
        }
    }
}

/// Enforcement decision emitted for each stage observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnforcementDecision {
    /// Stage evaluated.
    pub stage: LatencyStage,
    /// Observed latency.
    pub latency_us: f64,
    /// Whether budget was exceeded.
    pub overflow: bool,
    /// Raw mitigation from the enforcer (before policy clamping).
    pub raw_mitigation: MitigationLevel,
    /// Clamped mitigation (after policy constraint).
    pub applied_mitigation: MitigationLevel,
    /// Whether this was a recovery (de-escalation).
    pub recovery: bool,
    /// Reason code.
    pub reason: Option<ReasonCode>,
    /// Whether warmup period is still active (enforcement suppressed).
    pub warmup_active: bool,
}

/// Configuration for the runtime enforcer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEnforcerConfig {
    /// Base enforcer configuration.
    pub enforcer_config: BudgetEnforcerConfig,
    /// Per-stage policy constraints.
    pub policy_constraints: Vec<PolicyConstraint>,
    /// Recovery protocol.
    pub recovery: RecoveryProtocol,
    /// Whether to emit structured decision logs.
    pub log_decisions: bool,
}

impl Default for RuntimeEnforcerConfig {
    fn default() -> Self {
        Self {
            enforcer_config: BudgetEnforcerConfig::default(),
            policy_constraints: default_policy_constraints(),
            recovery: RecoveryProtocol::default(),
            log_decisions: true,
        }
    }
}

/// The runtime budget enforcer with policy constraints and recovery.
///
/// Wraps BudgetEnforcer with:
/// - Policy-safe mitigation clamping
/// - Warmup suppression
/// - Recovery protocol (gradual de-escalation)
/// - Structured decision logging
///
/// # Determinism
/// All decisions are deterministic given the same sequence of observations.
/// No randomness, no system time — caller provides all timestamps.
#[derive(Debug, Clone)]
pub struct RuntimeEnforcer {
    enforcer: BudgetEnforcer,
    config: RuntimeEnforcerConfig,
    states: Vec<(LatencyStage, StageEnforcementState)>,
    decisions: Vec<EnforcementDecision>,
    observation_count: u64,
}

impl RuntimeEnforcer {
    /// Create a new runtime enforcer with the given configuration.
    pub fn new(config: RuntimeEnforcerConfig) -> Self {
        let enforcer = BudgetEnforcer::new(config.enforcer_config.clone());
        let states = LatencyStage::PIPELINE_STAGES
            .iter()
            .map(|&s| (s, StageEnforcementState::new()))
            .collect();
        Self {
            enforcer,
            config,
            states,
            decisions: Vec::new(),
            observation_count: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(RuntimeEnforcerConfig::default())
    }

    /// Record an observation and produce an enforcement decision.
    ///
    /// This is the main entry point for the critical path. It:
    /// 1. Records the observation in the base enforcer
    /// 2. Determines raw mitigation from overflow severity
    /// 3. Applies policy constraints (clamping)
    /// 4. Checks recovery conditions
    /// 5. Updates enforcement state
    /// 6. Emits a structured decision
    pub fn enforce(
        &mut self,
        stage: LatencyStage,
        latency_us: f64,
        correlation_id: &str,
        now_us: u64,
    ) -> EnforcementDecision {
        self.observation_count += 1;

        // Step 1: Record in base enforcer.
        let obs = self.enforcer.record(stage, latency_us, correlation_id);

        // Find enforcement state for this stage.
        let state = self
            .states
            .iter_mut()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st);

        let state = match state {
            Some(s) => s,
            None => {
                // Unknown stage — pass through.
                return EnforcementDecision {
                    stage,
                    latency_us,
                    overflow: false,
                    raw_mitigation: MitigationLevel::None,
                    applied_mitigation: MitigationLevel::None,
                    recovery: false,
                    reason: None,
                    warmup_active: true,
                };
            }
        };

        // Find policy constraint.
        let constraint = self
            .config
            .policy_constraints
            .iter()
            .find(|c| c.stage == stage);

        // Step 2: Check warmup.
        let warmup_active = constraint
            .map(|c| self.observation_count <= c.warmup_count)
            .unwrap_or(false);

        // Step 3: Determine raw mitigation level.
        let raw_level = MitigationLevel::from_mitigation(obs.recommended_mitigation);

        // Step 4: Apply policy constraint.
        let clamped_level = if warmup_active {
            MitigationLevel::None
        } else {
            constraint.map(|c| c.clamp(raw_level)).unwrap_or(raw_level)
        };

        // Step 5: Recovery check.
        let mut recovery = false;
        if obs.overflow {
            state.consecutive_ok = 0;
            if clamped_level > state.current_level {
                state.current_level = clamped_level;
                state.last_escalation_us = now_us;
                state.escalation_count += 1;
            }
        } else {
            state.consecutive_ok += 1;

            // Check recovery conditions.
            let cooldown_met =
                state.consecutive_ok >= self.config.recovery.cooldown_observations;
            let timeout_met = now_us.saturating_sub(state.last_escalation_us)
                >= self.config.recovery.max_degraded_duration_us;

            if state.current_level > MitigationLevel::None && (cooldown_met || timeout_met) {
                recovery = true;
                state.recovery_count += 1;
                if self.config.recovery.gradual && state.current_level > MitigationLevel::None {
                    // Step down one level.
                    let severity = state.current_level.severity();
                    state.current_level = if severity > 0 {
                        MitigationLevel::ALL[severity as usize - 1]
                    } else {
                        MitigationLevel::None
                    };
                } else {
                    state.current_level = MitigationLevel::None;
                }
                state.consecutive_ok = 0;
            }
        }

        let decision = EnforcementDecision {
            stage,
            latency_us,
            overflow: obs.overflow,
            raw_mitigation: raw_level,
            applied_mitigation: state.current_level,
            recovery,
            reason: obs.reason,
            warmup_active,
        };

        if self.config.log_decisions {
            self.decisions.push(decision.clone());
        }

        decision
    }

    /// Get the current mitigation level for a stage.
    pub fn current_level(&self, stage: LatencyStage) -> MitigationLevel {
        self.states
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st.current_level)
            .unwrap_or(MitigationLevel::None)
    }

    /// Get the enforcement state for a stage.
    pub fn stage_state(&self, stage: LatencyStage) -> Option<&StageEnforcementState> {
        self.states
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st)
    }

    /// Get the underlying enforcer.
    pub fn base_enforcer(&self) -> &BudgetEnforcer {
        &self.enforcer
    }

    /// Get accumulated decisions and clear.
    pub fn drain_decisions(&mut self) -> Vec<EnforcementDecision> {
        std::mem::take(&mut self.decisions)
    }

    /// Total observations processed.
    pub fn total_observations(&self) -> u64 {
        self.observation_count
    }

    /// Total escalations across all stages.
    pub fn total_escalations(&self) -> u64 {
        self.states.iter().map(|(_, s)| s.escalation_count).sum()
    }

    /// Total recoveries across all stages.
    pub fn total_recoveries(&self) -> u64 {
        self.states.iter().map(|(_, s)| s.recovery_count).sum()
    }

    /// Whether all stages are at MitigationLevel::None.
    pub fn is_fully_recovered(&self) -> bool {
        self.states
            .iter()
            .all(|(_, s)| s.current_level == MitigationLevel::None)
    }

    /// Compact status string.
    pub fn status_line(&self) -> String {
        let degraded: Vec<String> = self
            .states
            .iter()
            .filter(|(_, s)| s.current_level > MitigationLevel::None)
            .map(|(stage, s)| format!("{}={}", stage, s.current_level))
            .collect();
        if degraded.is_empty() {
            format!(
                "enforcement=NOMINAL obs={} esc={} rec={}",
                self.observation_count,
                self.total_escalations(),
                self.total_recoveries()
            )
        } else {
            format!(
                "enforcement=DEGRADED [{}] obs={} esc={} rec={}",
                degraded.join(", "),
                self.observation_count,
                self.total_escalations(),
                self.total_recoveries()
            )
        }
    }

    /// Process a complete CorrelationContext through the enforcer.
    ///
    /// Returns per-stage enforcement decisions.
    pub fn enforce_run(
        &mut self,
        ctx: &CorrelationContext,
        base_time_us: u64,
    ) -> Vec<EnforcementDecision> {
        let mut decisions = Vec::with_capacity(ctx.timings.len());
        for timing in &ctx.timings {
            let d = self.enforce(
                timing.stage,
                timing.latency_us,
                &ctx.correlation_id,
                base_time_us + timing.end_us,
            );
            decisions.push(d);
        }
        decisions
    }

    /// Get a full diagnostic snapshot.
    pub fn diagnostic_snapshot(&self) -> RuntimeEnforcerSnapshot {
        RuntimeEnforcerSnapshot {
            observation_count: self.observation_count,
            total_escalations: self.total_escalations(),
            total_recoveries: self.total_recoveries(),
            fully_recovered: self.is_fully_recovered(),
            stage_states: self
                .states
                .iter()
                .map(|(s, st)| (*s, st.clone()))
                .collect(),
            base_snapshot: self.enforcer.snapshot(),
        }
    }
}

/// Full diagnostic snapshot of the runtime enforcer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEnforcerSnapshot {
    pub observation_count: u64,
    pub total_escalations: u64,
    pub total_recoveries: u64,
    pub fully_recovered: bool,
    pub stage_states: Vec<(LatencyStage, StageEnforcementState)>,
    pub base_snapshot: EnforcerSnapshot,
}

// ── A4: Adaptive Budget Allocator ─────────────────────────────────
//
// Redistributes slack from under-budget stages to over-budget stages
// while preserving safety invariants:
// 1. Total budget conservation: sum of lane budgets = constant.
// 2. Bounded adaptation rate: per-epoch change ≤ max_adjustment_pct.
// 3. Minimum floor: no stage drops below min_budget_pct of its default.
// 4. Deterministic replay: same observations + config → same allocations.
//
// AARSP Bead: ft-2p9cb.1.4.1

/// Pressure signal for a single stage — how much headroom or deficit it has.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StagePressure {
    pub stage: LatencyStage,
    /// Observed p95 latency in microseconds (rolling window).
    pub observed_p95_us: f64,
    /// Current budget p95 target in microseconds.
    pub budget_p95_us: f64,
    /// Headroom fraction: (budget - observed) / budget. Negative means over-budget.
    pub headroom: f64,
}

impl StagePressure {
    /// Compute pressure from observation and budget.
    pub fn compute(stage: LatencyStage, observed_p95_us: f64, budget_p95_us: f64) -> Self {
        let headroom = if budget_p95_us > 0.0 {
            (budget_p95_us - observed_p95_us) / budget_p95_us
        } else {
            0.0
        };
        Self {
            stage,
            observed_p95_us,
            budget_p95_us,
            headroom,
        }
    }

    /// Is this stage under pressure (observed > budget)?
    pub fn is_over_budget(&self) -> bool {
        self.headroom < 0.0
    }

    /// How much slack (in us) this stage can donate.
    /// Returns 0.0 if under pressure.
    pub fn donatable_slack_us(&self) -> f64 {
        if self.headroom > 0.0 {
            self.budget_p95_us * self.headroom
        } else {
            0.0
        }
    }

    /// How much additional budget (in us) this stage needs.
    /// Returns 0.0 if within budget.
    pub fn deficit_us(&self) -> f64 {
        if self.headroom < 0.0 {
            self.observed_p95_us - self.budget_p95_us
        } else {
            0.0
        }
    }
}

/// Configuration for the adaptive budget allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptiveAllocatorConfig {
    /// Maximum fraction of a stage's budget that can be adjusted per epoch (0.0..1.0).
    /// E.g., 0.10 means ±10% per epoch.
    pub max_adjustment_pct: f64,
    /// Minimum fraction of the default budget that any stage can be reduced to.
    /// E.g., 0.50 means no stage goes below 50% of its default.
    pub min_budget_pct: f64,
    /// Maximum fraction above the default budget a stage can grow to.
    /// E.g., 2.0 means up to 200% of default.
    pub max_budget_pct: f64,
    /// Number of observations required before allocator starts adjusting.
    pub warmup_observations: u64,
    /// EWMA decay factor for pressure smoothing (0.0..1.0).
    /// Higher = more weight on recent observations.
    pub pressure_alpha: f64,
    /// Minimum headroom fraction to consider a stage as having donatable slack.
    /// Prevents robbing Peter to pay Paul when both are borderline.
    pub min_donor_headroom: f64,
}

impl Default for AdaptiveAllocatorConfig {
    fn default() -> Self {
        Self {
            max_adjustment_pct: 0.10,
            min_budget_pct: 0.50,
            max_budget_pct: 2.0,
            warmup_observations: 100,
            pressure_alpha: 0.3,
            min_donor_headroom: 0.15,
        }
    }
}

impl AdaptiveAllocatorConfig {
    /// Validate the configuration. Returns errors for invalid settings.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.max_adjustment_pct <= 0.0 || self.max_adjustment_pct > 1.0 {
            errors.push(format!(
                "max_adjustment_pct must be in (0.0, 1.0], got {}",
                self.max_adjustment_pct
            ));
        }
        if self.min_budget_pct <= 0.0 || self.min_budget_pct > 1.0 {
            errors.push(format!(
                "min_budget_pct must be in (0.0, 1.0], got {}",
                self.min_budget_pct
            ));
        }
        if self.max_budget_pct < 1.0 {
            errors.push(format!(
                "max_budget_pct must be >= 1.0, got {}",
                self.max_budget_pct
            ));
        }
        if self.pressure_alpha <= 0.0 || self.pressure_alpha > 1.0 {
            errors.push(format!(
                "pressure_alpha must be in (0.0, 1.0], got {}",
                self.pressure_alpha
            ));
        }
        if self.min_donor_headroom < 0.0 || self.min_donor_headroom >= 1.0 {
            errors.push(format!(
                "min_donor_headroom must be in [0.0, 1.0), got {}",
                self.min_donor_headroom
            ));
        }
        errors
    }
}

/// Per-stage allocation state tracked by the allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneAllocation {
    pub stage: LatencyStage,
    /// Default (baseline) budget at p95 in microseconds.
    pub default_p95_us: f64,
    /// Current allocated budget at p95 in microseconds.
    pub current_p95_us: f64,
    /// EWMA-smoothed headroom fraction.
    pub smoothed_headroom: f64,
    /// Cumulative slack donated (positive) or received (negative) in us.
    pub cumulative_transfer_us: f64,
    /// Number of epochs where this stage was over-budget.
    pub over_budget_epochs: u64,
    /// Number of epochs where this stage donated slack.
    pub donor_epochs: u64,
}

impl LaneAllocation {
    fn new(stage: LatencyStage, default_p95_us: f64) -> Self {
        Self {
            stage,
            default_p95_us,
            current_p95_us: default_p95_us,
            smoothed_headroom: 1.0,
            cumulative_transfer_us: 0.0,
            over_budget_epochs: 0,
            donor_epochs: 0,
        }
    }
}

/// A single reallocation decision made by the allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationDecision {
    /// The epoch (observation count) when this decision was made.
    pub epoch: u64,
    /// Correlation ID for replay determinism.
    pub correlation_id: String,
    /// Per-stage adjustments.
    pub adjustments: Vec<StageAdjustment>,
    /// Total slack pool before this allocation.
    pub slack_pool_before_us: f64,
    /// Total slack pool after this allocation.
    pub slack_pool_after_us: f64,
    /// Was the allocator in warmup (no-op)?
    pub warmup: bool,
    /// Reason for the allocation decision.
    pub reason: AllocationReason,
}

/// Individual stage adjustment within an allocation decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageAdjustment {
    pub stage: LatencyStage,
    /// Budget before adjustment.
    pub before_p95_us: f64,
    /// Budget after adjustment.
    pub after_p95_us: f64,
    /// Delta (positive = received slack, negative = donated).
    pub delta_us: f64,
    /// Was this adjustment clamped by rate limit?
    pub rate_clamped: bool,
    /// Was this adjustment clamped by floor/ceiling?
    pub bound_clamped: bool,
}

/// Reason code for an allocation decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllocationReason {
    /// System is in warmup, no adjustments made.
    Warmup,
    /// All stages within budget, no redistribution needed.
    AllWithinBudget,
    /// No donors available (all stages under pressure).
    NoDonors,
    /// Slack redistributed from donors to receivers.
    SlackRedistributed {
        donor_count: usize,
        receiver_count: usize,
    },
    /// Explicit reset to defaults requested.
    ResetToDefaults,
}

impl fmt::Display for AllocationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warmup => write!(f, "WARMUP"),
            Self::AllWithinBudget => write!(f, "ALL_WITHIN_BUDGET"),
            Self::NoDonors => write!(f, "NO_DONORS"),
            Self::SlackRedistributed {
                donor_count,
                receiver_count,
            } => write!(
                f,
                "SLACK_REDISTRIBUTED donors={} receivers={}",
                donor_count, receiver_count
            ),
            Self::ResetToDefaults => write!(f, "RESET_TO_DEFAULTS"),
        }
    }
}

/// Snapshot of the allocator state for diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocatorSnapshot {
    pub epoch: u64,
    pub total_budget_us: f64,
    pub allocated_budget_us: f64,
    pub global_slack_us: f64,
    pub lanes: Vec<LaneAllocation>,
    pub last_decision: Option<AllocationDecision>,
    pub config: AdaptiveAllocatorConfig,
}

/// The adaptive budget allocator.
///
/// Redistributes latency slack from consistently under-budget stages to
/// stages experiencing pressure, using EWMA-smoothed signals and bounded
/// per-epoch adjustment rates.
///
/// # Invariants
///
/// 1. **Conservation**: `Σ lane.current_p95_us` is constant across epochs.
/// 2. **Bounded rate**: Per-epoch change ≤ `max_adjustment_pct * default_budget`.
/// 3. **Floor/ceiling**: `min_budget_pct * default ≤ current ≤ max_budget_pct * default`.
/// 4. **Determinism**: Same observation sequence → same allocation history.
///
/// # Not thread-safe
///
/// Caller provides synchronization if shared across threads.
#[derive(Debug, Clone)]
pub struct AdaptiveAllocator {
    config: AdaptiveAllocatorConfig,
    lanes: Vec<LaneAllocation>,
    total_budget_us: f64,
    epoch: u64,
    decisions: Vec<AllocationDecision>,
    max_decisions: usize,
}

impl AdaptiveAllocator {
    /// Create a new allocator from stage budgets and configuration.
    pub fn new(stage_budgets: &[StageBudget], config: AdaptiveAllocatorConfig) -> Self {
        let lanes: Vec<LaneAllocation> = stage_budgets
            .iter()
            .filter(|b| !b.stage.is_aggregate())
            .map(|b| LaneAllocation::new(b.stage, b.p95_us))
            .collect();
        let total_budget_us: f64 = lanes.iter().map(|l| l.current_p95_us).sum();
        Self {
            config,
            lanes,
            total_budget_us,
            epoch: 0,
            decisions: Vec::new(),
            max_decisions: 1000,
        }
    }

    /// Create an allocator with default budgets and configuration.
    pub fn with_defaults() -> Self {
        Self::new(&default_budgets(), AdaptiveAllocatorConfig::default())
    }

    /// Current epoch count.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Total conserved budget.
    pub fn total_budget_us(&self) -> f64 {
        self.total_budget_us
    }

    /// Current global slack: total_budget - Σ current allocations.
    /// Should be ≈ 0.0 due to conservation, but floating point may drift.
    pub fn global_slack_us(&self) -> f64 {
        let allocated: f64 = self.lanes.iter().map(|l| l.current_p95_us).sum();
        self.total_budget_us - allocated
    }

    /// Get current allocation for a stage.
    pub fn allocation(&self, stage: LatencyStage) -> Option<&LaneAllocation> {
        self.lanes.iter().find(|l| l.stage == stage)
    }

    /// Get all lane allocations.
    pub fn lanes(&self) -> &[LaneAllocation] {
        &self.lanes
    }

    /// Get the last N allocation decisions (most recent first).
    pub fn recent_decisions(&self, n: usize) -> &[AllocationDecision] {
        let start = self.decisions.len().saturating_sub(n);
        &self.decisions[start..]
    }

    /// Process a set of pressure observations and produce an allocation decision.
    ///
    /// This is the core method. Call it once per epoch (e.g., every N observations
    /// or every T seconds).
    ///
    /// # Determinism
    ///
    /// Given the same sequence of `pressures` and `correlation_id`, the allocator
    /// produces identical decisions regardless of wall-clock time.
    pub fn allocate(
        &mut self,
        pressures: &[StagePressure],
        correlation_id: &str,
    ) -> AllocationDecision {
        self.epoch += 1;

        // Update EWMA headroom for each observed stage.
        for pressure in pressures {
            if let Some(lane) = self.lanes.iter_mut().find(|l| l.stage == pressure.stage) {
                lane.smoothed_headroom = lane.smoothed_headroom * (1.0 - self.config.pressure_alpha)
                    + pressure.headroom * self.config.pressure_alpha;
                if pressure.is_over_budget() {
                    lane.over_budget_epochs += 1;
                }
            }
        }

        // During warmup, return no-op decision.
        if self.epoch <= self.config.warmup_observations {
            let decision = AllocationDecision {
                epoch: self.epoch,
                correlation_id: correlation_id.to_string(),
                adjustments: Vec::new(),
                slack_pool_before_us: self.global_slack_us(),
                slack_pool_after_us: self.global_slack_us(),
                warmup: true,
                reason: AllocationReason::Warmup,
            };
            self.push_decision(decision.clone());
            return decision;
        }

        // Classify lanes into donors (excess headroom) and receivers (over-budget).
        let mut donors: Vec<usize> = Vec::new();
        let mut receivers: Vec<usize> = Vec::new();

        for (i, lane) in self.lanes.iter().enumerate() {
            if lane.smoothed_headroom >= self.config.min_donor_headroom {
                donors.push(i);
            } else if lane.smoothed_headroom < 0.0 {
                receivers.push(i);
            }
        }

        let slack_before = self.global_slack_us();

        // No receivers — all within budget.
        if receivers.is_empty() {
            let decision = AllocationDecision {
                epoch: self.epoch,
                correlation_id: correlation_id.to_string(),
                adjustments: Vec::new(),
                slack_pool_before_us: slack_before,
                slack_pool_after_us: slack_before,
                warmup: false,
                reason: AllocationReason::AllWithinBudget,
            };
            self.push_decision(decision.clone());
            return decision;
        }

        // No donors — can't help.
        if donors.is_empty() {
            let decision = AllocationDecision {
                epoch: self.epoch,
                correlation_id: correlation_id.to_string(),
                adjustments: Vec::new(),
                slack_pool_before_us: slack_before,
                slack_pool_after_us: slack_before,
                warmup: false,
                reason: AllocationReason::NoDonors,
            };
            self.push_decision(decision.clone());
            return decision;
        }

        // Compute available slack from donors and total deficit from receivers.
        let mut available_slack = 0.0_f64;
        for &idx in &donors {
            let lane = &self.lanes[idx];
            let max_donate = lane.default_p95_us * self.config.max_adjustment_pct;
            let floor = lane.default_p95_us * self.config.min_budget_pct;
            let actual_donate = max_donate.min(lane.current_p95_us - floor).max(0.0);
            available_slack += actual_donate;
        }

        let mut total_deficit = 0.0_f64;
        for &idx in &receivers {
            let lane = &self.lanes[idx];
            // Deficit = how much more this lane needs.
            let deficit = (-lane.smoothed_headroom) * lane.current_p95_us;
            let max_receive = lane.default_p95_us * self.config.max_adjustment_pct;
            let ceiling = lane.default_p95_us * self.config.max_budget_pct;
            let room = ceiling - lane.current_p95_us;
            total_deficit += deficit.min(max_receive).min(room).max(0.0);
        }

        // Scale: if deficit > available, proportionally reduce.
        let scale = if total_deficit > 0.0 {
            (available_slack / total_deficit).min(1.0)
        } else {
            0.0
        };

        let mut adjustments = Vec::new();

        // Donate from donors.
        let mut donated_total = 0.0_f64;
        for &idx in &donors {
            let lane = &mut self.lanes[idx];
            let max_donate = lane.default_p95_us * self.config.max_adjustment_pct;
            let floor = lane.default_p95_us * self.config.min_budget_pct;
            let actual_donate = max_donate.min(lane.current_p95_us - floor).max(0.0);
            // Scale donation proportionally to how much is needed.
            let donate = if available_slack > 0.0 {
                actual_donate * (total_deficit * scale / available_slack).min(1.0)
            } else {
                0.0
            };
            if donate > 0.0 {
                let before = lane.current_p95_us;
                lane.current_p95_us -= donate;
                let rate_clamped = donate >= max_donate;
                let bound_clamped = lane.current_p95_us <= floor;
                if bound_clamped {
                    lane.current_p95_us = floor;
                }
                let actual_delta = before - lane.current_p95_us;
                lane.cumulative_transfer_us -= actual_delta;
                lane.donor_epochs += 1;
                donated_total += actual_delta;
                adjustments.push(StageAdjustment {
                    stage: lane.stage,
                    before_p95_us: before,
                    after_p95_us: lane.current_p95_us,
                    delta_us: -actual_delta,
                    rate_clamped,
                    bound_clamped,
                });
            }
        }

        // Distribute to receivers proportionally to deficit.
        let mut remaining = donated_total;
        for &idx in &receivers {
            let lane = &mut self.lanes[idx];
            let deficit = (-lane.smoothed_headroom) * lane.current_p95_us;
            let max_receive = lane.default_p95_us * self.config.max_adjustment_pct;
            let ceiling = lane.default_p95_us * self.config.max_budget_pct;
            let room = ceiling - lane.current_p95_us;
            let want = deficit.min(max_receive).min(room).max(0.0);

            let give = if total_deficit > 0.0 {
                (want / total_deficit * donated_total).min(remaining)
            } else {
                0.0
            };

            if give > 0.0 {
                let before = lane.current_p95_us;
                lane.current_p95_us += give;
                let rate_clamped = give >= max_receive;
                let bound_clamped = lane.current_p95_us >= ceiling;
                if bound_clamped {
                    lane.current_p95_us = ceiling;
                }
                let actual_give = lane.current_p95_us - before;
                lane.cumulative_transfer_us += actual_give;
                remaining -= actual_give;
                adjustments.push(StageAdjustment {
                    stage: lane.stage,
                    before_p95_us: before,
                    after_p95_us: lane.current_p95_us,
                    delta_us: actual_give,
                    rate_clamped,
                    bound_clamped,
                });
            }
        }

        let decision = AllocationDecision {
            epoch: self.epoch,
            correlation_id: correlation_id.to_string(),
            adjustments,
            slack_pool_before_us: slack_before,
            slack_pool_after_us: self.global_slack_us(),
            warmup: false,
            reason: AllocationReason::SlackRedistributed {
                donor_count: donors.len(),
                receiver_count: receivers.len(),
            },
        };
        self.push_decision(decision.clone());
        decision
    }

    /// Reset all lane allocations to their defaults.
    pub fn reset(&mut self) -> AllocationDecision {
        self.epoch += 1;
        let mut adjustments = Vec::new();
        for lane in &mut self.lanes {
            if (lane.current_p95_us - lane.default_p95_us).abs() > 1e-6 {
                adjustments.push(StageAdjustment {
                    stage: lane.stage,
                    before_p95_us: lane.current_p95_us,
                    after_p95_us: lane.default_p95_us,
                    delta_us: lane.default_p95_us - lane.current_p95_us,
                    rate_clamped: false,
                    bound_clamped: false,
                });
                lane.current_p95_us = lane.default_p95_us;
                lane.smoothed_headroom = 1.0;
                lane.cumulative_transfer_us = 0.0;
            }
        }
        let decision = AllocationDecision {
            epoch: self.epoch,
            correlation_id: String::new(),
            adjustments,
            slack_pool_before_us: self.global_slack_us(),
            slack_pool_after_us: 0.0,
            warmup: false,
            reason: AllocationReason::ResetToDefaults,
        };
        self.push_decision(decision.clone());
        decision
    }

    /// Get a diagnostic snapshot.
    pub fn snapshot(&self) -> AllocatorSnapshot {
        AllocatorSnapshot {
            epoch: self.epoch,
            total_budget_us: self.total_budget_us,
            allocated_budget_us: self.lanes.iter().map(|l| l.current_p95_us).sum(),
            global_slack_us: self.global_slack_us(),
            lanes: self.lanes.clone(),
            last_decision: self.decisions.last().cloned(),
            config: self.config.clone(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let over_budget: Vec<String> = self
            .lanes
            .iter()
            .filter(|l| l.smoothed_headroom < 0.0)
            .map(|l| format!("{}", l.stage))
            .collect();
        if over_budget.is_empty() {
            format!(
                "allocator=NOMINAL epoch={} slack={:.1}us",
                self.epoch,
                self.global_slack_us()
            )
        } else {
            format!(
                "allocator=REDISTRIBUTING epoch={} pressure=[{}] slack={:.1}us",
                self.epoch,
                over_budget.join(", "),
                self.global_slack_us()
            )
        }
    }

    fn push_decision(&mut self, decision: AllocationDecision) {
        self.decisions.push(decision);
        // Bounded history.
        if self.decisions.len() > self.max_decisions {
            self.decisions.drain(0..self.decisions.len() / 2);
        }
    }

    /// Extract pressure signals from an EnforcerSnapshot.
    ///
    /// This bridges the BudgetEnforcer output into the allocator input,
    /// using observed p95 from the enforcer's percentile estimates.
    pub fn pressures_from_snapshot(snapshot: &EnforcerSnapshot) -> Vec<StagePressure> {
        snapshot
            .stages
            .iter()
            .filter(|ss| !ss.stage.is_aggregate())
            .map(|ss| {
                let observed_p95 = ss.percentiles.p95_us.unwrap_or(0.0);
                StagePressure::compute(ss.stage, observed_p95, ss.budget.p95_us)
            })
            .collect()
    }

    /// Generate updated StageBudgets reflecting current allocations.
    ///
    /// Returns budgets with p95 adjusted to the allocator's current values.
    /// Other percentiles (p50, p99, p999) are scaled proportionally so
    /// the monotonic invariant is preserved.
    pub fn adjusted_budgets(&self) -> Vec<StageBudget> {
        self.lanes
            .iter()
            .map(|lane| {
                let ratio = if lane.default_p95_us > 0.0 {
                    lane.current_p95_us / lane.default_p95_us
                } else {
                    1.0
                };
                // Find the original budget from defaults.
                let defaults = default_budgets();
                let orig = defaults
                    .iter()
                    .find(|b| b.stage == lane.stage)
                    .cloned()
                    .unwrap_or(StageBudget {
                        stage: lane.stage,
                        p50_us: lane.default_p95_us * 0.5,
                        p95_us: lane.default_p95_us,
                        p99_us: lane.default_p95_us * 2.0,
                        p999_us: lane.default_p95_us * 5.0,
                    });
                StageBudget {
                    stage: lane.stage,
                    p50_us: orig.p50_us * ratio,
                    p95_us: lane.current_p95_us,
                    p99_us: orig.p99_us * ratio,
                    p999_us: orig.p999_us * ratio,
                }
            })
            .collect()
    }

    /// Check allocator health — detects potential instability.
    pub fn current_degradation(&self) -> AllocatorDegradation {
        // Check for oscillation: if many lanes flip between donor/receiver rapidly.
        let oscillating = self
            .lanes
            .iter()
            .filter(|l| l.donor_epochs > 0 && l.over_budget_epochs > 0)
            .count();

        if oscillating > self.lanes.len() / 2 {
            return AllocatorDegradation::Oscillating { lane_count: oscillating };
        }

        // Check conservation drift.
        let drift = self.global_slack_us().abs();
        if drift > 1.0 {
            return AllocatorDegradation::ConservationDrift { drift_us: drift };
        }

        // Check if too many lanes are at their floor.
        let at_floor = self
            .lanes
            .iter()
            .filter(|l| {
                (l.current_p95_us - l.default_p95_us * self.config.min_budget_pct).abs() < 1e-6
            })
            .count();

        if at_floor > self.lanes.len() / 2 {
            return AllocatorDegradation::FloorSaturation { lane_count: at_floor };
        }

        AllocatorDegradation::Healthy
    }

    /// Is the allocator in a healthy state?
    pub fn is_healthy(&self) -> bool {
        matches!(self.current_degradation(), AllocatorDegradation::Healthy)
    }

    /// Generate a structured log entry for the most recent allocation decision.
    pub fn last_log_entry(&self) -> Option<AllocationLogEntry> {
        self.decisions.last().map(|d| AllocationLogEntry {
            epoch: d.epoch,
            correlation_id: d.correlation_id.clone(),
            reason: d.reason.to_string(),
            adjustment_count: d.adjustments.len(),
            total_donated_us: d
                .adjustments
                .iter()
                .filter(|a| a.delta_us < 0.0)
                .map(|a| -a.delta_us)
                .sum(),
            total_received_us: d
                .adjustments
                .iter()
                .filter(|a| a.delta_us > 0.0)
                .map(|a| a.delta_us)
                .sum(),
            conservation_error_us: self.global_slack_us(),
            degradation: self.current_degradation(),
        })
    }
}

/// Degradation states for the adaptive allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AllocatorDegradation {
    /// All invariants hold, allocator operating normally.
    Healthy,
    /// Multiple lanes oscillating between donor and receiver roles.
    Oscillating { lane_count: usize },
    /// Budget conservation invariant has drifted beyond tolerance.
    ConservationDrift { drift_us: f64 },
    /// Too many lanes pinned at their minimum floor.
    FloorSaturation { lane_count: usize },
}

impl fmt::Display for AllocatorDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::Oscillating { lane_count } => {
                write!(f, "OSCILLATING lanes={}", lane_count)
            }
            Self::ConservationDrift { drift_us } => {
                write!(f, "CONSERVATION_DRIFT drift={:.3}us", drift_us)
            }
            Self::FloorSaturation { lane_count } => {
                write!(f, "FLOOR_SATURATION lanes={}", lane_count)
            }
        }
    }
}

/// Structured log entry for an allocation epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationLogEntry {
    pub epoch: u64,
    pub correlation_id: String,
    pub reason: String,
    pub adjustment_count: usize,
    pub total_donated_us: f64,
    pub total_received_us: f64,
    pub conservation_error_us: f64,
    pub degradation: AllocatorDegradation,
}

// ── B1: Three-Lane Scheduler Architecture ─────────────────────────
//
// Defines three scheduling lanes for the pipeline:
// - Input: User keystrokes, terminal I/O — highest priority, bounded queue.
// - Control: System signals, health checks — medium priority.
// - Bulk: Background tasks, batch indexing — lowest priority, elastic.
//
// Admission policy ensures input lane immunity during bulk pressure.
// AARSP Bead: ft-2p9cb.2.1.1

/// Scheduling lane classification.
///
/// Tasks are assigned to lanes based on their latency-sensitivity.
/// The scheduler services lanes in strict priority order: Input > Control > Bulk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SchedulerLane {
    /// User-facing I/O: keystrokes, display updates, PTY reads.
    /// Latency target: < 5ms p99. Never starved.
    Input = 0,
    /// System control: health checks, pane lifecycle, config reloads.
    /// Latency target: < 50ms p99. May be deferred under extreme input pressure.
    Control = 1,
    /// Background work: batch indexing, pattern scanning, log rotation.
    /// Latency target: best-effort. Throttled to protect input/control lanes.
    Bulk = 2,
}

impl SchedulerLane {
    /// All lanes in priority order (highest first).
    pub const ALL: &'static [Self] = &[Self::Input, Self::Control, Self::Bulk];

    /// Priority value (lower = higher priority).
    pub fn priority(self) -> u8 {
        self as u8
    }

    /// Which pipeline stages belong to this lane by default.
    pub fn default_stages(self) -> &'static [LatencyStage] {
        match self {
            Self::Input => &[
                LatencyStage::PtyCapture,
                LatencyStage::DeltaExtraction,
                LatencyStage::ApiResponse,
            ],
            Self::Control => &[
                LatencyStage::EventEmission,
                LatencyStage::WorkflowDispatch,
                LatencyStage::ActionExecution,
            ],
            Self::Bulk => &[
                LatencyStage::StorageWrite,
                LatencyStage::PatternDetection,
            ],
        }
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Control => "control",
            Self::Bulk => "bulk",
        }
    }
}

impl fmt::Display for SchedulerLane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Map a pipeline stage to its scheduling lane.
pub fn stage_to_lane(stage: LatencyStage) -> SchedulerLane {
    match stage {
        LatencyStage::PtyCapture
        | LatencyStage::DeltaExtraction
        | LatencyStage::ApiResponse => SchedulerLane::Input,
        LatencyStage::EventEmission
        | LatencyStage::WorkflowDispatch
        | LatencyStage::ActionExecution => SchedulerLane::Control,
        LatencyStage::StorageWrite
        | LatencyStage::PatternDetection => SchedulerLane::Bulk,
        // Aggregates don't schedule directly.
        LatencyStage::EndToEndCapture | LatencyStage::EndToEndAction => SchedulerLane::Bulk,
    }
}

/// A schedulable work item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Unique item ID.
    pub id: u64,
    /// Which lane this item belongs to.
    pub lane: SchedulerLane,
    /// Which pipeline stage this work is for.
    pub stage: LatencyStage,
    /// Estimated cost in microseconds.
    pub estimated_cost_us: f64,
    /// Correlation ID for tracing.
    pub correlation_id: String,
    /// Deadline in microseconds from epoch (0 = no deadline).
    pub deadline_us: u64,
}

/// Admission decision for a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmissionDecision {
    /// Item admitted to its lane queue.
    Admitted,
    /// Item deferred: bulk lane full, will retry.
    Deferred,
    /// Item shed: queue overflow, item dropped.
    Shed,
    /// Item promoted: moved to higher-priority lane due to deadline pressure.
    Promoted { from: SchedulerLane, to: SchedulerLane },
}

impl fmt::Display for AdmissionDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admitted => write!(f, "ADMITTED"),
            Self::Deferred => write!(f, "DEFERRED"),
            Self::Shed => write!(f, "SHED"),
            Self::Promoted { from, to } => write!(f, "PROMOTED {}→{}", from, to),
        }
    }
}

/// Configuration for the three-lane scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneSchedulerConfig {
    /// Maximum queue depth per lane.
    pub input_queue_capacity: usize,
    pub control_queue_capacity: usize,
    pub bulk_queue_capacity: usize,
    /// Maximum fraction of CPU time each lane can consume per scheduling epoch.
    /// Must sum to ≤ 1.0.
    pub input_cpu_share: f64,
    pub control_cpu_share: f64,
    pub bulk_cpu_share: f64,
    /// If input queue depth exceeds this fraction, shed bulk items.
    pub input_pressure_threshold: f64,
    /// Enable deadline-based promotion from bulk → control.
    pub enable_deadline_promotion: bool,
    /// Deadline promotion threshold: if remaining time < this fraction of deadline, promote.
    pub deadline_promotion_fraction: f64,
}

impl Default for LaneSchedulerConfig {
    fn default() -> Self {
        Self {
            input_queue_capacity: 256,
            control_queue_capacity: 128,
            bulk_queue_capacity: 1024,
            input_cpu_share: 0.50,
            control_cpu_share: 0.30,
            bulk_cpu_share: 0.20,
            input_pressure_threshold: 0.75,
            enable_deadline_promotion: true,
            deadline_promotion_fraction: 0.25,
        }
    }
}

impl LaneSchedulerConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let total_share = self.input_cpu_share + self.control_cpu_share + self.bulk_cpu_share;
        if total_share > 1.0 + 1e-6 {
            errors.push(format!(
                "CPU shares sum to {} (must be ≤ 1.0)",
                total_share
            ));
        }
        if self.input_cpu_share < 0.0 || self.control_cpu_share < 0.0 || self.bulk_cpu_share < 0.0
        {
            errors.push("CPU shares must be non-negative".into());
        }
        if self.input_pressure_threshold <= 0.0 || self.input_pressure_threshold > 1.0 {
            errors.push(format!(
                "input_pressure_threshold must be in (0.0, 1.0], got {}",
                self.input_pressure_threshold
            ));
        }
        if self.deadline_promotion_fraction <= 0.0 || self.deadline_promotion_fraction >= 1.0 {
            errors.push(format!(
                "deadline_promotion_fraction must be in (0.0, 1.0), got {}",
                self.deadline_promotion_fraction
            ));
        }
        errors
    }

    /// Get queue capacity for a lane.
    pub fn capacity(&self, lane: SchedulerLane) -> usize {
        match lane {
            SchedulerLane::Input => self.input_queue_capacity,
            SchedulerLane::Control => self.control_queue_capacity,
            SchedulerLane::Bulk => self.bulk_queue_capacity,
        }
    }

    /// Get CPU share for a lane.
    pub fn cpu_share(&self, lane: SchedulerLane) -> f64 {
        match lane {
            SchedulerLane::Input => self.input_cpu_share,
            SchedulerLane::Control => self.control_cpu_share,
            SchedulerLane::Bulk => self.bulk_cpu_share,
        }
    }
}

/// Per-lane queue state tracked by the scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneState {
    pub lane: SchedulerLane,
    pub depth: usize,
    pub capacity: usize,
    pub total_admitted: u64,
    pub total_deferred: u64,
    pub total_shed: u64,
    pub total_completed: u64,
    pub cpu_used_us: f64,
    pub cpu_budget_us: f64,
}

impl LaneState {
    fn new(lane: SchedulerLane, capacity: usize) -> Self {
        Self {
            lane,
            depth: 0,
            capacity,
            total_admitted: 0,
            total_deferred: 0,
            total_shed: 0,
            total_completed: 0,
            cpu_used_us: 0.0,
            cpu_budget_us: 0.0,
        }
    }

    /// Queue utilization fraction (0.0 to 1.0).
    pub fn utilization(&self) -> f64 {
        if self.capacity > 0 {
            self.depth as f64 / self.capacity as f64
        } else {
            0.0
        }
    }

    /// Is the queue at or above capacity?
    pub fn is_full(&self) -> bool {
        self.depth >= self.capacity
    }
}

/// Scheduling event for structured logging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulingEvent {
    pub item_id: u64,
    pub lane: SchedulerLane,
    pub stage: LatencyStage,
    pub decision: AdmissionDecision,
    pub queue_depth_before: usize,
    pub queue_depth_after: usize,
    pub correlation_id: String,
    pub reason_code: Option<String>,
}

/// Diagnostic snapshot of the three-lane scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerSnapshot {
    pub epoch: u64,
    pub lanes: Vec<LaneState>,
    pub total_items_processed: u64,
    pub input_pressure: bool,
    pub config: LaneSchedulerConfig,
}

/// The three-lane scheduler.
///
/// Manages admission, ordering, and completion tracking for work items
/// across three priority lanes: Input, Control, Bulk.
///
/// # Invariants
///
/// 1. **Input immunity**: Input lane items are never shed while input queue < capacity.
/// 2. **Strict ordering**: Input > Control > Bulk in scheduling priority.
/// 3. **Bounded queues**: Each lane has a fixed capacity; overflow triggers shed/defer.
/// 4. **Determinism**: Same item sequence → same scheduling decisions.
#[derive(Debug, Clone)]
pub struct LaneScheduler {
    config: LaneSchedulerConfig,
    lanes: Vec<LaneState>,
    epoch: u64,
    next_item_id: u64,
    events: Vec<SchedulingEvent>,
    max_events: usize,
}

impl LaneScheduler {
    /// Create a new scheduler with the given configuration.
    pub fn new(config: LaneSchedulerConfig) -> Self {
        let lanes = vec![
            LaneState::new(SchedulerLane::Input, config.input_queue_capacity),
            LaneState::new(SchedulerLane::Control, config.control_queue_capacity),
            LaneState::new(SchedulerLane::Bulk, config.bulk_queue_capacity),
        ];
        Self {
            config,
            lanes,
            epoch: 0,
            next_item_id: 1,
            events: Vec::new(),
            max_events: 1000,
        }
    }

    /// Create a scheduler with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(LaneSchedulerConfig::default())
    }

    /// Admit a work item to the appropriate lane.
    ///
    /// Returns the admission decision and assigns an item ID.
    pub fn admit(
        &mut self,
        stage: LatencyStage,
        estimated_cost_us: f64,
        correlation_id: &str,
        deadline_us: u64,
        now_us: u64,
    ) -> (WorkItem, AdmissionDecision) {
        let lane = stage_to_lane(stage);
        let item_id = self.next_item_id;
        self.next_item_id += 1;

        let item = WorkItem {
            id: item_id,
            lane,
            stage,
            estimated_cost_us,
            correlation_id: correlation_id.to_string(),
            deadline_us,
        };

        let decision = self.apply_admission(&item, now_us);

        let lane_state = &self.lanes[lane as usize];
        self.push_event(SchedulingEvent {
            item_id,
            lane,
            stage,
            decision: decision.clone(),
            queue_depth_before: if matches!(decision, AdmissionDecision::Admitted) {
                lane_state.depth.saturating_sub(1)
            } else {
                lane_state.depth
            },
            queue_depth_after: lane_state.depth,
            correlation_id: correlation_id.to_string(),
            reason_code: match &decision {
                AdmissionDecision::Deferred => Some("BULK_QUEUE_FULL".into()),
                AdmissionDecision::Shed => Some("QUEUE_OVERFLOW".into()),
                AdmissionDecision::Promoted { .. } => Some("DEADLINE_PROMOTION".into()),
                _ => None,
            },
        });

        (item, decision)
    }

    /// Mark an item as completed.
    pub fn complete(&mut self, lane: SchedulerLane, actual_cost_us: f64) {
        let state = &mut self.lanes[lane as usize];
        if state.depth > 0 {
            state.depth -= 1;
            state.total_completed += 1;
            state.cpu_used_us += actual_cost_us;
        }
    }

    /// Start a new scheduling epoch. Resets per-epoch CPU counters.
    pub fn begin_epoch(&mut self, epoch_budget_us: f64) {
        self.epoch += 1;
        for state in &mut self.lanes {
            state.cpu_used_us = 0.0;
            state.cpu_budget_us = epoch_budget_us * self.config.cpu_share(state.lane);
        }
    }

    /// Is the input lane under pressure?
    pub fn input_under_pressure(&self) -> bool {
        let input = &self.lanes[SchedulerLane::Input as usize];
        input.utilization() >= self.config.input_pressure_threshold
    }

    /// Get the lane state for a specific lane.
    pub fn lane_state(&self, lane: SchedulerLane) -> &LaneState {
        &self.lanes[lane as usize]
    }

    /// Get a diagnostic snapshot.
    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            epoch: self.epoch,
            lanes: self.lanes.clone(),
            total_items_processed: self.lanes.iter().map(|l| l.total_completed).sum(),
            input_pressure: self.input_under_pressure(),
            config: self.config.clone(),
        }
    }

    /// Get the last N scheduling events.
    pub fn recent_events(&self, n: usize) -> &[SchedulingEvent] {
        let start = self.events.len().saturating_sub(n);
        &self.events[start..]
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let depths: Vec<String> = self
            .lanes
            .iter()
            .map(|l| format!("{}={}/{}", l.lane, l.depth, l.capacity))
            .collect();
        format!(
            "scheduler epoch={} [{}] pressure={}",
            self.epoch,
            depths.join(" "),
            self.input_under_pressure()
        )
    }

    fn apply_admission(&mut self, item: &WorkItem, now_us: u64) -> AdmissionDecision {
        let lane_idx = item.lane as usize;

        // Check if input lane is under pressure — shed bulk items.
        if item.lane == SchedulerLane::Bulk && self.input_under_pressure() {
            self.lanes[lane_idx].total_shed += 1;
            return AdmissionDecision::Shed;
        }

        // Check for deadline-based promotion.
        if self.config.enable_deadline_promotion
            && item.lane == SchedulerLane::Bulk
            && item.deadline_us > 0
            && now_us > 0
        {
            let remaining = item.deadline_us.saturating_sub(now_us);
            let threshold =
                (item.deadline_us as f64 * self.config.deadline_promotion_fraction) as u64;
            if remaining < threshold {
                // Promote to control lane.
                let control_idx = SchedulerLane::Control as usize;
                if !self.lanes[control_idx].is_full() {
                    self.lanes[control_idx].depth += 1;
                    self.lanes[control_idx].total_admitted += 1;
                    return AdmissionDecision::Promoted {
                        from: SchedulerLane::Bulk,
                        to: SchedulerLane::Control,
                    };
                }
            }
        }

        // Try to admit to the item's lane.
        let state = &mut self.lanes[lane_idx];
        if state.is_full() {
            // Input items are never shed — they wait (defer).
            // Control items defer. Bulk items are shed.
            match item.lane {
                SchedulerLane::Input | SchedulerLane::Control => {
                    state.total_deferred += 1;
                    AdmissionDecision::Deferred
                }
                SchedulerLane::Bulk => {
                    state.total_shed += 1;
                    AdmissionDecision::Shed
                }
            }
        } else {
            state.depth += 1;
            state.total_admitted += 1;
            AdmissionDecision::Admitted
        }
    }

    fn push_event(&mut self, event: SchedulingEvent) {
        self.events.push(event);
        if self.events.len() > self.max_events {
            self.events.drain(0..self.events.len() / 2);
        }
    }

    /// Check whether a lane has remaining CPU budget in the current epoch.
    pub fn has_cpu_budget(&self, lane: SchedulerLane) -> bool {
        let state = &self.lanes[lane as usize];
        state.cpu_used_us < state.cpu_budget_us
    }

    /// Remaining CPU budget for a lane in the current epoch.
    pub fn remaining_cpu_us(&self, lane: SchedulerLane) -> f64 {
        let state = &self.lanes[lane as usize];
        (state.cpu_budget_us - state.cpu_used_us).max(0.0)
    }

    /// Pick the next lane to service using strict priority.
    ///
    /// Returns the highest-priority lane that has items and CPU budget.
    /// Falls through to lower priority lanes only when higher lanes are empty.
    pub fn next_lane(&self) -> Option<SchedulerLane> {
        for &lane in SchedulerLane::ALL {
            let state = &self.lanes[lane as usize];
            if state.depth > 0 && state.cpu_used_us < state.cpu_budget_us {
                return Some(lane);
            }
        }
        // Fallback: any lane with items (ignore budget for input lane).
        if self.lanes[SchedulerLane::Input as usize].depth > 0 {
            return Some(SchedulerLane::Input);
        }
        None
    }

    /// Compute fairness metric: ratio of actual CPU share to configured share per lane.
    ///
    /// Returns (lane, fairness_ratio) for each lane.
    /// Fairness ratio = 1.0 means exactly fair; < 1.0 means under-served; > 1.0 means over-served.
    pub fn fairness_ratios(&self) -> Vec<(SchedulerLane, f64)> {
        let total_cpu: f64 = self.lanes.iter().map(|l| l.cpu_used_us).sum();
        if total_cpu < 1e-6 {
            return SchedulerLane::ALL.iter().map(|&l| (l, 1.0)).collect();
        }
        SchedulerLane::ALL
            .iter()
            .map(|&lane| {
                let state = &self.lanes[lane as usize];
                let actual_share = state.cpu_used_us / total_cpu;
                let target_share = self.config.cpu_share(lane);
                let ratio = if target_share > 0.0 {
                    actual_share / target_share
                } else {
                    0.0
                };
                (lane, ratio)
            })
            .collect()
    }

    /// Detect scheduler degradation.
    pub fn current_degradation(&self) -> SchedulerDegradation {
        // Check for starvation: any lane with items but 0 completions over many epochs.
        let input = &self.lanes[SchedulerLane::Input as usize];
        let control = &self.lanes[SchedulerLane::Control as usize];
        let bulk = &self.lanes[SchedulerLane::Bulk as usize];

        // Input starvation is critical.
        if input.depth > 0 && input.total_deferred > input.total_admitted / 2 + 1 {
            return SchedulerDegradation::InputStarvation {
                depth: input.depth,
                deferred: input.total_deferred,
            };
        }

        // Bulk starvation: many items shed, few completed.
        if bulk.total_shed > bulk.total_completed + 10 {
            return SchedulerDegradation::BulkStarvation {
                shed_count: bulk.total_shed,
                completed_count: bulk.total_completed,
            };
        }

        // Control backlog: queue growing without drain.
        if control.depth > control.capacity / 2 {
            return SchedulerDegradation::ControlBacklog {
                depth: control.depth,
                capacity: control.capacity,
            };
        }

        SchedulerDegradation::Healthy
    }

    /// Is the scheduler healthy?
    pub fn is_healthy(&self) -> bool {
        matches!(self.current_degradation(), SchedulerDegradation::Healthy)
    }

    /// Generate a structured log entry for the current epoch state.
    pub fn log_entry(&self) -> SchedulerLogEntry {
        SchedulerLogEntry {
            epoch: self.epoch,
            depths: SchedulerLane::ALL
                .iter()
                .map(|&l| (l, self.lanes[l as usize].depth))
                .collect(),
            cpu_used: SchedulerLane::ALL
                .iter()
                .map(|&l| (l, self.lanes[l as usize].cpu_used_us))
                .collect(),
            input_pressure: self.input_under_pressure(),
            degradation: self.current_degradation(),
            fairness: self.fairness_ratios(),
        }
    }
}

/// Scheduler degradation states.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchedulerDegradation {
    /// All lanes operating normally.
    Healthy,
    /// Input lane experiencing starvation (critical).
    InputStarvation { depth: usize, deferred: u64 },
    /// Bulk lane heavily shed, few items completing.
    BulkStarvation { shed_count: u64, completed_count: u64 },
    /// Control lane backlog growing.
    ControlBacklog { depth: usize, capacity: usize },
}

impl fmt::Display for SchedulerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::InputStarvation { depth, deferred } => {
                write!(f, "INPUT_STARVATION depth={} deferred={}", depth, deferred)
            }
            Self::BulkStarvation {
                shed_count,
                completed_count,
            } => write!(
                f,
                "BULK_STARVATION shed={} completed={}",
                shed_count, completed_count
            ),
            Self::ControlBacklog { depth, capacity } => {
                write!(f, "CONTROL_BACKLOG depth={}/{}", depth, capacity)
            }
        }
    }
}

/// Structured log entry for a scheduling epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerLogEntry {
    pub epoch: u64,
    pub depths: Vec<(SchedulerLane, usize)>,
    pub cpu_used: Vec<(SchedulerLane, f64)>,
    pub input_pressure: bool,
    pub degradation: SchedulerDegradation,
    pub fairness: Vec<(SchedulerLane, f64)>,
}

// ── B2: Bounded Input Ring ────────────────────────────────────────
//
// Fixed-capacity FIFO ring for the input lane with backpressure.
// Operations are O(1) amortized, bounded in time — no allocation on enqueue.
// AARSP Bead: ft-2p9cb.2.2.1

/// An item in the input ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRingItem {
    /// Sequence number (monotonically increasing).
    pub seq: u64,
    /// Pipeline stage this item is for.
    pub stage: LatencyStage,
    /// Estimated latency cost in microseconds.
    pub estimated_cost_us: f64,
    /// Correlation ID.
    pub correlation_id: String,
    /// Arrival timestamp in microseconds from epoch.
    pub arrived_us: u64,
    /// Deadline (0 = none).
    pub deadline_us: u64,
}

/// Backpressure signal from the input ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RingBackpressure {
    /// Ring has capacity, accept freely.
    Accept,
    /// Ring is above high-water mark — signal producer to slow down.
    SlowDown,
    /// Ring is full — reject or drop.
    Full,
}

impl fmt::Display for RingBackpressure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept => write!(f, "ACCEPT"),
            Self::SlowDown => write!(f, "SLOW_DOWN"),
            Self::Full => write!(f, "FULL"),
        }
    }
}

/// Configuration for the bounded input ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRingConfig {
    /// Fixed capacity of the ring.
    pub capacity: usize,
    /// High-water mark fraction (0.0..1.0) above which backpressure = SlowDown.
    pub high_water_mark: f64,
    /// Whether to track per-item latency from arrival to dequeue.
    pub track_sojourn: bool,
}

impl Default for InputRingConfig {
    fn default() -> Self {
        Self {
            capacity: 256,
            high_water_mark: 0.75,
            track_sojourn: true,
        }
    }
}

/// Diagnostic snapshot of the input ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRingSnapshot {
    pub capacity: usize,
    pub len: usize,
    pub total_enqueued: u64,
    pub total_dequeued: u64,
    pub total_dropped: u64,
    pub backpressure: RingBackpressure,
    pub head_seq: u64,
    pub tail_seq: u64,
    pub sojourn_mean_us: Option<f64>,
}

/// Bounded FIFO ring for the input lane.
///
/// # Invariants
///
/// 1. `len <= capacity` always.
/// 2. `head_seq <= tail_seq` (head is next to dequeue, tail is next to enqueue).
/// 3. `total_enqueued = total_dequeued + total_dropped + len`.
/// 4. O(1) enqueue and dequeue.
/// 5. Deterministic: same sequence of ops → same state.
#[derive(Debug, Clone)]
pub struct InputRing {
    config: InputRingConfig,
    buffer: Vec<Option<InputRingItem>>,
    head: usize,
    tail: usize,
    len: usize,
    next_seq: u64,
    total_enqueued: u64,
    total_dequeued: u64,
    total_dropped: u64,
    sojourn_sum_us: f64,
    sojourn_count: u64,
}

impl InputRing {
    /// Create a new input ring with the given configuration.
    pub fn new(config: InputRingConfig) -> Self {
        let cap = config.capacity.max(1);
        Self {
            buffer: (0..cap).map(|_| None).collect(),
            config: InputRingConfig {
                capacity: cap,
                ..config
            },
            head: 0,
            tail: 0,
            len: 0,
            next_seq: 1,
            total_enqueued: 0,
            total_dequeued: 0,
            total_dropped: 0,
            sojourn_sum_us: 0.0,
            sojourn_count: 0,
        }
    }

    /// Create a ring with default config.
    pub fn with_defaults() -> Self {
        Self::new(InputRingConfig::default())
    }

    /// Current number of items in the ring.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Is the ring empty?
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Is the ring full?
    pub fn is_full(&self) -> bool {
        self.len >= self.config.capacity
    }

    /// Current backpressure signal.
    pub fn backpressure(&self) -> RingBackpressure {
        if self.is_full() {
            RingBackpressure::Full
        } else if self.len as f64 / self.config.capacity as f64 >= self.config.high_water_mark {
            RingBackpressure::SlowDown
        } else {
            RingBackpressure::Accept
        }
    }

    /// Enqueue an item. Returns Ok(seq) on success, Err(item) if full.
    pub fn enqueue(
        &mut self,
        stage: LatencyStage,
        estimated_cost_us: f64,
        correlation_id: &str,
        arrived_us: u64,
        deadline_us: u64,
    ) -> Result<u64, RingBackpressure> {
        if self.is_full() {
            self.total_dropped += 1;
            return Err(RingBackpressure::Full);
        }

        let seq = self.next_seq;
        self.next_seq += 1;

        self.buffer[self.tail] = Some(InputRingItem {
            seq,
            stage,
            estimated_cost_us,
            correlation_id: correlation_id.to_string(),
            arrived_us,
            deadline_us,
        });
        self.tail = (self.tail + 1) % self.config.capacity;
        self.len += 1;
        self.total_enqueued += 1;

        Ok(seq)
    }

    /// Dequeue the oldest item. Returns None if empty.
    pub fn dequeue(&mut self, now_us: u64) -> Option<InputRingItem> {
        if self.is_empty() {
            return None;
        }

        let item = self.buffer[self.head].take()?;
        self.head = (self.head + 1) % self.config.capacity;
        self.len -= 1;
        self.total_dequeued += 1;

        if self.config.track_sojourn && now_us >= item.arrived_us {
            self.sojourn_sum_us += (now_us - item.arrived_us) as f64;
            self.sojourn_count += 1;
        }

        Some(item)
    }

    /// Peek at the head item without removing it.
    pub fn peek(&self) -> Option<&InputRingItem> {
        if self.is_empty() {
            None
        } else {
            self.buffer[self.head].as_ref()
        }
    }

    /// Mean sojourn time (time in ring) in microseconds, if tracked.
    pub fn mean_sojourn_us(&self) -> Option<f64> {
        if self.sojourn_count > 0 {
            Some(self.sojourn_sum_us / self.sojourn_count as f64)
        } else {
            None
        }
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> InputRingSnapshot {
        InputRingSnapshot {
            capacity: self.config.capacity,
            len: self.len,
            total_enqueued: self.total_enqueued,
            total_dequeued: self.total_dequeued,
            total_dropped: self.total_dropped,
            backpressure: self.backpressure(),
            head_seq: self
                .peek()
                .map(|i| i.seq)
                .unwrap_or(self.next_seq),
            tail_seq: self.next_seq,
            sojourn_mean_us: self.mean_sojourn_us(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        format!(
            "input_ring len={}/{} bp={} enq={} deq={} drop={}",
            self.len,
            self.config.capacity,
            self.backpressure(),
            self.total_enqueued,
            self.total_dequeued,
            self.total_dropped,
        )
    }

    /// Batch dequeue up to `max` items. Returns items in FIFO order.
    pub fn drain(&mut self, max: usize, now_us: u64) -> Vec<InputRingItem> {
        let count = max.min(self.len);
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(item) = self.dequeue(now_us) {
                items.push(item);
            } else {
                break;
            }
        }
        items
    }

    /// Dequeue items that have passed their deadline.
    /// Expired items are returned so the caller can handle them (e.g., log, escalate).
    pub fn drain_expired(&mut self, now_us: u64) -> Vec<InputRingItem> {
        let mut expired = Vec::new();
        let mut remaining = Vec::new();

        // Drain all items, separate expired from still-valid.
        let all = self.drain(self.len, now_us);
        for item in all {
            if item.deadline_us > 0 && now_us > item.deadline_us {
                expired.push(item);
            } else {
                remaining.push(item);
            }
        }

        // Re-enqueue non-expired items.
        for item in remaining {
            // Direct re-insert (bypass normal enqueue to preserve seq numbers).
            if self.len < self.config.capacity {
                self.buffer[self.tail] = Some(item);
                self.tail = (self.tail + 1) % self.config.capacity;
                self.len += 1;
                // Adjust counters to compensate for the drain+re-enqueue.
                self.total_dequeued -= 1;
            }
        }

        expired
    }

    /// Utilization fraction (0.0 to 1.0).
    pub fn utilization(&self) -> f64 {
        self.len as f64 / self.config.capacity as f64
    }

    /// Capacity of the ring.
    pub fn capacity(&self) -> usize {
        self.config.capacity
    }
}

// ── AARSP Bead: ft-2p9cb.2.3 — Priority Inheritance & Lock-Order ──

// AARSP Bead: ft-2p9cb.2.3.1

/// Priority level for work items. Higher numeric value = higher priority.
/// Used in priority inheritance to temporarily boost blocked low-priority work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    /// Background work — can be preempted freely.
    Background = 0,
    /// Normal interactive work.
    Normal = 1,
    /// Elevated — time-sensitive user action.
    Elevated = 2,
    /// Critical — keystroke path, must not be delayed.
    Critical = 3,
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Background => write!(f, "BACKGROUND"),
            Self::Normal => write!(f, "NORMAL"),
            Self::Elevated => write!(f, "ELEVATED"),
            Self::Critical => write!(f, "CRITICAL"),
        }
    }
}

impl Priority {
    /// All priority levels in ascending order.
    pub const ALL: [Priority; 4] = [
        Priority::Background,
        Priority::Normal,
        Priority::Elevated,
        Priority::Critical,
    ];
}

/// Maps pipeline stages to default priority levels.
pub fn stage_to_priority(stage: LatencyStage) -> Priority {
    match stage {
        LatencyStage::PtyCapture | LatencyStage::DeltaExtraction => Priority::Critical,
        LatencyStage::EventEmission | LatencyStage::WorkflowDispatch => Priority::Elevated,
        LatencyStage::PatternDetection | LatencyStage::ActionExecution => Priority::Normal,
        LatencyStage::StorageWrite
        | LatencyStage::ApiResponse
        | LatencyStage::EndToEndCapture
        | LatencyStage::EndToEndAction => Priority::Background,
    }
}

/// A resource that work items can contend over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Resource {
    /// Storage writer lock.
    StorageLock,
    /// Pattern engine lock.
    PatternLock,
    /// Event bus lock.
    EventBusLock,
    /// Workflow executor lock.
    WorkflowLock,
}

impl Resource {
    /// All resources in canonical lock order.
    /// Acquiring locks MUST follow this order to prevent deadlock.
    pub const LOCK_ORDER: [Resource; 4] = [
        Resource::StorageLock,
        Resource::PatternLock,
        Resource::EventBusLock,
        Resource::WorkflowLock,
    ];

    /// Position in canonical lock order (0-indexed).
    pub fn order_index(self) -> usize {
        match self {
            Self::StorageLock => 0,
            Self::PatternLock => 1,
            Self::EventBusLock => 2,
            Self::WorkflowLock => 3,
        }
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageLock => write!(f, "storage"),
            Self::PatternLock => write!(f, "pattern"),
            Self::EventBusLock => write!(f, "event_bus"),
            Self::WorkflowLock => write!(f, "workflow"),
        }
    }
}

/// A priority inheritance event — records when a task's effective priority was boosted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InheritanceEvent {
    /// Correlation ID of the task that received the boost.
    pub holder_id: String,
    /// Correlation ID of the higher-priority waiter that triggered the boost.
    pub waiter_id: String,
    /// Resource being contended.
    pub resource: Resource,
    /// Original priority of the holder.
    pub original_priority: Priority,
    /// Boosted priority (inherited from waiter).
    pub inherited_priority: Priority,
    /// Timestamp when inheritance was applied.
    pub applied_us: u64,
    /// Timestamp when inheritance was released (None if still active).
    pub released_us: Option<u64>,
}

/// Lock acquisition attempt result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LockResult {
    /// Lock acquired immediately.
    Acquired,
    /// Lock acquired after priority inheritance boosted the holder.
    AcquiredAfterInheritance {
        /// ID of the task whose priority was boosted.
        boosted_holder: String,
    },
    /// Lock denied — would violate lock ordering.
    OrderViolation {
        /// Resource we tried to acquire.
        requested: Resource,
        /// Resource we already hold that comes AFTER requested in canonical order.
        held_after: Resource,
    },
}

/// Configuration for the priority inheritance protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorityInheritanceConfig {
    /// Maximum chain depth for transitive inheritance.
    pub max_chain_depth: usize,
    /// Whether to enforce strict lock ordering.
    pub enforce_lock_order: bool,
    /// Maximum time (us) a boosted priority can persist before auto-release.
    pub max_inheritance_duration_us: u64,
}

impl Default for PriorityInheritanceConfig {
    fn default() -> Self {
        Self {
            max_chain_depth: 4,
            enforce_lock_order: true,
            max_inheritance_duration_us: 50_000, // 50ms
        }
    }
}

/// State of a single held lock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeldLock {
    /// Resource held.
    pub resource: Resource,
    /// Task holding the lock.
    pub holder_id: String,
    /// Original priority of the holder.
    pub original_priority: Priority,
    /// Current effective priority (may be boosted).
    pub effective_priority: Priority,
    /// Timestamp when lock was acquired.
    pub acquired_us: u64,
    /// Queue of waiters (correlation IDs), ordered by priority (highest first).
    pub waiters: Vec<(String, Priority)>,
}

/// Snapshot of the priority inheritance tracker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InheritanceSnapshot {
    /// Currently held locks.
    pub held_locks: Vec<HeldLock>,
    /// Total inheritance events since creation.
    pub total_inheritance_events: u64,
    /// Total lock-order violations prevented.
    pub total_order_violations: u64,
    /// Active inheritance chains (holder → waiter chain depth).
    pub active_chains: usize,
    /// Maximum chain depth observed.
    pub max_chain_depth_observed: usize,
}

/// Priority inheritance tracker. Manages lock state, detects inversions,
/// applies priority inheritance, and enforces lock ordering.
///
/// # Invariants
///
/// 1. A task's effective priority >= its original priority (boosted, never lowered).
/// 2. Lock ordering: if task holds resource A, it cannot acquire resource B where B < A.
/// 3. Inheritance is transitive up to `max_chain_depth`.
/// 4. When a holder releases a lock, its priority reverts to max(original, other inherited).
/// 5. Deterministic: same sequence of ops → same state.
#[derive(Debug, Clone)]
pub struct PriorityInheritanceTracker {
    config: PriorityInheritanceConfig,
    /// Resource → HeldLock state.
    locks: Vec<Option<HeldLock>>,
    /// History of inheritance events.
    events: Vec<InheritanceEvent>,
    max_events: usize,
    total_inheritance_events: u64,
    total_order_violations: u64,
    max_chain_depth_observed: usize,
}

impl PriorityInheritanceTracker {
    /// Create a new tracker with the given configuration.
    pub fn new(config: PriorityInheritanceConfig) -> Self {
        Self {
            config,
            locks: Resource::LOCK_ORDER.iter().map(|_| None).collect(),
            events: Vec::new(),
            max_events: 256,
            total_inheritance_events: 0,
            total_order_violations: 0,
            max_chain_depth_observed: 0,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(PriorityInheritanceConfig::default())
    }

    /// Attempt to acquire a resource lock.
    pub fn acquire(
        &mut self,
        resource: Resource,
        task_id: &str,
        priority: Priority,
        now_us: u64,
    ) -> LockResult {
        // Check lock-order violation: if we hold any lock with higher order index,
        // acquiring this one would violate canonical order.
        if self.config.enforce_lock_order {
            let requested_idx = resource.order_index();
            for lock_opt in &self.locks {
                if let Some(held) = lock_opt {
                    if held.holder_id == task_id && held.resource.order_index() > requested_idx {
                        self.total_order_violations += 1;
                        return LockResult::OrderViolation {
                            requested: resource,
                            held_after: held.resource,
                        };
                    }
                }
            }
        }

        let idx = resource.order_index();
        if self.locks[idx].is_none() {
            // Lock is free — acquire immediately.
            self.locks[idx] = Some(HeldLock {
                resource,
                holder_id: task_id.to_string(),
                original_priority: priority,
                effective_priority: priority,
                acquired_us: now_us,
                waiters: Vec::new(),
            });
            return LockResult::Acquired;
        }

        // Lock is held by another task. Apply priority inheritance if waiter has higher priority.
        let held = self.locks[idx].as_mut().unwrap();

        // If we already hold it, treat as re-entrant (acquired).
        if held.holder_id == task_id {
            return LockResult::Acquired;
        }

        // Add to waiter queue (sorted by priority, highest first).
        let insert_pos = held
            .waiters
            .iter()
            .position(|(_, p)| *p < priority)
            .unwrap_or(held.waiters.len());
        held.waiters
            .insert(insert_pos, (task_id.to_string(), priority));

        // Apply inheritance if waiter priority > holder effective priority.
        if priority > held.effective_priority {
            let event = InheritanceEvent {
                holder_id: held.holder_id.clone(),
                waiter_id: task_id.to_string(),
                resource,
                original_priority: held.original_priority,
                inherited_priority: priority,
                applied_us: now_us,
                released_us: None,
            };
            held.effective_priority = priority;
            self.total_inheritance_events += 1;

            if self.events.len() >= self.max_events {
                self.events.remove(0);
            }
            self.events.push(event);

            return LockResult::AcquiredAfterInheritance {
                boosted_holder: held.holder_id.clone(),
            };
        }

        // Waiter added but no inheritance needed; from the waiter's perspective
        // they are blocked. We return Acquired to signal the lock state was updated
        // (the caller should check if they are the holder).
        LockResult::Acquired
    }

    /// Release a resource lock. Returns the inheritance events that were resolved.
    pub fn release(&mut self, resource: Resource, task_id: &str, now_us: u64) -> Vec<String> {
        let idx = resource.order_index();
        let mut promoted = Vec::new();

        if let Some(held) = &self.locks[idx] {
            if held.holder_id != task_id {
                return promoted;
            }
        } else {
            return promoted;
        }

        // Close any open inheritance events for this resource.
        for event in &mut self.events {
            if event.resource == resource && event.released_us.is_none() {
                event.released_us = Some(now_us);
            }
        }

        let held = self.locks[idx].take().unwrap();

        // Promote the highest-priority waiter to be the new holder.
        if let Some((waiter_id, waiter_priority)) = held.waiters.first().cloned() {
            let remaining_waiters: Vec<_> = held.waiters[1..].to_vec();
            self.locks[idx] = Some(HeldLock {
                resource,
                holder_id: waiter_id.clone(),
                original_priority: waiter_priority,
                effective_priority: waiter_priority,
                acquired_us: now_us,
                waiters: remaining_waiters,
            });
            promoted.push(waiter_id);
        }

        promoted
    }

    /// Check if a task holds a specific resource.
    pub fn is_held_by(&self, resource: Resource, task_id: &str) -> bool {
        let idx = resource.order_index();
        self.locks[idx]
            .as_ref()
            .map(|h| h.holder_id == task_id)
            .unwrap_or(false)
    }

    /// Get effective priority of a task across all held locks.
    pub fn effective_priority(&self, task_id: &str) -> Option<Priority> {
        let mut max_priority: Option<Priority> = None;
        for lock_opt in &self.locks {
            if let Some(held) = lock_opt {
                if held.holder_id == task_id {
                    max_priority = Some(match max_priority {
                        Some(p) if p >= held.effective_priority => p,
                        _ => held.effective_priority,
                    });
                }
            }
        }
        max_priority
    }

    /// Validate lock ordering for a task's currently held locks.
    /// Returns list of violations (if any).
    pub fn check_lock_order(&self, task_id: &str) -> Vec<(Resource, Resource)> {
        let mut held_indices: Vec<(usize, Resource)> = Vec::new();
        for lock_opt in &self.locks {
            if let Some(held) = lock_opt {
                if held.holder_id == task_id {
                    held_indices.push((held.resource.order_index(), held.resource));
                }
            }
        }
        held_indices.sort_by_key(|(idx, _)| *idx);

        let mut violations = Vec::new();
        for w in held_indices.windows(2) {
            if w[0].0 >= w[1].0 {
                violations.push((w[0].1, w[1].1));
            }
        }
        violations
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> InheritanceSnapshot {
        let held_locks: Vec<_> = self.locks.iter().filter_map(|l| l.clone()).collect();
        let active_chains = held_locks
            .iter()
            .filter(|l| l.effective_priority > l.original_priority)
            .count();

        InheritanceSnapshot {
            held_locks,
            total_inheritance_events: self.total_inheritance_events,
            total_order_violations: self.total_order_violations,
            active_chains,
            max_chain_depth_observed: self.max_chain_depth_observed,
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let held = self.locks.iter().filter(|l| l.is_some()).count();
        let snap = self.snapshot();
        format!(
            "pi_tracker held={} inherit={} violations={} chains={}",
            held,
            snap.total_inheritance_events,
            snap.total_order_violations,
            snap.active_chains,
        )
    }

    /// Release all locks held by a task. Returns resources that were released.
    pub fn release_all(&mut self, task_id: &str, now_us: u64) -> Vec<Resource> {
        let mut released = Vec::new();
        for resource in Resource::LOCK_ORDER {
            if self.is_held_by(resource, task_id) {
                self.release(resource, task_id, now_us);
                released.push(resource);
            }
        }
        released
    }

    /// Expire stale inheritance — auto-release boosts that have persisted too long.
    /// Returns the number of inheritances expired.
    pub fn expire_stale_inheritance(&mut self, now_us: u64) -> usize {
        let max_dur = self.config.max_inheritance_duration_us;
        let mut expired_count = 0;

        for lock_opt in &mut self.locks {
            if let Some(held) = lock_opt {
                if held.effective_priority > held.original_priority
                    && now_us.saturating_sub(held.acquired_us) > max_dur
                {
                    held.effective_priority = held.original_priority;
                    expired_count += 1;
                }
            }
        }

        // Also close open events that are past duration.
        for event in &mut self.events {
            if event.released_us.is_none()
                && now_us.saturating_sub(event.applied_us) > max_dur
            {
                event.released_us = Some(now_us);
            }
        }

        expired_count
    }

    /// Count of currently held locks.
    pub fn held_count(&self) -> usize {
        self.locks.iter().filter(|l| l.is_some()).count()
    }

    /// Total number of waiters across all held locks.
    pub fn total_waiters(&self) -> usize {
        self.locks
            .iter()
            .filter_map(|l| l.as_ref())
            .map(|h| h.waiters.len())
            .sum()
    }
}

// AARSP Bead: ft-2p9cb.2.3.2

/// Degradation signal from the priority inheritance tracker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InheritanceDegradation {
    /// Everything is fine.
    Healthy,
    /// Too many concurrent inheritance chains — possible priority ceiling issue.
    ExcessiveInheritance {
        active_chains: usize,
        threshold: usize,
    },
    /// Lock contention is high — many waiters.
    HighContention {
        total_waiters: usize,
        threshold: usize,
    },
    /// Lock-order violations are accumulating.
    OrderViolationSpike {
        total_violations: u64,
        threshold: u64,
    },
}

impl fmt::Display for InheritanceDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::ExcessiveInheritance {
                active_chains,
                threshold,
            } => write!(f, "EXCESSIVE_INHERITANCE({}/{})", active_chains, threshold),
            Self::HighContention {
                total_waiters,
                threshold,
            } => write!(f, "HIGH_CONTENTION({}/{})", total_waiters, threshold),
            Self::OrderViolationSpike {
                total_violations,
                threshold,
            } => write!(f, "ORDER_VIOLATION_SPIKE({}/{})", total_violations, threshold),
        }
    }
}

/// Structured log entry for priority inheritance events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InheritanceLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Number of held locks.
    pub held_locks: usize,
    /// Total inheritance events so far.
    pub total_inheritance_events: u64,
    /// Total order violations so far.
    pub total_order_violations: u64,
    /// Active inheritance chains.
    pub active_chains: usize,
    /// Current degradation signal.
    pub degradation: InheritanceDegradation,
}

impl PriorityInheritanceTracker {
    /// Detect degradation based on current state.
    pub fn detect_degradation(&self) -> InheritanceDegradation {
        let snap = self.snapshot();

        // Threshold: more than 2 concurrent inheritance chains.
        if snap.active_chains > 2 {
            return InheritanceDegradation::ExcessiveInheritance {
                active_chains: snap.active_chains,
                threshold: 2,
            };
        }

        // Threshold: more than 8 total waiters.
        let total_waiters = self.total_waiters();
        if total_waiters > 8 {
            return InheritanceDegradation::HighContention {
                total_waiters,
                threshold: 8,
            };
        }

        // Threshold: more than 10 order violations.
        if snap.total_order_violations > 10 {
            return InheritanceDegradation::OrderViolationSpike {
                total_violations: snap.total_order_violations,
                threshold: 10,
            };
        }

        InheritanceDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self, now_us: u64) -> InheritanceLogEntry {
        let snap = self.snapshot();
        InheritanceLogEntry {
            timestamp_us: now_us,
            held_locks: snap.held_locks.len(),
            total_inheritance_events: snap.total_inheritance_events,
            total_order_violations: snap.total_order_violations,
            active_chains: snap.active_chains,
            degradation: self.detect_degradation(),
        }
    }
}

// ── AARSP Bead: ft-2p9cb.2.4 — Starvation Prevention & Fairness ──

// AARSP Bead: ft-2p9cb.2.4.1

/// Configuration for starvation prevention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StarvationConfig {
    /// Max consecutive epochs a lane can go unserviced before forced promotion.
    pub max_starved_epochs: u64,
    /// Fairness window size (epochs) for computing running averages.
    pub fairness_window: usize,
    /// Minimum share of CPU any lane must receive (0.0..1.0).
    pub min_lane_share: f64,
    /// Enable aging — deferred items get priority boost over time.
    pub enable_aging: bool,
    /// Aging boost interval: every N epochs, deferred items gain one priority level.
    pub aging_interval_epochs: u64,
}

impl Default for StarvationConfig {
    fn default() -> Self {
        Self {
            max_starved_epochs: 5,
            fairness_window: 20,
            min_lane_share: 0.05,
            enable_aging: true,
            aging_interval_epochs: 3,
        }
    }
}

/// Per-lane fairness state tracked over a sliding window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneFairnessState {
    /// Which lane.
    pub lane: SchedulerLane,
    /// Consecutive epochs with zero completions.
    pub starved_epochs: u64,
    /// CPU share over the fairness window (0.0..1.0).
    pub windowed_share: f64,
    /// Total completions in the fairness window.
    pub windowed_completions: u64,
    /// Total items deferred in the fairness window.
    pub windowed_deferred: u64,
    /// Whether this lane is currently being force-promoted.
    pub force_promoted: bool,
}

/// A starvation event — records when a lane was force-promoted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StarvationEvent {
    /// Epoch when detected.
    pub epoch: u64,
    /// Lane that was starving.
    pub lane: SchedulerLane,
    /// Consecutive starved epochs before promotion.
    pub starved_epochs: u64,
    /// CPU share at the time of detection.
    pub cpu_share: f64,
}

/// Fairness snapshot across all lanes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FairnessSnapshot {
    /// Per-lane fairness state.
    pub lanes: Vec<LaneFairnessState>,
    /// Gini coefficient of CPU shares (0.0 = perfect equality, 1.0 = total inequality).
    pub gini_coefficient: f64,
    /// Total starvation events since creation.
    pub total_starvation_events: u64,
    /// Whether any lane is currently starving.
    pub any_starving: bool,
}

/// Starvation prevention tracker. Monitors per-lane service rates,
/// detects starvation, and triggers force-promotions.
///
/// # Invariants
///
/// 1. No lane goes more than `max_starved_epochs` without service.
/// 2. Every lane's windowed share >= min_lane_share (or force-promotion triggers).
/// 3. Gini coefficient is in [0.0, 1.0].
/// 4. Deterministic: same epoch observations → same fairness state.
#[derive(Debug, Clone)]
pub struct StarvationTracker {
    config: StarvationConfig,
    /// Per-lane state.
    lanes: Vec<LaneFairnessState>,
    /// History of per-epoch CPU shares per lane (ring buffer).
    share_history: Vec<Vec<f64>>,
    history_head: usize,
    epoch: u64,
    events: Vec<StarvationEvent>,
    max_events: usize,
    total_starvation_events: u64,
}

impl StarvationTracker {
    /// Create a new tracker.
    pub fn new(config: StarvationConfig) -> Self {
        let window = config.fairness_window.max(1);
        Self {
            lanes: vec![
                LaneFairnessState {
                    lane: SchedulerLane::Input,
                    starved_epochs: 0,
                    windowed_share: 0.0,
                    windowed_completions: 0,
                    windowed_deferred: 0,
                    force_promoted: false,
                },
                LaneFairnessState {
                    lane: SchedulerLane::Control,
                    starved_epochs: 0,
                    windowed_share: 0.0,
                    windowed_completions: 0,
                    windowed_deferred: 0,
                    force_promoted: false,
                },
                LaneFairnessState {
                    lane: SchedulerLane::Bulk,
                    starved_epochs: 0,
                    windowed_share: 0.0,
                    windowed_completions: 0,
                    windowed_deferred: 0,
                    force_promoted: false,
                },
            ],
            share_history: vec![vec![0.0; 3]; window],
            history_head: 0,
            epoch: 0,
            events: Vec::new(),
            max_events: 256,
            total_starvation_events: 0,
            config: StarvationConfig {
                fairness_window: window,
                ..config
            },
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(StarvationConfig::default())
    }

    /// Record one epoch's observations: completions and CPU shares per lane.
    /// Returns list of lanes that are now force-promoted.
    pub fn observe_epoch(
        &mut self,
        completions: &[u64; 3],
        cpu_shares: &[f64; 3],
    ) -> Vec<SchedulerLane> {
        self.epoch += 1;
        let mut promoted = Vec::new();

        // Record shares in ring buffer.
        self.share_history[self.history_head] = cpu_shares.to_vec();
        self.history_head = (self.history_head + 1) % self.config.fairness_window;

        // Update per-lane state.
        for (i, lane_state) in self.lanes.iter_mut().enumerate() {
            if completions[i] == 0 {
                lane_state.starved_epochs += 1;
            } else {
                lane_state.starved_epochs = 0;
                lane_state.force_promoted = false;
            }

            // Compute windowed share.
            let mut sum = 0.0;
            let mut count = 0;
            for entry in &self.share_history {
                if entry[i] > 0.0 || count < self.epoch as usize {
                    sum += entry[i];
                    count += 1;
                }
            }
            lane_state.windowed_share = if count > 0 {
                sum / count as f64
            } else {
                0.0
            };
            lane_state.windowed_completions = completions[i];
            lane_state.windowed_deferred = 0; // will be updated externally

            // Check starvation.
            if lane_state.starved_epochs >= self.config.max_starved_epochs
                && !lane_state.force_promoted
            {
                lane_state.force_promoted = true;
                self.total_starvation_events += 1;

                let event = StarvationEvent {
                    epoch: self.epoch,
                    lane: lane_state.lane,
                    starved_epochs: lane_state.starved_epochs,
                    cpu_share: lane_state.windowed_share,
                };
                if self.events.len() >= self.max_events {
                    self.events.remove(0);
                }
                self.events.push(event);

                promoted.push(lane_state.lane);
            }
        }

        promoted
    }

    /// Compute the Gini coefficient of current windowed shares.
    pub fn gini_coefficient(&self) -> f64 {
        let shares: Vec<f64> = self.lanes.iter().map(|l| l.windowed_share).collect();
        let n = shares.len() as f64;
        if n == 0.0 {
            return 0.0;
        }
        let mean = shares.iter().sum::<f64>() / n;
        if mean <= 0.0 {
            return 0.0;
        }

        let mut sum_abs_diff = 0.0;
        for i in 0..shares.len() {
            for j in 0..shares.len() {
                sum_abs_diff += (shares[i] - shares[j]).abs();
            }
        }

        sum_abs_diff / (2.0 * n * n * mean)
    }

    /// Whether any lane is currently starving.
    pub fn any_starving(&self) -> bool {
        self.lanes.iter().any(|l| l.force_promoted)
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> FairnessSnapshot {
        FairnessSnapshot {
            lanes: self.lanes.clone(),
            gini_coefficient: self.gini_coefficient(),
            total_starvation_events: self.total_starvation_events,
            any_starving: self.any_starving(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "fairness gini={:.3} starving={} events={} epoch={}",
            snap.gini_coefficient,
            snap.any_starving,
            snap.total_starvation_events,
            self.epoch,
        )
    }

    /// Current epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Get lane fairness state.
    pub fn lane_state(&self, lane: SchedulerLane) -> &LaneFairnessState {
        &self.lanes[lane as usize]
    }

    /// Reset starvation counters for all lanes.
    pub fn reset(&mut self) {
        for lane_state in &mut self.lanes {
            lane_state.starved_epochs = 0;
            lane_state.force_promoted = false;
            lane_state.windowed_share = 0.0;
            lane_state.windowed_completions = 0;
            lane_state.windowed_deferred = 0;
        }
        self.epoch = 0;
        self.total_starvation_events = 0;
        self.events.clear();
        self.history_head = 0;
        for entry in &mut self.share_history {
            for v in entry.iter_mut() {
                *v = 0.0;
            }
        }
    }

    /// Get the most recent starvation events (up to limit).
    pub fn recent_events(&self, limit: usize) -> &[StarvationEvent] {
        let start = self.events.len().saturating_sub(limit);
        &self.events[start..]
    }

    /// Whether a specific lane is force-promoted.
    pub fn is_force_promoted(&self, lane: SchedulerLane) -> bool {
        self.lanes[lane as usize].force_promoted
    }
}

// AARSP Bead: ft-2p9cb.2.4.2

/// Degradation signal from the starvation tracker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FairnessDegradation {
    /// Everything is fine.
    Healthy,
    /// One or more lanes are starving.
    LaneStarvation {
        starving_lanes: Vec<SchedulerLane>,
    },
    /// Gini coefficient is too high — severe unfairness.
    SevereUnfairness {
        gini: f64,
        threshold: f64,
    },
    /// Force promotions are happening too frequently.
    PromotionStorm {
        events_in_window: u64,
        threshold: u64,
    },
}

impl fmt::Display for FairnessDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::LaneStarvation { starving_lanes } => {
                write!(f, "LANE_STARVATION({:?})", starving_lanes)
            }
            Self::SevereUnfairness { gini, threshold } => {
                write!(f, "SEVERE_UNFAIRNESS(gini={:.3}/thresh={:.3})", gini, threshold)
            }
            Self::PromotionStorm {
                events_in_window,
                threshold,
            } => write!(f, "PROMOTION_STORM({}/{})", events_in_window, threshold),
        }
    }
}

/// Structured log entry for fairness/starvation events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FairnessLogEntry {
    /// Epoch.
    pub epoch: u64,
    /// Per-lane windowed shares.
    pub shares: Vec<f64>,
    /// Per-lane starved epoch counts.
    pub starved_epochs: Vec<u64>,
    /// Gini coefficient.
    pub gini_coefficient: f64,
    /// Whether any lane is starving.
    pub any_starving: bool,
    /// Degradation signal.
    pub degradation: FairnessDegradation,
}

impl StarvationTracker {
    /// Detect degradation based on current state.
    pub fn detect_degradation(&self) -> FairnessDegradation {
        // Check for lane starvation.
        let starving: Vec<SchedulerLane> = self
            .lanes
            .iter()
            .filter(|l| l.force_promoted)
            .map(|l| l.lane)
            .collect();
        if !starving.is_empty() {
            return FairnessDegradation::LaneStarvation {
                starving_lanes: starving,
            };
        }

        // Check Gini coefficient (threshold: 0.5).
        let gini = self.gini_coefficient();
        if gini > 0.5 {
            return FairnessDegradation::SevereUnfairness {
                gini,
                threshold: 0.5,
            };
        }

        // Check for promotion storms (>5 events in last window).
        if self.total_starvation_events > 5 {
            return FairnessDegradation::PromotionStorm {
                events_in_window: self.total_starvation_events,
                threshold: 5,
            };
        }

        FairnessDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self) -> FairnessLogEntry {
        FairnessLogEntry {
            epoch: self.epoch,
            shares: self.lanes.iter().map(|l| l.windowed_share).collect(),
            starved_epochs: self.lanes.iter().map(|l| l.starved_epochs).collect(),
            gini_coefficient: self.gini_coefficient(),
            any_starving: self.any_starving(),
            degradation: self.detect_degradation(),
        }
    }
}

// ── AARSP Bead: ft-2p9cb.3.1 — Memory Ownership Graph & Pool ──────

// AARSP Bead: ft-2p9cb.3.1.1

/// Memory ownership domain — identifies which subsystem owns an allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MemoryDomain {
    /// PTY capture buffers (hot path).
    PtyCapture,
    /// Delta extraction scratch space.
    DeltaExtraction,
    /// Storage write staging area.
    StorageWrite,
    /// Pattern detection working set.
    PatternDetection,
    /// Event bus message queues.
    EventBus,
    /// Workflow executor state.
    WorkflowEngine,
    /// Scrollback ring buffers.
    Scrollback,
    /// Shared/uncategorized.
    Shared,
}

impl MemoryDomain {
    /// All domains in canonical order.
    pub const ALL: [MemoryDomain; 8] = [
        MemoryDomain::PtyCapture,
        MemoryDomain::DeltaExtraction,
        MemoryDomain::StorageWrite,
        MemoryDomain::PatternDetection,
        MemoryDomain::EventBus,
        MemoryDomain::WorkflowEngine,
        MemoryDomain::Scrollback,
        MemoryDomain::Shared,
    ];
}

impl fmt::Display for MemoryDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PtyCapture => write!(f, "pty_capture"),
            Self::DeltaExtraction => write!(f, "delta_extract"),
            Self::StorageWrite => write!(f, "storage_write"),
            Self::PatternDetection => write!(f, "pattern_detect"),
            Self::EventBus => write!(f, "event_bus"),
            Self::WorkflowEngine => write!(f, "workflow"),
            Self::Scrollback => write!(f, "scrollback"),
            Self::Shared => write!(f, "shared"),
        }
    }
}

/// Maps pipeline stages to their primary memory domain.
pub fn stage_to_domain(stage: LatencyStage) -> MemoryDomain {
    match stage {
        LatencyStage::PtyCapture => MemoryDomain::PtyCapture,
        LatencyStage::DeltaExtraction => MemoryDomain::DeltaExtraction,
        LatencyStage::StorageWrite => MemoryDomain::StorageWrite,
        LatencyStage::PatternDetection => MemoryDomain::PatternDetection,
        LatencyStage::EventEmission => MemoryDomain::EventBus,
        LatencyStage::WorkflowDispatch | LatencyStage::ActionExecution => {
            MemoryDomain::WorkflowEngine
        }
        LatencyStage::ApiResponse
        | LatencyStage::EndToEndCapture
        | LatencyStage::EndToEndAction => MemoryDomain::Shared,
    }
}

/// Configuration for a memory pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Domain this pool serves.
    pub domain: MemoryDomain,
    /// Fixed block size in bytes.
    pub block_size: usize,
    /// Initial number of blocks.
    pub initial_blocks: usize,
    /// Maximum blocks (hard cap).
    pub max_blocks: usize,
    /// High-water mark fraction for backpressure (0.0..1.0).
    pub high_water_mark: f64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            domain: MemoryDomain::Shared,
            block_size: 4096,
            initial_blocks: 64,
            max_blocks: 1024,
            high_water_mark: 0.85,
        }
    }
}

/// Allocation result from a pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllocResult {
    /// Allocated from free list.
    FromFreeList { block_id: u64 },
    /// Allocated a new block (pool grew).
    Grown { block_id: u64 },
    /// Pool is at max capacity — allocation refused.
    PoolExhausted,
}

/// Per-pool diagnostic snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolSnapshot {
    /// Domain this pool serves.
    pub domain: MemoryDomain,
    /// Block size in bytes.
    pub block_size: usize,
    /// Total blocks allocated (in use + free list).
    pub total_blocks: usize,
    /// Blocks currently in use.
    pub in_use: usize,
    /// Blocks on the free list.
    pub free_count: usize,
    /// Maximum blocks allowed.
    pub max_blocks: usize,
    /// Total allocations since creation.
    pub total_allocs: u64,
    /// Total frees since creation.
    pub total_frees: u64,
    /// Total allocation failures (pool exhausted).
    pub total_exhausted: u64,
    /// Utilization fraction (0.0..1.0).
    pub utilization: f64,
}

/// Fixed-block memory pool. O(1) alloc/free via free list.
///
/// # Invariants
///
/// 1. `in_use + free_count == total_blocks` always.
/// 2. `total_blocks <= max_blocks` always.
/// 3. `total_allocs = total_frees + in_use + total_exhausted`... no:
///    `total_allocs = total_frees + in_use` (exhausted are refused, not allocated).
/// 4. O(1) allocate and free.
/// 5. Deterministic: same sequence of ops → same state.
#[derive(Debug, Clone)]
pub struct MemoryPool {
    config: PoolConfig,
    free_list: Vec<u64>,
    next_block_id: u64,
    total_blocks: usize,
    in_use: usize,
    total_allocs: u64,
    total_frees: u64,
    total_exhausted: u64,
}

impl MemoryPool {
    /// Create a new pool.
    pub fn new(config: PoolConfig) -> Self {
        let initial = config.initial_blocks.min(config.max_blocks);
        let free_list: Vec<u64> = (0..initial as u64).collect();
        Self {
            next_block_id: initial as u64,
            total_blocks: initial,
            in_use: 0,
            free_list,
            total_allocs: 0,
            total_frees: 0,
            total_exhausted: 0,
            config,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(PoolConfig::default())
    }

    /// Allocate a block.
    pub fn allocate(&mut self) -> AllocResult {
        // Try free list first.
        if let Some(block_id) = self.free_list.pop() {
            self.in_use += 1;
            self.total_allocs += 1;
            return AllocResult::FromFreeList { block_id };
        }

        // Try growing.
        if self.total_blocks < self.config.max_blocks {
            let block_id = self.next_block_id;
            self.next_block_id += 1;
            self.total_blocks += 1;
            self.in_use += 1;
            self.total_allocs += 1;
            return AllocResult::Grown { block_id };
        }

        self.total_exhausted += 1;
        AllocResult::PoolExhausted
    }

    /// Free a block (return to free list).
    pub fn free(&mut self, block_id: u64) {
        self.free_list.push(block_id);
        self.in_use = self.in_use.saturating_sub(1);
        self.total_frees += 1;
    }

    /// Current utilization (in_use / total_blocks).
    pub fn utilization(&self) -> f64 {
        if self.total_blocks == 0 {
            0.0
        } else {
            self.in_use as f64 / self.total_blocks as f64
        }
    }

    /// Whether pool is under pressure (above high-water mark).
    pub fn under_pressure(&self) -> bool {
        self.utilization() >= self.config.high_water_mark
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            domain: self.config.domain,
            block_size: self.config.block_size,
            total_blocks: self.total_blocks,
            in_use: self.in_use,
            free_count: self.free_list.len(),
            max_blocks: self.config.max_blocks,
            total_allocs: self.total_allocs,
            total_frees: self.total_frees,
            total_exhausted: self.total_exhausted,
            utilization: self.utilization(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        format!(
            "pool[{}] {}/{} util={:.1}% alloc={} free={} exhausted={}",
            self.config.domain,
            self.in_use,
            self.total_blocks,
            self.utilization() * 100.0,
            self.total_allocs,
            self.total_frees,
            self.total_exhausted,
        )
    }

    /// Domain this pool serves.
    pub fn domain(&self) -> MemoryDomain {
        self.config.domain
    }

    /// In-use count.
    pub fn in_use(&self) -> usize {
        self.in_use
    }

    /// Free count.
    pub fn free_count(&self) -> usize {
        self.free_list.len()
    }

    /// Total blocks allocated (in use + free).
    pub fn total_blocks(&self) -> usize {
        self.total_blocks
    }

    /// Shrink pool: return excess free blocks to reclaim memory.
    /// Returns number of blocks reclaimed.
    pub fn shrink(&mut self, target_free: usize) -> usize {
        let excess = self.free_list.len().saturating_sub(target_free);
        if excess > 0 {
            self.free_list.truncate(self.free_list.len() - excess);
            self.total_blocks -= excess;
        }
        excess
    }

    /// Reset pool to initial state.
    pub fn reset(&mut self) {
        let initial = self.config.initial_blocks.min(self.config.max_blocks);
        self.free_list = (0..initial as u64).collect();
        self.next_block_id = initial as u64;
        self.total_blocks = initial;
        self.in_use = 0;
        self.total_allocs = 0;
        self.total_frees = 0;
        self.total_exhausted = 0;
    }
}

// AARSP Bead: ft-2p9cb.3.1.2

/// Degradation signal from the memory pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PoolDegradation {
    /// Pool is healthy.
    Healthy,
    /// Pool is under pressure (utilization above high-water mark).
    HighUtilization { utilization: f64, threshold: f64 },
    /// Pool is exhausted — allocations are failing.
    Exhausted { total_exhausted: u64 },
    /// Pool is fragmented — many blocks but high free count.
    Fragmented { total_blocks: usize, free_count: usize },
}

impl fmt::Display for PoolDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::HighUtilization {
                utilization,
                threshold,
            } => write!(f, "HIGH_UTIL({:.1}%/thresh={:.1}%)", utilization * 100.0, threshold * 100.0),
            Self::Exhausted { total_exhausted } => write!(f, "EXHAUSTED({})", total_exhausted),
            Self::Fragmented {
                total_blocks,
                free_count,
            } => write!(f, "FRAGMENTED({}/{}free)", total_blocks, free_count),
        }
    }
}

/// Structured log entry for pool health.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolLogEntry {
    /// Domain.
    pub domain: MemoryDomain,
    /// Utilization.
    pub utilization: f64,
    /// In use.
    pub in_use: usize,
    /// Total blocks.
    pub total_blocks: usize,
    /// Degradation signal.
    pub degradation: PoolDegradation,
}

impl MemoryPool {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> PoolDegradation {
        if self.total_exhausted > 0 {
            return PoolDegradation::Exhausted {
                total_exhausted: self.total_exhausted,
            };
        }

        if self.under_pressure() {
            return PoolDegradation::HighUtilization {
                utilization: self.utilization(),
                threshold: self.config.high_water_mark,
            };
        }

        // Fragmentation: total blocks > 2x initial and > 50% free.
        if self.total_blocks > self.config.initial_blocks * 2
            && self.free_list.len() > self.total_blocks / 2
        {
            return PoolDegradation::Fragmented {
                total_blocks: self.total_blocks,
                free_count: self.free_list.len(),
            };
        }

        PoolDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self) -> PoolLogEntry {
        PoolLogEntry {
            domain: self.config.domain,
            utilization: self.utilization(),
            in_use: self.in_use,
            total_blocks: self.total_blocks,
            degradation: self.detect_degradation(),
        }
    }
}

// ── AARSP Bead: ft-2p9cb.3.2 — Zero-Copy Ingestion Parser ──────

// AARSP Bead: ft-2p9cb.3.2.1

/// Ingestion chunk — a borrowed byte slice with metadata for zero-copy parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestChunk {
    /// Source pane ID.
    pub pane_id: u64,
    /// Byte offset in the source stream.
    pub offset: u64,
    /// Length of this chunk.
    pub length: usize,
    /// Whether the chunk ends at a line boundary.
    pub line_aligned: bool,
    /// Timestamp of capture.
    pub captured_us: u64,
}

/// Parsing result from the ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParseResult {
    /// Complete line(s) found — ready for downstream.
    Complete {
        lines: usize,
        bytes_consumed: usize,
    },
    /// Partial data — need more input.
    Partial {
        bytes_buffered: usize,
    },
    /// Invalid/corrupt data detected.
    Invalid {
        bytes_skipped: usize,
        reason: String,
    },
}

/// Configuration for the zero-copy ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestParserConfig {
    /// Maximum line length before forced split.
    pub max_line_bytes: usize,
    /// Maximum chunks to buffer before flushing.
    pub max_buffered_chunks: usize,
    /// Whether to strip ANSI escape sequences in-place.
    pub strip_escapes: bool,
    /// Whether to compute FNV-1a checksum for integrity.
    pub checksum: bool,
}

impl Default for IngestParserConfig {
    fn default() -> Self {
        Self {
            max_line_bytes: 16384,
            max_buffered_chunks: 64,
            strip_escapes: false,
            checksum: true,
        }
    }
}

/// Diagnostic snapshot of the ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestParserSnapshot {
    /// Total bytes processed.
    pub total_bytes: u64,
    /// Total lines emitted.
    pub total_lines: u64,
    /// Total chunks processed.
    pub total_chunks: u64,
    /// Total invalid/corrupt bytes skipped.
    pub total_invalid_bytes: u64,
    /// Buffered bytes awaiting next chunk.
    pub buffered_bytes: usize,
    /// Zero-copy ratio: fraction of bytes processed without copying.
    pub zero_copy_ratio: f64,
}

/// Zero-copy ingestion parser. Processes byte streams into lines with
/// minimal data movement.
///
/// # Invariants
///
/// 1. `total_bytes = total_consumed + buffered_bytes + total_invalid_bytes`.
/// 2. Zero-copy ratio is always in [0.0, 1.0].
/// 3. Lines are emitted in order.
/// 4. Deterministic: same byte sequence → same parse results.
#[derive(Debug, Clone)]
pub struct IngestParser {
    config: IngestParserConfig,
    buffer: Vec<u8>,
    total_bytes: u64,
    total_lines: u64,
    total_chunks: u64,
    total_invalid_bytes: u64,
    total_consumed: u64,
    zero_copy_bytes: u64,
}

impl IngestParser {
    /// Create a new parser.
    pub fn new(config: IngestParserConfig) -> Self {
        Self {
            buffer: Vec::new(),
            total_bytes: 0,
            total_lines: 0,
            total_chunks: 0,
            total_invalid_bytes: 0,
            total_consumed: 0,
            zero_copy_bytes: 0,
            config,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(IngestParserConfig::default())
    }

    /// Feed a chunk of bytes. Returns parsing result.
    pub fn feed(&mut self, data: &[u8]) -> ParseResult {
        self.total_bytes += data.len() as u64;
        self.total_chunks += 1;

        // If buffer is empty and data contains a newline, we can process zero-copy.
        if self.buffer.is_empty() {
            if let Some(newline_pos) = memchr_last_newline(data) {
                let lines = count_newlines(&data[..=newline_pos]);
                let consumed = newline_pos + 1;
                self.total_lines += lines as u64;
                self.total_consumed += consumed as u64;
                self.zero_copy_bytes += consumed as u64;

                // Buffer the remainder.
                if consumed < data.len() {
                    self.buffer.extend_from_slice(&data[consumed..]);
                }

                return ParseResult::Complete {
                    lines,
                    bytes_consumed: consumed,
                };
            }

            // No newline — check for max line length.
            if data.len() > self.config.max_line_bytes {
                self.total_invalid_bytes += data.len() as u64;
                return ParseResult::Invalid {
                    bytes_skipped: data.len(),
                    reason: "line exceeds max_line_bytes".to_string(),
                };
            }

            // Buffer it.
            self.buffer.extend_from_slice(data);
            return ParseResult::Partial {
                bytes_buffered: self.buffer.len(),
            };
        }

        // We have buffered data — append and scan.
        self.buffer.extend_from_slice(data);

        if let Some(newline_pos) = memchr_last_newline(&self.buffer) {
            let lines = count_newlines(&self.buffer[..=newline_pos]);
            let consumed = newline_pos + 1;
            self.total_lines += lines as u64;
            self.total_consumed += consumed as u64;

            // Keep remainder in buffer.
            let remainder = self.buffer[consumed..].to_vec();
            self.buffer = remainder;

            return ParseResult::Complete {
                lines,
                bytes_consumed: consumed,
            };
        }

        // Check max buffer size.
        if self.buffer.len() > self.config.max_line_bytes {
            let skipped = self.buffer.len();
            self.total_invalid_bytes += skipped as u64;
            self.buffer.clear();
            return ParseResult::Invalid {
                bytes_skipped: skipped,
                reason: "buffered line exceeds max_line_bytes".to_string(),
            };
        }

        ParseResult::Partial {
            bytes_buffered: self.buffer.len(),
        }
    }

    /// Flush any remaining buffered data as a final line.
    pub fn flush(&mut self) -> Option<ParseResult> {
        if self.buffer.is_empty() {
            return None;
        }

        let len = self.buffer.len();
        self.total_lines += 1;
        self.total_consumed += len as u64;
        self.buffer.clear();

        Some(ParseResult::Complete {
            lines: 1,
            bytes_consumed: len,
        })
    }

    /// Zero-copy ratio.
    pub fn zero_copy_ratio(&self) -> f64 {
        if self.total_consumed == 0 {
            0.0
        } else {
            self.zero_copy_bytes as f64 / self.total_consumed as f64
        }
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> IngestParserSnapshot {
        IngestParserSnapshot {
            total_bytes: self.total_bytes,
            total_lines: self.total_lines,
            total_chunks: self.total_chunks,
            total_invalid_bytes: self.total_invalid_bytes,
            buffered_bytes: self.buffer.len(),
            zero_copy_ratio: self.zero_copy_ratio(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        format!(
            "ingest bytes={} lines={} chunks={} zc={:.1}% buf={}",
            self.total_bytes,
            self.total_lines,
            self.total_chunks,
            self.zero_copy_ratio() * 100.0,
            self.buffer.len(),
        )
    }

    /// Buffered byte count.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Reset parser state.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.total_bytes = 0;
        self.total_lines = 0;
        self.total_chunks = 0;
        self.total_invalid_bytes = 0;
        self.total_consumed = 0;
        self.zero_copy_bytes = 0;
    }

    /// Total bytes processed.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Total lines emitted.
    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    /// Total chunks processed.
    pub fn total_chunks(&self) -> u64 {
        self.total_chunks
    }
}

// AARSP Bead: ft-2p9cb.3.2.2

/// Degradation signal from the ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IngestDegradation {
    /// Parser is healthy.
    Healthy,
    /// High buffer pressure — too much data buffered.
    HighBufferPressure {
        buffered_bytes: usize,
        max_line_bytes: usize,
    },
    /// Data corruption detected.
    DataCorruption {
        invalid_bytes: u64,
        total_bytes: u64,
    },
    /// Low zero-copy ratio — too much data is being copied.
    LowZeroCopy {
        ratio: f64,
        threshold: f64,
    },
}

impl fmt::Display for IngestDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::HighBufferPressure {
                buffered_bytes,
                max_line_bytes,
            } => write!(f, "HIGH_BUFFER({}/{})", buffered_bytes, max_line_bytes),
            Self::DataCorruption {
                invalid_bytes,
                total_bytes,
            } => write!(f, "CORRUPT({}/{})", invalid_bytes, total_bytes),
            Self::LowZeroCopy { ratio, threshold } => {
                write!(f, "LOW_ZC({:.1}%/thresh={:.1}%)", ratio * 100.0, threshold * 100.0)
            }
        }
    }
}

/// Structured log entry for ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestLogEntry {
    /// Total bytes.
    pub total_bytes: u64,
    /// Total lines.
    pub total_lines: u64,
    /// Zero-copy ratio.
    pub zero_copy_ratio: f64,
    /// Buffered bytes.
    pub buffered_bytes: usize,
    /// Degradation signal.
    pub degradation: IngestDegradation,
}

impl IngestParser {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> IngestDegradation {
        // Check buffer pressure (>75% of max line length).
        if self.buffer.len() > self.config.max_line_bytes * 3 / 4 {
            return IngestDegradation::HighBufferPressure {
                buffered_bytes: self.buffer.len(),
                max_line_bytes: self.config.max_line_bytes,
            };
        }

        // Check data corruption (>1% invalid).
        if self.total_bytes > 100 && self.total_invalid_bytes * 100 > self.total_bytes {
            return IngestDegradation::DataCorruption {
                invalid_bytes: self.total_invalid_bytes,
                total_bytes: self.total_bytes,
            };
        }

        // Check zero-copy ratio (< 50% after sufficient data).
        if self.total_consumed > 1000 && self.zero_copy_ratio() < 0.5 {
            return IngestDegradation::LowZeroCopy {
                ratio: self.zero_copy_ratio(),
                threshold: 0.5,
            };
        }

        IngestDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self) -> IngestLogEntry {
        IngestLogEntry {
            total_bytes: self.total_bytes,
            total_lines: self.total_lines,
            zero_copy_ratio: self.zero_copy_ratio(),
            buffered_bytes: self.buffer.len(),
            degradation: self.detect_degradation(),
        }
    }
}

/// Find the position of the first newline byte in a slice.
fn memchr_newline(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| b == b'\n')
}

/// Find the position of the last newline byte in a slice.
fn memchr_last_newline(data: &[u8]) -> Option<usize> {
    data.iter().rposition(|&b| b == b'\n')
}

/// Count newline bytes in a slice.
fn count_newlines(data: &[u8]) -> usize {
    data.iter().filter(|&&b| b == b'\n').count()
}

// ── C3: Tiered Scrollback Memory Hierarchy ─────────────────────────

/// Scrollback storage tier — data migrates Hot → Warm → Cold as it ages.
///
/// # Invariants
/// - Hot tier: O(1) random access, RAM-resident, bounded by `hot_max_bytes`.
/// - Warm tier: mmap-backed, O(1) page-fault access, bounded by `warm_max_bytes`.
/// - Cold tier: compressed (zstd-style length-prefix), sequential access only.
/// - Tier transitions are monotonic: once demoted, data never promotes back.
/// - Total bytes across all tiers = sum of segment sizes (conservation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScrollbackTier {
    /// RAM-resident, O(1) random access.
    Hot,
    /// mmap-backed file segments, page-fault access.
    Warm,
    /// Compressed segments, sequential decompression required.
    Cold,
}

impl std::fmt::Display for ScrollbackTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScrollbackTier::Hot => write!(f, "HOT"),
            ScrollbackTier::Warm => write!(f, "WARM"),
            ScrollbackTier::Cold => write!(f, "COLD"),
        }
    }
}

impl ScrollbackTier {
    /// Ordered tiers from fastest to slowest.
    pub const ALL: [ScrollbackTier; 3] = [
        ScrollbackTier::Hot,
        ScrollbackTier::Warm,
        ScrollbackTier::Cold,
    ];

    /// Numeric rank (0=Hot, 1=Warm, 2=Cold).
    pub fn rank(self) -> usize {
        match self {
            ScrollbackTier::Hot => 0,
            ScrollbackTier::Warm => 1,
            ScrollbackTier::Cold => 2,
        }
    }

    /// Next colder tier, if any.
    pub fn demote(self) -> Option<ScrollbackTier> {
        match self {
            ScrollbackTier::Hot => Some(ScrollbackTier::Warm),
            ScrollbackTier::Warm => Some(ScrollbackTier::Cold),
            ScrollbackTier::Cold => None,
        }
    }
}

/// Per-tier capacity and latency budget configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierConfig {
    pub tier: ScrollbackTier,
    /// Maximum bytes this tier may hold.
    pub max_bytes: u64,
    /// Target retrieval latency in microseconds (p99).
    pub target_latency_us: u64,
    /// Compression ratio estimate (1.0 = no compression, 0.25 = 4:1).
    pub compression_ratio: f64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            tier: ScrollbackTier::Hot,
            max_bytes: 64 * 1024 * 1024, // 64 MiB
            target_latency_us: 10,        // 10 µs
            compression_ratio: 1.0,
        }
    }
}

/// Migration policy governing tier transitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierMigrationPolicy {
    /// Age threshold (in microseconds) before hot→warm migration.
    pub hot_to_warm_age_us: u64,
    /// Age threshold (in microseconds) before warm→cold migration.
    pub warm_to_cold_age_us: u64,
    /// Minimum segment size in bytes to be eligible for migration.
    pub min_segment_bytes: u64,
    /// High-water mark (0.0–1.0) triggering eager demotion.
    pub pressure_threshold: f64,
    /// Maximum concurrent migrations per epoch.
    pub max_concurrent_migrations: usize,
}

impl Default for TierMigrationPolicy {
    fn default() -> Self {
        Self {
            hot_to_warm_age_us: 60_000_000,      // 60 seconds
            warm_to_cold_age_us: 600_000_000,     // 10 minutes
            min_segment_bytes: 4096,
            pressure_threshold: 0.85,
            max_concurrent_migrations: 4,
        }
    }
}

/// A contiguous segment of scrollback data tracked by the tier manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScrollbackSegment {
    pub segment_id: u64,
    pub pane_id: u64,
    pub tier: ScrollbackTier,
    pub byte_size: u64,
    pub line_count: u64,
    pub created_us: u64,
    pub last_accessed_us: u64,
    pub compressed: bool,
}

/// Migration event capturing a tier transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierMigrationEvent {
    pub segment_id: u64,
    pub from_tier: ScrollbackTier,
    pub to_tier: ScrollbackTier,
    pub bytes_migrated: u64,
    pub duration_us: u64,
    pub timestamp_us: u64,
}

/// Snapshot of the tiered scrollback manager state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TieredScrollbackSnapshot {
    pub hot_bytes: u64,
    pub warm_bytes: u64,
    pub cold_bytes: u64,
    pub hot_segments: usize,
    pub warm_segments: usize,
    pub cold_segments: usize,
    pub total_migrations: u64,
    pub total_bytes: u64,
    pub hot_utilization: f64,
    pub warm_utilization: f64,
}

/// Tiered scrollback manager: tracks segments across Hot/Warm/Cold tiers.
///
/// # Invariants
/// - Segment IDs are globally unique and monotonically increasing.
/// - `hot_bytes + warm_bytes + cold_bytes == sum(segment.byte_size)`.
/// - Tier transitions are monotonic (Hot→Warm→Cold, never reverse).
/// - Each segment belongs to exactly one tier at any time.
pub struct TieredScrollbackManager {
    hot_config: TierConfig,
    warm_config: TierConfig,
    cold_config: TierConfig,
    policy: TierMigrationPolicy,
    segments: Vec<ScrollbackSegment>,
    next_segment_id: u64,
    hot_bytes: u64,
    warm_bytes: u64,
    cold_bytes: u64,
    migration_events: Vec<TierMigrationEvent>,
    max_events: usize,
    total_migrations: u64,
}

impl TieredScrollbackManager {
    /// Create a new manager with explicit tier configs and migration policy.
    pub fn new(
        hot_config: TierConfig,
        warm_config: TierConfig,
        cold_config: TierConfig,
        policy: TierMigrationPolicy,
    ) -> Self {
        Self {
            hot_config,
            warm_config,
            cold_config,
            policy,
            segments: Vec::new(),
            next_segment_id: 0,
            hot_bytes: 0,
            warm_bytes: 0,
            cold_bytes: 0,
            migration_events: Vec::new(),
            max_events: 1024,
            total_migrations: 0,
        }
    }

    /// Create with sensible defaults (64 MiB hot, 256 MiB warm, 1 GiB cold).
    pub fn with_defaults() -> Self {
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 64 * 1024 * 1024,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 256 * 1024 * 1024,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 1024 * 1024 * 1024,
            target_latency_us: 10_000,
            compression_ratio: 0.25,
        };
        Self::new(hot, warm, cold, TierMigrationPolicy::default())
    }

    /// Ingest a new scrollback segment into the hot tier.
    /// Returns the assigned segment_id.
    pub fn ingest(&mut self, pane_id: u64, byte_size: u64, line_count: u64, now_us: u64) -> u64 {
        let segment_id = self.next_segment_id;
        self.next_segment_id += 1;
        let segment = ScrollbackSegment {
            segment_id,
            pane_id,
            tier: ScrollbackTier::Hot,
            byte_size,
            line_count,
            created_us: now_us,
            last_accessed_us: now_us,
            compressed: false,
        };
        self.segments.push(segment);
        self.hot_bytes += byte_size;
        segment_id
    }

    /// Record an access to a segment (updates last_accessed_us).
    pub fn touch(&mut self, segment_id: u64, now_us: u64) {
        if let Some(seg) = self.segments.iter_mut().find(|s| s.segment_id == segment_id) {
            seg.last_accessed_us = now_us;
        }
    }

    /// Evaluate migration policy and demote eligible segments.
    /// Returns the number of segments migrated.
    pub fn migrate(&mut self, now_us: u64) -> usize {
        let mut migrations: Vec<(usize, ScrollbackTier)> = Vec::new();
        let mut count = 0;

        for (i, seg) in self.segments.iter().enumerate() {
            if count >= self.policy.max_concurrent_migrations {
                break;
            }
            if seg.byte_size < self.policy.min_segment_bytes {
                continue;
            }
            let age = now_us.saturating_sub(seg.last_accessed_us);
            match seg.tier {
                ScrollbackTier::Hot => {
                    let pressure = if self.hot_config.max_bytes > 0 {
                        self.hot_bytes as f64 / self.hot_config.max_bytes as f64
                    } else {
                        0.0
                    };
                    if age >= self.policy.hot_to_warm_age_us
                        || pressure >= self.policy.pressure_threshold
                    {
                        migrations.push((i, ScrollbackTier::Warm));
                        count += 1;
                    }
                }
                ScrollbackTier::Warm => {
                    let pressure = if self.warm_config.max_bytes > 0 {
                        self.warm_bytes as f64 / self.warm_config.max_bytes as f64
                    } else {
                        0.0
                    };
                    if age >= self.policy.warm_to_cold_age_us
                        || pressure >= self.policy.pressure_threshold
                    {
                        migrations.push((i, ScrollbackTier::Cold));
                        count += 1;
                    }
                }
                ScrollbackTier::Cold => {}
            }
        }

        // Apply migrations
        for (idx, new_tier) in &migrations {
            let seg = &mut self.segments[*idx];
            let from_tier = seg.tier;
            let bytes = seg.byte_size;

            // Adjust tier byte counts
            match from_tier {
                ScrollbackTier::Hot => self.hot_bytes = self.hot_bytes.saturating_sub(bytes),
                ScrollbackTier::Warm => self.warm_bytes = self.warm_bytes.saturating_sub(bytes),
                ScrollbackTier::Cold => {}
            }
            match new_tier {
                ScrollbackTier::Warm => self.warm_bytes += bytes,
                ScrollbackTier::Cold => {
                    // Apply compression ratio
                    let compressed = (bytes as f64 * self.cold_config.compression_ratio) as u64;
                    seg.byte_size = compressed.max(1);
                    seg.compressed = true;
                    self.cold_bytes += seg.byte_size;
                }
                ScrollbackTier::Hot => {} // Never happens (monotonic)
            }

            let event = TierMigrationEvent {
                segment_id: seg.segment_id,
                from_tier,
                to_tier: *new_tier,
                bytes_migrated: bytes,
                duration_us: 0, // Simulated — real impl would measure
                timestamp_us: now_us,
            };

            seg.tier = *new_tier;
            self.total_migrations += 1;

            if self.migration_events.len() < self.max_events {
                self.migration_events.push(event);
            }
        }

        migrations.len()
    }

    /// Lookup a segment by ID.
    pub fn segment(&self, segment_id: u64) -> Option<&ScrollbackSegment> {
        self.segments.iter().find(|s| s.segment_id == segment_id)
    }

    /// Total bytes across all tiers.
    pub fn total_bytes(&self) -> u64 {
        self.hot_bytes + self.warm_bytes + self.cold_bytes
    }

    /// Number of segments in a given tier.
    pub fn tier_segment_count(&self, tier: ScrollbackTier) -> usize {
        self.segments.iter().filter(|s| s.tier == tier).count()
    }

    /// Hot tier utilization (0.0–1.0).
    pub fn hot_utilization(&self) -> f64 {
        if self.hot_config.max_bytes == 0 {
            return 0.0;
        }
        self.hot_bytes as f64 / self.hot_config.max_bytes as f64
    }

    /// Warm tier utilization (0.0–1.0).
    pub fn warm_utilization(&self) -> f64 {
        if self.warm_config.max_bytes == 0 {
            return 0.0;
        }
        self.warm_bytes as f64 / self.warm_config.max_bytes as f64
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> TieredScrollbackSnapshot {
        TieredScrollbackSnapshot {
            hot_bytes: self.hot_bytes,
            warm_bytes: self.warm_bytes,
            cold_bytes: self.cold_bytes,
            hot_segments: self.tier_segment_count(ScrollbackTier::Hot),
            warm_segments: self.tier_segment_count(ScrollbackTier::Warm),
            cold_segments: self.tier_segment_count(ScrollbackTier::Cold),
            total_migrations: self.total_migrations,
            total_bytes: self.total_bytes(),
            hot_utilization: self.hot_utilization(),
            warm_utilization: self.warm_utilization(),
        }
    }

    /// One-line status summary.
    pub fn status_line(&self) -> String {
        format!(
            "scrollback hot={}/{} warm={}/{} cold={} migrations={}",
            self.hot_bytes,
            self.hot_config.max_bytes,
            self.warm_bytes,
            self.warm_config.max_bytes,
            self.cold_bytes,
            self.total_migrations,
        )
    }

    /// Number of segments total.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Recent migration events.
    pub fn recent_migrations(&self) -> &[TierMigrationEvent] {
        &self.migration_events
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.segments.clear();
        self.migration_events.clear();
        self.next_segment_id = 0;
        self.hot_bytes = 0;
        self.warm_bytes = 0;
        self.cold_bytes = 0;
        self.total_migrations = 0;
    }

    /// Evict all segments for a given pane.
    pub fn evict_pane(&mut self, pane_id: u64) {
        self.segments.retain(|s| {
            if s.pane_id == pane_id {
                match s.tier {
                    ScrollbackTier::Hot => self.hot_bytes = self.hot_bytes.saturating_sub(s.byte_size),
                    ScrollbackTier::Warm => self.warm_bytes = self.warm_bytes.saturating_sub(s.byte_size),
                    ScrollbackTier::Cold => self.cold_bytes = self.cold_bytes.saturating_sub(s.byte_size),
                }
                false
            } else {
                true
            }
        });
    }

    /// Bulk ingest multiple segments. Returns assigned IDs.
    pub fn ingest_bulk(
        &mut self,
        items: &[(u64, u64, u64)], // (pane_id, byte_size, line_count)
        now_us: u64,
    ) -> Vec<u64> {
        items
            .iter()
            .map(|&(pane_id, byte_size, line_count)| self.ingest(pane_id, byte_size, line_count, now_us))
            .collect()
    }

    /// Segments for a given pane, ordered by creation time.
    pub fn segments_for_pane(&self, pane_id: u64) -> Vec<&ScrollbackSegment> {
        self.segments
            .iter()
            .filter(|s| s.pane_id == pane_id)
            .collect()
    }

    /// Tier-specific byte count.
    pub fn tier_bytes(&self, tier: ScrollbackTier) -> u64 {
        match tier {
            ScrollbackTier::Hot => self.hot_bytes,
            ScrollbackTier::Warm => self.warm_bytes,
            ScrollbackTier::Cold => self.cold_bytes,
        }
    }

    /// Total line count across all segments.
    pub fn total_lines(&self) -> u64 {
        self.segments.iter().map(|s| s.line_count).sum()
    }

    /// Evict the oldest hot-tier segments until hot utilization drops below the target ratio.
    /// Evicted segments are removed entirely (not migrated). Returns bytes freed.
    pub fn evict_hot_to_target(&mut self, target_utilization: f64) -> u64 {
        let target_bytes = (self.hot_config.max_bytes as f64 * target_utilization) as u64;
        let mut freed = 0u64;
        while self.hot_bytes > target_bytes {
            // Find the oldest hot segment by created_us
            let oldest_idx = self
                .segments
                .iter()
                .enumerate()
                .filter(|(_, s)| s.tier == ScrollbackTier::Hot)
                .min_by_key(|(_, s)| s.created_us)
                .map(|(i, _)| i);
            match oldest_idx {
                Some(idx) => {
                    let removed = self.segments.remove(idx);
                    self.hot_bytes = self.hot_bytes.saturating_sub(removed.byte_size);
                    freed += removed.byte_size;
                }
                None => break,
            }
        }
        freed
    }

    /// Oldest segment in the hot tier, if any.
    pub fn oldest_hot_segment(&self) -> Option<&ScrollbackSegment> {
        self.segments
            .iter()
            .filter(|s| s.tier == ScrollbackTier::Hot)
            .min_by_key(|s| s.created_us)
    }

    /// Age of the oldest hot segment in microseconds, or 0 if none.
    pub fn oldest_hot_age_us(&self, now_us: u64) -> u64 {
        self.oldest_hot_segment()
            .map(|s| now_us.saturating_sub(s.last_accessed_us))
            .unwrap_or(0)
    }

    /// Distinct pane IDs with data in the manager.
    pub fn active_pane_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.segments.iter().map(|s| s.pane_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Cold tier utilization (0.0–1.0).
    pub fn cold_utilization(&self) -> f64 {
        if self.cold_config.max_bytes == 0 {
            return 0.0;
        }
        self.cold_bytes as f64 / self.cold_config.max_bytes as f64
    }
}

/// Degradation states for the tiered scrollback system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScrollbackDegradation {
    Healthy,
    HotPressure { utilization: f64, threshold: f64 },
    WarmPressure { utilization: f64, threshold: f64 },
    MigrationBacklog { pending: usize, max_concurrent: usize },
}

impl std::fmt::Display for ScrollbackDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScrollbackDegradation::Healthy => write!(f, "HEALTHY"),
            ScrollbackDegradation::HotPressure { utilization, threshold } => {
                write!(f, "HOT_PRESSURE({:.1}%/{:.1}%)", utilization * 100.0, threshold * 100.0)
            }
            ScrollbackDegradation::WarmPressure { utilization, threshold } => {
                write!(f, "WARM_PRESSURE({:.1}%/{:.1}%)", utilization * 100.0, threshold * 100.0)
            }
            ScrollbackDegradation::MigrationBacklog { pending, max_concurrent } => {
                write!(f, "MIGRATION_BACKLOG({}/{})", pending, max_concurrent)
            }
        }
    }
}

/// Structured log entry for tiered scrollback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScrollbackLogEntry {
    pub hot_bytes: u64,
    pub warm_bytes: u64,
    pub cold_bytes: u64,
    pub total_segments: usize,
    pub total_migrations: u64,
    pub degradation: ScrollbackDegradation,
}

impl TieredScrollbackManager {
    /// Detect degradation state.
    pub fn detect_degradation(&self) -> ScrollbackDegradation {
        let hot_util = self.hot_utilization();
        if hot_util >= self.policy.pressure_threshold {
            return ScrollbackDegradation::HotPressure {
                utilization: hot_util,
                threshold: self.policy.pressure_threshold,
            };
        }
        let warm_util = self.warm_utilization();
        if warm_util >= self.policy.pressure_threshold {
            return ScrollbackDegradation::WarmPressure {
                utilization: warm_util,
                threshold: self.policy.pressure_threshold,
            };
        }
        // Check if hot tier has many segments ready to migrate
        let pending = self.segments.iter().filter(|s| {
            s.tier == ScrollbackTier::Hot && s.byte_size >= self.policy.min_segment_bytes
        }).count();
        if pending > self.policy.max_concurrent_migrations * 2 {
            return ScrollbackDegradation::MigrationBacklog {
                pending,
                max_concurrent: self.policy.max_concurrent_migrations,
            };
        }
        ScrollbackDegradation::Healthy
    }

    /// Create a structured log entry.
    pub fn log_entry(&self) -> ScrollbackLogEntry {
        ScrollbackLogEntry {
            hot_bytes: self.hot_bytes,
            warm_bytes: self.warm_bytes,
            cold_bytes: self.cold_bytes,
            total_segments: self.segments.len(),
            total_migrations: self.total_migrations,
            degradation: self.detect_degradation(),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Stage Definitions ──

    #[test]
    fn test_pipeline_stages_complete() {
        assert_eq!(LatencyStage::PIPELINE_STAGES.len(), 8);
        assert!(!LatencyStage::PIPELINE_STAGES
            .iter()
            .any(|s| s.is_aggregate()));
    }

    #[test]
    fn test_capture_path_subset_of_pipeline() {
        for stage in LatencyStage::CAPTURE_PATH {
            assert!(
                LatencyStage::PIPELINE_STAGES.contains(stage),
                "capture path stage {stage} not in pipeline"
            );
        }
    }

    #[test]
    fn test_action_path_subset_of_pipeline() {
        for stage in LatencyStage::ACTION_PATH {
            assert!(
                LatencyStage::PIPELINE_STAGES.contains(stage),
                "action path stage {stage} not in pipeline"
            );
        }
    }

    #[test]
    fn test_aggregate_stages_identified() {
        assert!(LatencyStage::EndToEndCapture.is_aggregate());
        assert!(LatencyStage::EndToEndAction.is_aggregate());
        assert!(!LatencyStage::PtyCapture.is_aggregate());
    }

    #[test]
    fn test_reason_prefix_unique() {
        let mut prefixes = std::collections::HashSet::new();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                prefixes.insert(stage.reason_prefix()),
                "duplicate prefix: {}",
                stage.reason_prefix()
            );
        }
    }

    #[test]
    fn test_stage_display_matches_prefix() {
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert_eq!(format!("{stage}"), stage.reason_prefix());
        }
    }

    // ── Percentile ──

    #[test]
    fn test_percentile_values_ordered() {
        let values: Vec<f64> = Percentile::ALL.iter().map(|p| p.value()).collect();
        for window in values.windows(2) {
            assert!(window[0] < window[1], "percentiles not strictly increasing");
        }
    }

    #[test]
    fn test_percentile_display() {
        assert_eq!(format!("{}", Percentile::P50), "p50");
        assert_eq!(format!("{}", Percentile::P999), "p999");
    }

    // ── StageBudget ──

    #[test]
    fn test_budget_construction_valid() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0);
        assert!(b.is_ok());
        let b = b.unwrap();
        assert_eq!(b.target(Percentile::P50), 100.0);
        assert_eq!(b.target(Percentile::P999), 400.0);
    }

    #[test]
    fn test_budget_rejects_negative() {
        let b = StageBudget::new(LatencyStage::PtyCapture, -1.0, 200.0, 300.0, 400.0);
        assert!(matches!(b, Err(BudgetError::NegativeTarget { .. })));
    }

    #[test]
    fn test_budget_rejects_nonmonotonic() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 200.0, 100.0, 300.0, 400.0);
        assert!(matches!(b, Err(BudgetError::NonMonotonic { .. })));
    }

    #[test]
    fn test_budget_equal_percentiles_allowed() {
        // Equal values at consecutive percentiles is valid (≤ not <).
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 100.0, 100.0, 100.0);
        assert!(b.is_ok());
    }

    #[test]
    fn test_budget_exceeds() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        assert!(!b.exceeds(Percentile::P50, 99.0));
        assert!(b.exceeds(Percentile::P50, 101.0));
        assert!(!b.exceeds(Percentile::P50, 100.0)); // equal is not exceeded
    }

    #[test]
    fn test_budget_violation_reason() {
        let b = StageBudget::new(LatencyStage::StorageWrite, 100.0, 200.0, 300.0, 400.0).unwrap();
        let reason = b.violation_reason(Percentile::P99);
        assert!(matches!(
            reason,
            ReasonCode::BudgetExceeded {
                stage: LatencyStage::StorageWrite,
                percentile: Percentile::P99,
            }
        ));
    }

    // ── Default Budgets ──

    #[test]
    fn test_default_budgets_cover_all_stages() {
        let budgets = default_budgets();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                budgets.iter().any(|b| b.stage == stage),
                "missing budget for {stage}"
            );
        }
        // Aggregates also have budgets.
        assert!(budgets
            .iter()
            .any(|b| b.stage == LatencyStage::EndToEndCapture));
        assert!(budgets
            .iter()
            .any(|b| b.stage == LatencyStage::EndToEndAction));
    }

    #[test]
    fn test_default_budgets_monotonic() {
        for budget in default_budgets() {
            assert!(
                budget.p50_us <= budget.p95_us,
                "{}: p50 > p95",
                budget.stage
            );
            assert!(
                budget.p95_us <= budget.p99_us,
                "{}: p95 > p99",
                budget.stage
            );
            assert!(
                budget.p99_us <= budget.p999_us,
                "{}: p99 > p999",
                budget.stage
            );
        }
    }

    #[test]
    fn test_default_budgets_nonnegative() {
        for budget in default_budgets() {
            assert!(budget.p50_us >= 0.0, "{}: negative p50", budget.stage);
            assert!(budget.p95_us >= 0.0, "{}: negative p95", budget.stage);
            assert!(budget.p99_us >= 0.0, "{}: negative p99", budget.stage);
            assert!(budget.p999_us >= 0.0, "{}: negative p999", budget.stage);
        }
    }

    // ── Budget Algebra ──

    #[test]
    fn test_leaf_aggregate() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let node = BudgetNode::Leaf(b);
        assert_eq!(node.aggregate(Percentile::P50), 100.0);
        assert_eq!(node.aggregate(Percentile::P999), 400.0);
    }

    #[test]
    fn test_sequential_composition_additive() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let b = StageBudget::new(LatencyStage::DeltaExtraction, 50.0, 100.0, 150.0, 200.0).unwrap();
        let seq = BudgetNode::Seq(vec![BudgetNode::Leaf(a), BudgetNode::Leaf(b)]);
        assert_eq!(seq.aggregate(Percentile::P50), 150.0);
        assert_eq!(seq.aggregate(Percentile::P999), 600.0);
    }

    #[test]
    fn test_parallel_composition_max() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let b =
            StageBudget::new(LatencyStage::DeltaExtraction, 150.0, 180.0, 250.0, 500.0).unwrap();
        let par = BudgetNode::Par(vec![BudgetNode::Leaf(a), BudgetNode::Leaf(b)]);
        assert_eq!(par.aggregate(Percentile::P50), 150.0); // max(100, 150)
        assert_eq!(par.aggregate(Percentile::P95), 200.0); // max(200, 180)
        assert_eq!(par.aggregate(Percentile::P999), 500.0); // max(400, 500)
    }

    #[test]
    fn test_conditional_composition_weighted() {
        let then_b = StageBudget::new(
            LatencyStage::WorkflowDispatch,
            1000.0,
            2000.0,
            3000.0,
            5000.0,
        )
        .unwrap();
        let cond = BudgetNode::Cond {
            probability: 0.5,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: None,
        };
        assert_eq!(cond.aggregate(Percentile::P50), 500.0); // 0.5 * 1000 + 0.5 * 0
        assert_eq!(cond.aggregate(Percentile::P999), 2500.0);
    }

    #[test]
    fn test_conditional_with_else_branch() {
        let then_b = StageBudget::new(
            LatencyStage::WorkflowDispatch,
            1000.0,
            2000.0,
            3000.0,
            5000.0,
        )
        .unwrap();
        let else_b =
            StageBudget::new(LatencyStage::ApiResponse, 200.0, 400.0, 600.0, 1000.0).unwrap();
        let cond = BudgetNode::Cond {
            probability: 0.3,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: Some(Box::new(BudgetNode::Leaf(else_b))),
        };
        // 0.3 * 1000 + 0.7 * 200 = 300 + 140 = 440
        let result = cond.aggregate(Percentile::P50);
        assert!((result - 440.0).abs() < 0.01);
    }

    #[test]
    fn test_slack_positive_means_headroom() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let node = BudgetNode::Leaf(a);
        let slack = node.slack(Percentile::P50, 200.0);
        assert_eq!(slack, 100.0); // 200 - 100 = 100μs headroom
    }

    #[test]
    fn test_slack_negative_means_over_budget() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let node = BudgetNode::Leaf(a);
        let slack = node.slack(Percentile::P50, 50.0);
        assert_eq!(slack, -50.0); // 50 - 100 = -50μs over budget
    }

    #[test]
    fn test_leaves_collects_all() {
        let tree = default_pipeline_tree();
        let leaves = tree.leaves();
        // All 8 pipeline stages should appear as leaves.
        assert_eq!(leaves.len(), 8);
    }

    // ── Default Pipeline Tree ──

    #[test]
    fn test_default_pipeline_tree_structure() {
        let tree = default_pipeline_tree();
        let leaves = tree.leaves();
        let stages: Vec<LatencyStage> = leaves.iter().map(|b| b.stage).collect();
        assert_eq!(stages[0], LatencyStage::PtyCapture);
        assert_eq!(stages[1], LatencyStage::DeltaExtraction);
        assert_eq!(stages[2], LatencyStage::StorageWrite);
        assert_eq!(stages[3], LatencyStage::PatternDetection);
        assert_eq!(stages[4], LatencyStage::EventEmission);
        assert_eq!(stages[5], LatencyStage::WorkflowDispatch);
        assert_eq!(stages[6], LatencyStage::ActionExecution);
        assert_eq!(stages[7], LatencyStage::ApiResponse);
    }

    #[test]
    fn test_default_pipeline_aggregate_within_e2e_capture_budget() {
        let _tree = default_pipeline_tree();
        let budgets = default_budgets();
        let e2e_capture = budgets
            .iter()
            .find(|b| b.stage == LatencyStage::EndToEndCapture)
            .unwrap();

        // The capture path aggregate should fit within the E2E capture budget.
        // Note: full tree includes conditional workflow path, so we check capture path only.
        let capture_stages: Vec<BudgetNode> = LatencyStage::CAPTURE_PATH
            .iter()
            .map(|&s| BudgetNode::Leaf(*budgets.iter().find(|b| b.stage == s).unwrap()))
            .collect();
        let capture_tree = BudgetNode::Seq(capture_stages);

        for &p in Percentile::ALL {
            let agg = capture_tree.aggregate(p);
            let ceiling = e2e_capture.target(p);
            assert!(
                agg <= ceiling,
                "capture path {p} aggregate {agg:.0}μs > E2E ceiling {ceiling:.0}μs"
            );
        }
    }

    // ── Reason Codes ──

    #[test]
    fn test_reason_code_display_budget_exceeded() {
        let rc = ReasonCode::BudgetExceeded {
            stage: LatencyStage::StorageWrite,
            percentile: Percentile::P99,
        };
        assert_eq!(format!("{rc}"), "BUDGET_EXCEEDED_STORAGE_WRITE_p99");
    }

    #[test]
    fn test_reason_code_display_slack_exhausted() {
        assert_eq!(format!("{}", ReasonCode::SlackExhausted), "SLACK_EXHAUSTED");
    }

    #[test]
    fn test_reason_code_display_overflow_isolated() {
        let rc = ReasonCode::OverflowIsolated {
            stage: LatencyStage::PatternDetection,
        };
        assert_eq!(format!("{rc}"), "OVERFLOW_ISOLATED_PATTERN_DETECT");
    }

    #[test]
    fn test_reason_code_display_cascade_prevented() {
        let rc = ReasonCode::CascadePrevented {
            stage: LatencyStage::ActionExecution,
            mitigation: Mitigation::Shed,
        };
        assert_eq!(format!("{rc}"), "CASCADE_PREVENTED_ACTION_EXEC_SHED");
    }

    #[test]
    fn test_reason_code_display_redistributed() {
        let rc = ReasonCode::SlackRedistributed {
            donor: LatencyStage::DeltaExtraction,
            recipient: LatencyStage::StorageWrite,
            amount_us: 500,
        };
        assert_eq!(
            format!("{rc}"),
            "SLACK_REDISTRIBUTED_DELTA_EXTRACT_TO_STORAGE_WRITE"
        );
    }

    // ── Mitigation ──

    #[test]
    fn test_mitigation_display() {
        assert_eq!(format!("{}", Mitigation::Skip), "SKIP");
        assert_eq!(format!("{}", Mitigation::Degrade), "DEGRADE");
        assert_eq!(format!("{}", Mitigation::Shed), "SHED");
        assert_eq!(format!("{}", Mitigation::Defer), "DEFER");
        assert_eq!(format!("{}", Mitigation::None), "NONE");
    }

    // ── Pipeline Run Validation ──

    #[test]
    fn test_pipeline_run_valid() {
        let run = make_valid_run();
        assert!(run.validate().is_ok());
    }

    #[test]
    fn test_pipeline_run_detects_stage_misordering() {
        let mut run = make_valid_run();
        // Swap two stages.
        run.stages.swap(0, 1);
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::StageOrdering { .. })));
    }

    #[test]
    fn test_pipeline_run_detects_timestamp_regression() {
        let mut run = make_valid_run();
        // Make second stage start before first ends.
        run.stages[1].start_epoch_us = run.stages[0].start_epoch_us;
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::TimestampRegression { .. })));
    }

    #[test]
    fn test_pipeline_run_detects_total_mismatch() {
        let mut run = make_valid_run();
        run.total_latency_us = 999_999.0; // way off
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::TotalMismatch { .. })));
    }

    #[test]
    fn test_pipeline_run_detects_overflow_mismatch() {
        let mut run = make_valid_run();
        run.has_overflow = true; // no stage actually overflowed
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::OverflowFlagMismatch { .. })));
    }

    // ── Workload Classes ──

    #[test]
    fn test_workload_classes_complete() {
        assert_eq!(WorkloadClass::ALL.len(), 8);
    }

    #[test]
    fn test_adversarial_workloads() {
        assert!(!WorkloadClass::LightSingle.is_adversarial());
        assert!(!WorkloadClass::HeavySingle.is_adversarial());
        assert!(WorkloadClass::BurstySwarm.is_adversarial());
        assert!(WorkloadClass::StorageDegraded.is_adversarial());
    }

    #[test]
    fn test_workload_primary_percentile_ordering() {
        // Adversarial workloads should target higher percentiles.
        let nominal_p = WorkloadClass::LightSingle.primary_percentile();
        let stress_p = WorkloadClass::BurstySwarm.primary_percentile();
        assert!(nominal_p < stress_p);
    }

    // ── Benchmark Contract ──

    #[test]
    fn test_benchmark_contract_covers_all_stages() {
        let contract = BenchmarkContract::default_contract();
        for &stage in LatencyStage::PIPELINE_STAGES {
            let has_criteria = contract.criteria.iter().any(|c| c.stage == stage);
            assert!(has_criteria, "no benchmark criteria for {stage}");
        }
    }

    #[test]
    fn test_benchmark_contract_covers_all_workloads() {
        let contract = BenchmarkContract::default_contract();
        for &workload in WorkloadClass::ALL {
            let has_criteria = contract.criteria.iter().any(|c| c.workload == workload);
            assert!(has_criteria, "no benchmark criteria for {workload}");
        }
    }

    #[test]
    fn test_benchmark_contract_overhead_limits() {
        let contract = BenchmarkContract::default_contract();
        for c in &contract.criteria {
            if c.workload.is_adversarial() {
                assert_eq!(c.max_overhead_fraction, 0.10);
            } else {
                assert_eq!(c.max_overhead_fraction, 0.05);
            }
        }
    }

    // ── Verification Matrix ──

    #[test]
    fn test_verification_matrix_covers_all_categories() {
        let matrix = verification_matrix();
        let categories: std::collections::HashSet<_> = matrix.iter().map(|e| e.category).collect();
        assert!(categories.contains(&TestCategory::Unit));
        assert!(categories.contains(&TestCategory::Property));
        assert!(categories.contains(&TestCategory::Integration));
        assert!(categories.contains(&TestCategory::EndToEnd));
        assert!(categories.contains(&TestCategory::Chaos));
        assert!(categories.contains(&TestCategory::Soak));
    }

    #[test]
    fn test_verification_matrix_all_named() {
        let matrix = verification_matrix();
        for entry in &matrix {
            assert!(!entry.name.is_empty(), "verification entry has empty name");
            assert!(
                !entry.invariants.is_empty(),
                "verification entry {} has no invariants",
                entry.name
            );
        }
    }

    // ── Serde Roundtrip ──

    #[test]
    fn test_stage_budget_serde_roundtrip() {
        let budget =
            StageBudget::new(LatencyStage::PatternDetection, 100.0, 200.0, 300.0, 400.0).unwrap();
        let json = serde_json::to_string(&budget).unwrap();
        let back: StageBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(budget, back);
    }

    #[test]
    fn test_reason_code_serde_roundtrip() {
        let rc = ReasonCode::BudgetExceeded {
            stage: LatencyStage::EventEmission,
            percentile: Percentile::P95,
        };
        let json = serde_json::to_string(&rc).unwrap();
        let back: ReasonCode = serde_json::from_str(&json).unwrap();
        assert_eq!(rc, back);
    }

    #[test]
    fn test_pipeline_run_serde_roundtrip() {
        let run = make_valid_run();
        let json = serde_json::to_string(&run).unwrap();
        let back: PipelineRun = serde_json::from_str(&json).unwrap();
        assert_eq!(run, back);
    }

    #[test]
    fn test_log_entry_serde_roundtrip() {
        let entry = LatencyLogEntry {
            timestamp: "2026-02-23T19:00:00.000000Z".into(),
            subsystem: "latency.pty_capture".into(),
            correlation_id: "run-001".into(),
            scenario_id: Some("test-nominal".into()),
            inputs: serde_json::json!({"pane_id": 0, "content_len": 1024}),
            decision: "delta_extracted".into(),
            outcome: serde_json::json!({"latency_us": 450.0, "overflow": false}),
            reason_code: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: LatencyLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    // ── Helper ──

    // ── BudgetEnforcer Tests ──

    #[test]
    fn test_enforcer_creation_default() {
        let enforcer = BudgetEnforcer::with_defaults();
        assert_eq!(enforcer.total_observations(), 0);
        assert_eq!(enforcer.total_overflows(), 0);
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(enforcer.has_stage(stage), "missing stage {stage}");
        }
    }

    #[test]
    fn test_enforcer_record_within_budget() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        let result = enforcer.record(LatencyStage::DeltaExtraction, 100.0, "test-001");
        assert!(!result.overflow);
        assert_eq!(result.recommended_mitigation, Mitigation::None);
        assert_eq!(enforcer.total_observations(), 1);
        assert_eq!(enforcer.total_overflows(), 0);
    }

    #[test]
    fn test_enforcer_record_exceeds_p999() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // DeltaExtraction p999 budget is 5000μs. Send 10000μs.
        let result = enforcer.record(LatencyStage::DeltaExtraction, 10_000.0, "test-002");
        assert!(result.overflow);
        assert_eq!(result.violated_percentile, Some(Percentile::P999));
        assert!(result.reason.is_some());
        assert_ne!(result.recommended_mitigation, Mitigation::None);
        assert_eq!(enforcer.total_overflows(), 1);
    }

    #[test]
    fn test_enforcer_record_exceeds_p99_not_p999() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // DeltaExtraction p99=1000, p999=5000. Send 2000μs.
        let result = enforcer.record(LatencyStage::DeltaExtraction, 2_000.0, "test-003");
        assert!(result.overflow);
        assert_eq!(result.violated_percentile, Some(Percentile::P99));
    }

    #[test]
    fn test_enforcer_percentile_estimation() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Add 100 observations for PtyCapture.
        for i in 0..100 {
            enforcer.record(LatencyStage::PtyCapture, (i + 1) as f64 * 10.0, "test");
        }
        let snap = enforcer.snapshot();
        let pty_snap = snap
            .stages
            .iter()
            .find(|s| s.stage == LatencyStage::PtyCapture)
            .unwrap();
        assert_eq!(pty_snap.percentiles.sample_count, 100);
        assert_eq!(pty_snap.percentiles.total_observations, 100);
        // p50 should be around 500μs (50th value in 10,20,...,1000)
        let p50 = pty_snap.percentiles.p50_us.unwrap();
        assert!(p50 > 400.0 && p50 < 600.0, "p50 = {p50}");
    }

    #[test]
    fn test_enforcer_window_wraps() {
        let config = BudgetEnforcerConfig {
            window_size: 10,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        // Add 25 observations — wraps around.
        for i in 0..25 {
            enforcer.record(LatencyStage::PtyCapture, (i + 1) as f64, "test");
        }
        let snap = enforcer.snapshot();
        let pty_snap = snap
            .stages
            .iter()
            .find(|s| s.stage == LatencyStage::PtyCapture)
            .unwrap();
        assert_eq!(pty_snap.percentiles.sample_count, 10);
        assert_eq!(pty_snap.percentiles.total_observations, 25);
    }

    #[test]
    fn test_enforcer_snapshot_slack() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Record normal values for all stages.
        for &stage in LatencyStage::PIPELINE_STAGES {
            enforcer.record(stage, 10.0, "test");
        }
        let snap = enforcer.snapshot();
        // Slack should be positive for all percentiles (10μs is well under budget).
        for (pctl, slack) in &snap.slack {
            assert!(*slack > 0.0, "negative slack at {pctl}: {slack}");
        }
    }

    #[test]
    fn test_enforcer_log_overflows_only() {
        let config = BudgetEnforcerConfig {
            log_overflows_only: true,
            log_all_observations: false,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        enforcer.record(LatencyStage::DeltaExtraction, 100.0, "test"); // within budget
        assert_eq!(enforcer.log_count(), 0);
        enforcer.record(LatencyStage::DeltaExtraction, 100_000.0, "test"); // overflow
        assert_eq!(enforcer.log_count(), 1);
    }

    #[test]
    fn test_enforcer_log_all() {
        let config = BudgetEnforcerConfig {
            log_overflows_only: false,
            log_all_observations: true,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        enforcer.record(LatencyStage::DeltaExtraction, 100.0, "test");
        enforcer.record(LatencyStage::DeltaExtraction, 200.0, "test");
        assert_eq!(enforcer.log_count(), 2);
    }

    #[test]
    fn test_enforcer_drain_logs() {
        let config = BudgetEnforcerConfig {
            log_all_observations: true,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        enforcer.record(LatencyStage::PtyCapture, 100.0, "test");
        let logs = enforcer.drain_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(enforcer.log_count(), 0);
    }

    #[test]
    fn test_enforcer_mitigation_for_stage() {
        let enforcer = BudgetEnforcer::with_defaults();
        // PatternDetection: p99=Degrade, p999=Skip
        assert_eq!(
            enforcer.mitigation_for(LatencyStage::PatternDetection, Percentile::P99),
            Mitigation::Degrade
        );
        assert_eq!(
            enforcer.mitigation_for(LatencyStage::PatternDetection, Percentile::P999),
            Mitigation::Skip
        );
        assert_eq!(
            enforcer.mitigation_for(LatencyStage::PatternDetection, Percentile::P50),
            Mitigation::None
        );
    }

    #[test]
    fn test_enforcer_unknown_stage() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Aggregate stages have no state — should return benign result.
        let result = enforcer.record(LatencyStage::EndToEndCapture, 100.0, "test");
        assert!(!result.overflow);
        assert_eq!(result.current_percentiles.sample_count, 0);
    }

    #[test]
    fn test_enforcer_build_run() {
        let enforcer = BudgetEnforcer::with_defaults();
        let obs = vec![StageObservation {
            stage: LatencyStage::PtyCapture,
            latency_us: 5000.0,
            correlation_id: "run-001".into(),
            scenario_id: None,
            start_epoch_us: 1000,
            end_epoch_us: 6000,
            overflow: false,
            reason: None,
            mitigation: Mitigation::None,
        }];
        let run = enforcer.build_run("run-001", "corr-001", obs);
        assert_eq!(run.run_id, "run-001");
        assert_eq!(run.total_latency_us, 5000.0);
        assert!(!run.has_overflow);
    }

    #[test]
    fn test_enforcer_multiple_stages_tracking() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        let stages = [
            LatencyStage::PtyCapture,
            LatencyStage::DeltaExtraction,
            LatencyStage::StorageWrite,
        ];
        for &stage in &stages {
            for i in 1..=10 {
                enforcer.record(stage, i as f64 * 100.0, "test");
            }
        }
        assert_eq!(enforcer.total_observations(), 30);
        let snap = enforcer.snapshot();
        assert_eq!(snap.stages.len(), 8); // all pipeline stages tracked
        for s in &snap.stages {
            if stages.contains(&s.stage) {
                assert_eq!(s.percentiles.total_observations, 10);
            }
        }
    }

    #[test]
    fn test_enforcer_snapshot_serde_roundtrip() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        for &stage in LatencyStage::PIPELINE_STAGES {
            enforcer.record(stage, 100.0, "test");
        }
        let snap = enforcer.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: EnforcerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.total_observations, back.total_observations);
        assert_eq!(snap.stages.len(), back.stages.len());
    }

    #[test]
    fn test_default_mitigation_policies_cover_all_stages() {
        let policies = default_mitigation_policies();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                policies.iter().any(|p| p.stage == stage),
                "missing mitigation policy for {stage}"
            );
        }
    }

    #[test]
    fn test_latency_window_empty() {
        let window = LatencyWindow::new(10);
        assert!(window.percentile(0.5).is_none());
        assert!(window.mean().is_none());
        assert_eq!(window.len(), 0);
        assert_eq!(window.total_count(), 0);
    }

    #[test]
    fn test_latency_window_single() {
        let mut window = LatencyWindow::new(10);
        window.push(42.0);
        assert_eq!(window.percentile(0.5), Some(42.0));
        assert_eq!(window.mean(), Some(42.0));
        assert_eq!(window.len(), 1);
    }

    #[test]
    fn test_latency_window_mean() {
        let mut window = LatencyWindow::new(100);
        for i in 1..=10 {
            window.push(i as f64);
        }
        let mean = window.mean().unwrap();
        assert!((mean - 5.5).abs() < 0.01);
    }

    // ── CorrelationContext ──

    #[test]
    fn test_correlation_context_new() {
        let ctx = CorrelationContext::new("run-001", 1_000_000);
        assert_eq!(ctx.run_id, "run-001");
        assert_eq!(ctx.correlation_id, "run-001");
        assert!(ctx.propagation_intact);
        assert_eq!(ctx.next_expected, Some(LatencyStage::PtyCapture));
        assert!(ctx.timings.is_empty());
        assert_eq!(ctx.created_at_us, 1_000_000);
    }

    #[test]
    fn test_correlation_context_with_correlation() {
        let ctx = CorrelationContext::with_correlation("run-001", "corr-abc", 500);
        assert_eq!(ctx.run_id, "run-001");
        assert_eq!(ctx.correlation_id, "corr-abc");
    }

    #[test]
    fn test_correlation_context_begin_end_stage() {
        let mut ctx = CorrelationContext::new("run-001", 1000);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        assert_eq!(probe.stage, LatencyStage::PtyCapture);
        assert_eq!(probe.start_us, 1000);
        assert_eq!(probe.correlation_id, "run-001");
        ctx.end_stage(probe, 1500);
        assert_eq!(ctx.timings.len(), 1);
        assert_eq!(ctx.timings[0].latency_us, 500.0);
        assert_eq!(ctx.next_expected, Some(LatencyStage::DeltaExtraction));
        assert!(ctx.propagation_intact);
    }

    #[test]
    fn test_correlation_context_full_pipeline() {
        let mut ctx = CorrelationContext::new("run-full", 0);
        let mut t = 1000_u64;
        for &stage in LatencyStage::PIPELINE_STAGES {
            let probe = ctx.begin_stage(stage, t);
            t += 100;
            ctx.end_stage(probe, t);
            t += 10; // gap
        }
        assert_eq!(ctx.stage_count(), 8);
        assert!(ctx.propagation_intact);
        assert!(ctx.missing_stages().is_empty());
        // next_expected should be None after last stage
        assert_eq!(ctx.next_expected, None);
    }

    #[test]
    fn test_correlation_context_gap_detection() {
        let mut ctx = CorrelationContext::new("run-gap", 0);
        // Skip PtyCapture, start with DeltaExtraction
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 1000);
        ctx.end_stage(probe, 1500);
        assert!(!ctx.propagation_intact);
        assert_eq!(ctx.missing_stages().len(), 7); // all except DeltaExtraction
    }

    #[test]
    fn test_correlation_context_clock_regression() {
        let mut ctx = CorrelationContext::new("run-clock", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 2000);
        // End before start — should clamp to 0
        ctx.end_stage(probe, 1000);
        assert_eq!(ctx.timings[0].latency_us, 0.0);
    }

    #[test]
    fn test_correlation_context_total_elapsed() {
        let mut ctx = CorrelationContext::new("run-elapsed", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 1600);
        ctx.end_stage(probe, 2000);
        assert_eq!(ctx.total_elapsed_us(), 1000); // 2000 - 1000
    }

    #[test]
    fn test_correlation_context_total_elapsed_empty() {
        let ctx = CorrelationContext::new("run-empty", 0);
        assert_eq!(ctx.total_elapsed_us(), 0);
    }

    #[test]
    fn test_correlation_context_to_pipeline_run() {
        let mut ctx = CorrelationContext::new("run-convert", 0);
        ctx.scenario_id = Some("test-scenario".into());
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 1600);
        ctx.end_stage(probe, 2100);

        let run = ctx.to_pipeline_run();
        assert_eq!(run.run_id, "run-convert");
        assert_eq!(run.correlation_id, "run-convert");
        assert_eq!(run.scenario_id, Some("test-scenario".into()));
        assert_eq!(run.stages.len(), 2);
        assert!((run.total_latency_us - 1000.0).abs() < 0.01); // 500 + 500
        assert!(!run.has_overflow);
    }

    #[test]
    fn test_correlation_context_serde_roundtrip() {
        let mut ctx = CorrelationContext::new("run-serde", 1000);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let json = serde_json::to_string(&ctx).unwrap();
        let back: CorrelationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, back);
    }

    // ── StageProbe ──

    #[test]
    fn test_stage_probe_serde_roundtrip() {
        let probe = StageProbe {
            stage: LatencyStage::StorageWrite,
            start_us: 12345,
            correlation_id: "corr-001".into(),
        };
        let json = serde_json::to_string(&probe).unwrap();
        let back: StageProbe = serde_json::from_str(&json).unwrap();
        assert_eq!(probe, back);
    }

    // ── StageTiming ──

    #[test]
    fn test_stage_timing_serde_roundtrip() {
        let timing = StageTiming {
            stage: LatencyStage::PatternDetection,
            start_us: 100,
            end_us: 500,
            latency_us: 400.0,
        };
        let json = serde_json::to_string(&timing).unwrap();
        let back: StageTiming = serde_json::from_str(&json).unwrap();
        assert_eq!(timing, back);
    }

    // ── InstrumentationOverhead ──

    #[test]
    fn test_overhead_new_defaults() {
        let oh = InstrumentationOverhead::new();
        assert_eq!(oh.probe_count, 0);
        assert_eq!(oh.total_overhead_us, 0.0);
        assert_eq!(oh.budget_per_probe_us, 1.0);
        assert!(oh.within_budget);
    }

    #[test]
    fn test_overhead_default_matches_new() {
        let a = InstrumentationOverhead::new();
        let b = InstrumentationOverhead::default();
        assert_eq!(a, b);
    }

    #[test]
    fn test_overhead_record_within_budget() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.5);
        oh.record(0.3);
        oh.record(0.8);
        assert_eq!(oh.probe_count, 3);
        assert!((oh.total_overhead_us - 1.6).abs() < 1e-10);
        assert!((oh.mean_overhead_us - 1.6 / 3.0).abs() < 1e-10);
        assert!((oh.max_overhead_us - 0.8).abs() < 1e-10);
        assert!(oh.within_budget);
    }

    #[test]
    fn test_overhead_record_exceeds_budget() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.5);
        oh.record(1.5); // exceeds 1μs budget
        assert!(!oh.within_budget);
        assert!((oh.max_overhead_us - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_overhead_fraction() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.5);
        oh.record(0.5);
        // total_overhead = 1.0μs, pipeline = 1000μs → 0.001 = 0.1%
        let frac = oh.overhead_fraction(1000.0);
        assert!((frac - 0.001).abs() < 1e-10);
    }

    #[test]
    fn test_overhead_fraction_zero_pipeline() {
        let oh = InstrumentationOverhead::new();
        assert_eq!(oh.overhead_fraction(0.0), 0.0);
        assert_eq!(oh.overhead_fraction(-1.0), 0.0);
    }

    #[test]
    fn test_overhead_serde_roundtrip() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.3);
        oh.record(0.7);
        let json = serde_json::to_string(&oh).unwrap();
        let back: InstrumentationOverhead = serde_json::from_str(&json).unwrap();
        assert_eq!(oh, back);
    }

    // ── InstrumentedEnforcer ──

    #[test]
    fn test_instrumented_enforcer_new() {
        let ie = InstrumentedEnforcer::new();
        assert_eq!(ie.completed_runs(), 0);
        assert_eq!(ie.overflow_runs(), 0);
        assert_eq!(ie.overflow_rate(), 0.0);
    }

    #[test]
    fn test_instrumented_enforcer_default_matches_new() {
        let a = InstrumentedEnforcer::new();
        let b = InstrumentedEnforcer::default();
        assert_eq!(a.completed_runs(), b.completed_runs());
        assert_eq!(a.overflow_runs(), b.overflow_runs());
    }

    #[test]
    fn test_instrumented_enforcer_process_nominal_run() {
        let mut ie = InstrumentedEnforcer::new();
        let mut ctx = CorrelationContext::new("run-nominal", 0);
        let mut t = 1000_u64;
        for &stage in LatencyStage::PIPELINE_STAGES {
            let probe = ctx.begin_stage(stage, t);
            t += 50; // 50μs per stage — well within budget
            ctx.end_stage(probe, t);
            t += 10;
        }
        let results = ie.process_run(&ctx);
        assert_eq!(results.len(), 8);
        assert!(results.iter().all(|r| !r.overflow));
        assert_eq!(ie.completed_runs(), 1);
        assert_eq!(ie.overflow_runs(), 0);
        assert_eq!(ie.overflow_rate(), 0.0);
    }

    #[test]
    fn test_instrumented_enforcer_process_overflow_run() {
        let mut ie = InstrumentedEnforcer::new();
        let mut ctx = CorrelationContext::new("run-overflow", 0);
        // PtyCapture within budget
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 50);
        // DeltaExtraction WAY over budget (100ms vs 1ms p999)
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 100);
        ctx.end_stage(probe, 100_100); // 100,000μs
        let results = ie.process_run(&ctx);
        assert!(results.iter().any(|r| r.overflow));
        assert_eq!(ie.completed_runs(), 1);
        assert_eq!(ie.overflow_runs(), 1);
        assert!((ie.overflow_rate() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_instrumented_enforcer_overhead_tracking() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(0.3);
        ie.record_overhead(0.5);
        assert_eq!(ie.overhead().probe_count, 2);
        assert!(ie.overhead().within_budget);
    }

    #[test]
    fn test_instrumented_enforcer_overflow_rate() {
        let mut ie = InstrumentedEnforcer::new();

        // Run 1: nominal
        let mut ctx = CorrelationContext::new("run-1", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 10);
        ie.process_run(&ctx);

        // Run 2: overflow
        let mut ctx2 = CorrelationContext::new("run-2", 0);
        let probe = ctx2.begin_stage(LatencyStage::PtyCapture, 0);
        ctx2.end_stage(probe, 1_000_000); // 1s — way over any budget
        ie.process_run(&ctx2);

        assert_eq!(ie.completed_runs(), 2);
        assert_eq!(ie.overflow_runs(), 1);
        assert!((ie.overflow_rate() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_instrumented_enforcer_enforcer_access() {
        let ie = InstrumentedEnforcer::new();
        assert!(ie.enforcer().has_stage(LatencyStage::PtyCapture));
        assert!(!ie.enforcer().has_stage(LatencyStage::EndToEndCapture));
    }

    #[test]
    fn test_instrumented_enforcer_with_config() {
        let config = BudgetEnforcerConfig {
            window_size: 50,
            ..BudgetEnforcerConfig::default()
        };
        let ie = InstrumentedEnforcer::with_config(config);
        assert_eq!(ie.completed_runs(), 0);
        assert!(ie.enforcer().has_stage(LatencyStage::PtyCapture));
    }

    // ── Guardrails / Validation ──

    #[test]
    fn test_validation_valid_context() {
        let mut ctx = CorrelationContext::new("run-valid", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let errors = ctx.validate();
        assert!(errors.is_empty());
    }

    #[test]
    fn test_validation_empty_run() {
        let ctx = CorrelationContext::new("run-empty", 0);
        let errors = ctx.validate();
        assert_eq!(errors.len(), 1);
        let is_empty = matches!(&errors[0], InstrumentationError::EmptyRun { .. });
        assert!(is_empty);
    }

    #[test]
    fn test_validation_duplicate_stage() {
        let mut ctx = CorrelationContext::new("run-dup", 0);
        // Record PtyCapture twice
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 2000);
        ctx.end_stage(probe, 2500);
        let errors = ctx.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            InstrumentationError::DuplicateStage {
                stage: LatencyStage::PtyCapture
            }
        )));
    }

    #[test]
    fn test_validation_clock_regression_detected() {
        let mut ctx = CorrelationContext::new("run-regress", 0);
        // Manually add a timing with regression
        ctx.timings.push(StageTiming {
            stage: LatencyStage::PtyCapture,
            start_us: 2000,
            end_us: 1000, // before start
            latency_us: 0.0,
        });
        let errors = ctx.validate();
        assert!(errors
            .iter()
            .any(|e| matches!(e, InstrumentationError::ClockRegression { .. })));
    }

    #[test]
    fn test_validated_ok() {
        let mut ctx = CorrelationContext::new("run-ok", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 100);
        ctx.end_stage(probe, 200);
        let result = ctx.validated();
        assert!(result.is_ok());
    }

    #[test]
    fn test_validated_err() {
        let ctx = CorrelationContext::new("run-err", 0); // empty
        let result = ctx.validated();
        assert!(result.is_err());
    }

    #[test]
    fn test_instrumentation_error_display() {
        let e = InstrumentationError::UnterminatedProbe {
            stage: LatencyStage::StorageWrite,
            start_us: 5000,
        };
        let s = format!("{e}");
        assert!(s.contains("STORAGE_WRITE"));
        assert!(s.contains("5000"));
    }

    #[test]
    fn test_instrumentation_error_serde_roundtrip() {
        let e = InstrumentationError::OverheadBudgetExceeded {
            max_observed_us: 2.5,
            budget_us: 1.0,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: InstrumentationError = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    // ── Degradation ──

    #[test]
    fn test_degradation_ordering() {
        assert!(InstrumentationDegradation::Full < InstrumentationDegradation::SkipOverhead);
        assert!(InstrumentationDegradation::SkipOverhead < InstrumentationDegradation::SkipCorrelation);
        assert!(InstrumentationDegradation::SkipCorrelation < InstrumentationDegradation::Passthrough);
    }

    #[test]
    fn test_degradation_display() {
        assert_eq!(format!("{}", InstrumentationDegradation::Full), "FULL");
        assert_eq!(
            format!("{}", InstrumentationDegradation::Passthrough),
            "PASSTHROUGH"
        );
    }

    #[test]
    fn test_degradation_serde_roundtrip() {
        let d = InstrumentationDegradation::SkipCorrelation;
        let json = serde_json::to_string(&d).unwrap();
        let back: InstrumentationDegradation = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    // ── InstrumentedEnforcer diagnostics ──

    #[test]
    fn test_enforcer_degradation_full_when_within_budget() {
        let ie = InstrumentedEnforcer::new();
        assert_eq!(ie.current_degradation(), InstrumentationDegradation::Full);
    }

    #[test]
    fn test_enforcer_degradation_skip_overhead() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(3.0); // 3x budget (budget=1μs)
        assert_eq!(
            ie.current_degradation(),
            InstrumentationDegradation::SkipOverhead
        );
    }

    #[test]
    fn test_enforcer_degradation_skip_correlation() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(7.0); // 7x budget
        assert_eq!(
            ie.current_degradation(),
            InstrumentationDegradation::SkipCorrelation
        );
    }

    #[test]
    fn test_enforcer_degradation_passthrough() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(15.0); // 15x budget
        assert_eq!(
            ie.current_degradation(),
            InstrumentationDegradation::Passthrough
        );
    }

    #[test]
    fn test_enforcer_is_healthy_nominal() {
        let mut ie = InstrumentedEnforcer::new();
        // Record a nominal run
        let mut ctx = CorrelationContext::new("run-h", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 10);
        ie.process_run(&ctx);
        assert!(ie.is_healthy());
    }

    #[test]
    fn test_enforcer_is_unhealthy_overhead() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(5.0); // over budget
        assert!(!ie.is_healthy());
    }

    #[test]
    fn test_enforcer_status_line_format() {
        let ie = InstrumentedEnforcer::new();
        let status = ie.status_line();
        assert!(status.contains("degradation=FULL"));
        assert!(status.contains("runs=0"));
        assert!(status.contains("overflows=0"));
    }

    #[test]
    fn test_enforcer_diagnostic_snapshot() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(0.3);
        let diag = ie.diagnostic();
        assert_eq!(diag.degradation, InstrumentationDegradation::Full);
        assert_eq!(diag.completed_runs, 0);
        assert_eq!(diag.overhead.probe_count, 1);
        assert!(diag.last_validation_errors.is_empty());
    }

    #[test]
    fn test_enforcer_diagnostic_serde_roundtrip() {
        let ie = InstrumentedEnforcer::new();
        let diag = ie.diagnostic();
        let json = serde_json::to_string(&diag).unwrap();
        let back: InstrumentationDiagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(diag, back);
    }

    #[test]
    fn test_enforcer_process_validated_run() {
        let mut ie = InstrumentedEnforcer::new();
        let mut ctx = CorrelationContext::new("run-pv", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 50);
        let (results, errors) = ie.process_validated_run(&ctx);
        assert_eq!(results.len(), 1);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_enforcer_process_validated_run_with_errors() {
        let mut ie = InstrumentedEnforcer::new();
        let ctx = CorrelationContext::new("run-empty-val", 0); // empty run
        let (results, errors) = ie.process_validated_run(&ctx);
        assert!(results.is_empty());
        assert!(!errors.is_empty());
    }

    // ── FastProbe ──

    #[test]
    fn test_fast_probe_begin() {
        let probe = FastProbe::begin(LatencyStage::PtyCapture, 1000);
        assert_eq!(probe.stage, LatencyStage::PtyCapture);
        assert_eq!(probe.start_us, 1000);
    }

    #[test]
    fn test_fast_probe_elapsed() {
        let probe = FastProbe::begin(LatencyStage::DeltaExtraction, 1000);
        assert!((probe.elapsed_us(1500) - 500.0).abs() < 1e-10);
    }

    #[test]
    fn test_fast_probe_clock_regression() {
        let probe = FastProbe::begin(LatencyStage::StorageWrite, 2000);
        assert_eq!(probe.elapsed_us(1000), 0.0);
    }

    #[test]
    fn test_fast_probe_zero_duration() {
        let probe = FastProbe::begin(LatencyStage::EventEmission, 1000);
        assert_eq!(probe.elapsed_us(1000), 0.0);
    }

    #[test]
    fn test_fast_probe_copy_semantics() {
        let probe = FastProbe::begin(LatencyStage::ApiResponse, 100);
        let copy = probe;
        // Both should be usable (Copy semantics, no move).
        assert_eq!(probe.elapsed_us(200), 100.0);
        assert_eq!(copy.elapsed_us(200), 100.0);
    }

    // ── MitigationLevel ──

    #[test]
    fn test_mitigation_level_ordering() {
        assert!(MitigationLevel::None < MitigationLevel::Defer);
        assert!(MitigationLevel::Defer < MitigationLevel::Degrade);
        assert!(MitigationLevel::Degrade < MitigationLevel::Shed);
        assert!(MitigationLevel::Shed < MitigationLevel::Skip);
    }

    #[test]
    fn test_mitigation_level_severity() {
        assert_eq!(MitigationLevel::None.severity(), 0);
        assert_eq!(MitigationLevel::Defer.severity(), 1);
        assert_eq!(MitigationLevel::Degrade.severity(), 2);
        assert_eq!(MitigationLevel::Shed.severity(), 3);
        assert_eq!(MitigationLevel::Skip.severity(), 4);
    }

    #[test]
    fn test_mitigation_level_roundtrip() {
        for &level in MitigationLevel::ALL {
            let mit = level.to_mitigation();
            let back = MitigationLevel::from_mitigation(mit);
            assert_eq!(level, back);
        }
    }

    #[test]
    fn test_mitigation_level_serde_roundtrip() {
        for &level in MitigationLevel::ALL {
            let json = serde_json::to_string(&level).unwrap();
            let back: MitigationLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    // ── PolicyConstraint ──

    #[test]
    fn test_policy_constraint_allows() {
        let pc = PolicyConstraint {
            stage: LatencyStage::StorageWrite,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        };
        assert!(pc.allows(MitigationLevel::None));
        assert!(pc.allows(MitigationLevel::Defer));
        assert!(!pc.allows(MitigationLevel::Degrade));
        assert!(!pc.allows(MitigationLevel::Skip));
    }

    #[test]
    fn test_policy_constraint_clamp() {
        let pc = PolicyConstraint {
            stage: LatencyStage::StorageWrite,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        };
        assert_eq!(pc.clamp(MitigationLevel::None), MitigationLevel::None);
        assert_eq!(pc.clamp(MitigationLevel::Defer), MitigationLevel::Defer);
        assert_eq!(pc.clamp(MitigationLevel::Skip), MitigationLevel::Defer);
    }

    #[test]
    fn test_default_policy_constraints_cover_all_stages() {
        let constraints = default_policy_constraints();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                constraints.iter().any(|c| c.stage == stage),
                "missing policy constraint for {stage}"
            );
        }
    }

    #[test]
    fn test_critical_stages_have_limited_mitigation() {
        let constraints = default_policy_constraints();
        for c in &constraints {
            if c.critical {
                // Critical stages should NOT allow Skip.
                assert!(
                    c.max_level < MitigationLevel::Skip,
                    "critical stage {} allows Skip",
                    c.stage
                );
            }
        }
    }

    // ── RecoveryProtocol ──

    #[test]
    fn test_recovery_protocol_defaults() {
        let rp = RecoveryProtocol::default();
        assert_eq!(rp.cooldown_observations, 20);
        assert_eq!(rp.max_degraded_duration_us, 30_000_000);
        assert!(rp.gradual);
    }

    #[test]
    fn test_recovery_protocol_serde_roundtrip() {
        let rp = RecoveryProtocol::default();
        let json = serde_json::to_string(&rp).unwrap();
        let back: RecoveryProtocol = serde_json::from_str(&json).unwrap();
        assert_eq!(rp, back);
    }

    // ── RuntimeEnforcer ──

    #[test]
    fn test_runtime_enforcer_new() {
        let re = RuntimeEnforcer::with_defaults();
        assert_eq!(re.total_observations(), 0);
        assert_eq!(re.total_escalations(), 0);
        assert_eq!(re.total_recoveries(), 0);
        assert!(re.is_fully_recovered());
    }

    #[test]
    fn test_runtime_enforcer_nominal() {
        let mut re = RuntimeEnforcer::with_defaults();
        // Record many nominal observations to get past warmup.
        for i in 0..50 {
            let d = re.enforce(LatencyStage::PtyCapture, 10.0, "test", i * 1000);
            assert!(!d.overflow);
            assert_eq!(d.applied_mitigation, MitigationLevel::None);
        }
        assert!(re.is_fully_recovered());
        assert_eq!(re.total_escalations(), 0);
    }

    #[test]
    fn test_runtime_enforcer_warmup_suppresses() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::DeltaExtraction,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 5,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // During warmup, even overflow shouldn't escalate.
        for i in 0..5 {
            let d = re.enforce(LatencyStage::DeltaExtraction, 100_000.0, "test", i * 1000);
            assert!(d.warmup_active);
            assert_eq!(d.applied_mitigation, MitigationLevel::None);
        }
    }

    #[test]
    fn test_runtime_enforcer_escalation() {
        let mut re = RuntimeEnforcer::with_defaults();
        // Get past warmup with normal observations.
        for i in 0..20 {
            re.enforce(LatencyStage::PatternDetection, 10.0, "test", i * 1000);
        }
        // Now trigger overflow (PatternDetection p999=10000, so 50000 overflows).
        let d = re.enforce(LatencyStage::PatternDetection, 50_000.0, "test", 100_000);
        assert!(d.overflow);
        assert!(d.applied_mitigation >= MitigationLevel::None);
        // Should have escalated.
        let level = re.current_level(LatencyStage::PatternDetection);
        assert!(level > MitigationLevel::None);
    }

    #[test]
    fn test_runtime_enforcer_policy_clamp() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::StorageWrite,
                max_level: MitigationLevel::Defer,
                critical: true,
                warmup_count: 0, // no warmup
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // StorageWrite with extreme overflow — policy should clamp to Defer.
        re.enforce(LatencyStage::StorageWrite, 1_000_000.0, "test", 1000);
        let level = re.current_level(LatencyStage::StorageWrite);
        assert!(level <= MitigationLevel::Defer);
    }

    #[test]
    fn test_runtime_enforcer_recovery() {
        let config = RuntimeEnforcerConfig {
            recovery: RecoveryProtocol {
                cooldown_observations: 5,
                max_degraded_duration_us: 1_000_000_000,
                gradual: true,
            },
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::PatternDetection,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 0,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // Trigger escalation.
        re.enforce(LatencyStage::PatternDetection, 100_000.0, "test", 1000);
        assert!(re.current_level(LatencyStage::PatternDetection) > MitigationLevel::None);

        // Now send enough within-budget observations for recovery.
        for i in 0..10 {
            re.enforce(LatencyStage::PatternDetection, 10.0, "test", 2000 + i * 100);
        }
        // Should have recovered (at least partially).
        let level = re.current_level(LatencyStage::PatternDetection);
        // With gradual recovery, may have stepped down but not necessarily to None.
        assert!(level < MitigationLevel::Skip);
    }

    #[test]
    fn test_runtime_enforcer_status_line_nominal() {
        let re = RuntimeEnforcer::with_defaults();
        let status = re.status_line();
        assert!(status.contains("NOMINAL"));
    }

    #[test]
    fn test_runtime_enforcer_status_line_degraded() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::PatternDetection,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 0,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        re.enforce(LatencyStage::PatternDetection, 100_000.0, "test", 1000);
        let status = re.status_line();
        assert!(status.contains("DEGRADED"));
    }

    #[test]
    fn test_runtime_enforcer_drain_decisions() {
        let mut re = RuntimeEnforcer::with_defaults();
        re.enforce(LatencyStage::PtyCapture, 10.0, "test", 0);
        re.enforce(LatencyStage::DeltaExtraction, 10.0, "test", 100);
        let decisions = re.drain_decisions();
        assert_eq!(decisions.len(), 2);
        assert_eq!(re.drain_decisions().len(), 0);
    }

    #[test]
    fn test_enforcement_decision_serde_roundtrip() {
        let d = EnforcementDecision {
            stage: LatencyStage::PtyCapture,
            latency_us: 42.0,
            overflow: false,
            raw_mitigation: MitigationLevel::None,
            applied_mitigation: MitigationLevel::None,
            recovery: false,
            reason: None,
            warmup_active: false,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: EnforcementDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn test_stage_enforcement_state_serde_roundtrip() {
        let s = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 5,
            last_escalation_us: 1000,
            escalation_count: 2,
            recovery_count: 1,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: StageEnforcementState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // ── RuntimeEnforcer Impl extensions ──

    #[test]
    fn test_runtime_enforcer_enforce_run() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: default_policy_constraints()
                .into_iter()
                .map(|mut c| {
                    c.warmup_count = 0;
                    c
                })
                .collect(),
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        let mut ctx = CorrelationContext::new("batch-run", 0);
        let mut t = 1000_u64;
        for &stage in LatencyStage::PIPELINE_STAGES {
            let probe = ctx.begin_stage(stage, t);
            t += 50; // 50μs, well within budget
            ctx.end_stage(probe, t);
            t += 10;
        }
        let decisions = re.enforce_run(&ctx, 0);
        assert_eq!(decisions.len(), 8);
        assert!(decisions.iter().all(|d| !d.overflow));
    }

    #[test]
    fn test_runtime_enforcer_diagnostic_snapshot() {
        let mut re = RuntimeEnforcer::with_defaults();
        re.enforce(LatencyStage::PtyCapture, 10.0, "test", 0);
        let snap = re.diagnostic_snapshot();
        assert_eq!(snap.observation_count, 1);
        assert_eq!(snap.total_escalations, 0);
        assert!(snap.fully_recovered);
        assert_eq!(snap.stage_states.len(), 8);
    }

    #[test]
    fn test_runtime_enforcer_snapshot_serde_roundtrip() {
        let mut re = RuntimeEnforcer::with_defaults();
        for i in 0..5 {
            re.enforce(LatencyStage::PtyCapture, 10.0, "test", i * 100);
        }
        let snap = re.diagnostic_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: RuntimeEnforcerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.observation_count, back.observation_count);
        assert_eq!(snap.total_escalations, back.total_escalations);
        assert_eq!(snap.fully_recovered, back.fully_recovered);
    }

    #[test]
    fn test_runtime_enforcer_timeout_recovery() {
        let config = RuntimeEnforcerConfig {
            recovery: RecoveryProtocol {
                cooldown_observations: 1000, // high, so cooldown won't trigger
                max_degraded_duration_us: 5000, // 5ms timeout
                gradual: false, // jump to full
            },
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::PatternDetection,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 0,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // Trigger escalation at time 1000.
        re.enforce(LatencyStage::PatternDetection, 100_000.0, "test", 1000);
        assert!(re.current_level(LatencyStage::PatternDetection) > MitigationLevel::None);

        // Record ok observation at time 7000 (6ms after escalation, past 5ms timeout).
        let d = re.enforce(LatencyStage::PatternDetection, 10.0, "test", 7000);
        assert!(d.recovery);
        assert_eq!(
            re.current_level(LatencyStage::PatternDetection),
            MitigationLevel::None
        );
    }

    // ── Helper ──

    fn make_valid_run() -> PipelineRun {
        let budgets = default_budgets();
        let mut stages = Vec::new();
        let mut t = 1_000_000_u64; // start at 1s epoch

        for &stage in LatencyStage::PIPELINE_STAGES {
            let budget = budgets.iter().find(|b| b.stage == stage).unwrap();
            let latency = budget.p50_us;
            stages.push(StageObservation {
                stage,
                latency_us: latency,
                correlation_id: "test-run-001".into(),
                scenario_id: Some("nominal".into()),
                start_epoch_us: t,
                end_epoch_us: t + latency as u64,
                overflow: false,
                reason: None,
                mitigation: Mitigation::None,
            });
            t += latency as u64 + 100; // 100μs gap between stages
        }

        let total: f64 = stages.iter().map(|s| s.latency_us).sum();

        PipelineRun {
            run_id: "test-run-001".into(),
            correlation_id: "test-run-001".into(),
            scenario_id: Some("nominal".into()),
            stages,
            total_latency_us: total,
            has_overflow: false,
            reasons: vec![],
        }
    }

    // ── A4: Adaptive Budget Allocator ──

    #[test]
    fn test_stage_pressure_compute() {
        let p = StagePressure::compute(LatencyStage::PtyCapture, 5000.0, 10000.0);
        assert_eq!(p.headroom, 0.5);
        assert!(!p.is_over_budget());
        assert_eq!(p.donatable_slack_us(), 5000.0);
        assert_eq!(p.deficit_us(), 0.0);
    }

    #[test]
    fn test_stage_pressure_over_budget() {
        let p = StagePressure::compute(LatencyStage::StorageWrite, 15000.0, 10000.0);
        assert!(p.headroom < 0.0);
        assert!(p.is_over_budget());
        assert_eq!(p.donatable_slack_us(), 0.0);
        assert_eq!(p.deficit_us(), 5000.0);
    }

    #[test]
    fn test_stage_pressure_zero_budget() {
        let p = StagePressure::compute(LatencyStage::PtyCapture, 100.0, 0.0);
        assert_eq!(p.headroom, 0.0);
        assert!(!p.is_over_budget());
        assert_eq!(p.donatable_slack_us(), 0.0);
    }

    #[test]
    fn test_allocator_config_default_valid() {
        let cfg = AdaptiveAllocatorConfig::default();
        let errors = cfg.validate();
        assert!(errors.is_empty(), "default config should be valid: {:?}", errors);
    }

    #[test]
    fn test_allocator_config_validation_catches_bad_values() {
        let cfg = AdaptiveAllocatorConfig {
            max_adjustment_pct: -0.1,
            min_budget_pct: 0.0,
            max_budget_pct: 0.5,
            pressure_alpha: 1.5,
            min_donor_headroom: 1.0,
            ..Default::default()
        };
        let errors = cfg.validate();
        assert_eq!(errors.len(), 5);
    }

    #[test]
    fn test_allocator_with_defaults_conservation() {
        let alloc = AdaptiveAllocator::with_defaults();
        let sum: f64 = alloc.lanes().iter().map(|l| l.current_p95_us).sum();
        assert!((sum - alloc.total_budget_us()).abs() < 1e-6);
        assert!(alloc.global_slack_us().abs() < 1e-6);
    }

    #[test]
    fn test_allocator_warmup_noop() {
        let mut alloc = AdaptiveAllocator::with_defaults();
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| StagePressure::compute(l.stage, l.default_p95_us * 0.5, l.default_p95_us))
            .collect();
        let d = alloc.allocate(&pressures, "test-warmup");
        assert!(d.warmup);
        assert_eq!(d.reason, AllocationReason::Warmup);
        assert!(d.adjustments.is_empty());
    }

    #[test]
    fn test_allocator_all_within_budget() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // All stages well within budget.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| StagePressure::compute(l.stage, l.default_p95_us * 0.5, l.default_p95_us))
            .collect();
        let d = alloc.allocate(&pressures, "test-nominal");
        assert!(!d.warmup);
        assert_eq!(d.reason, AllocationReason::AllWithinBudget);
    }

    #[test]
    fn test_allocator_redistribution_preserves_total() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.10,
            ..Default::default()
        };
        let budgets = default_budgets();
        let mut alloc = AdaptiveAllocator::new(&budgets, cfg);
        let total_before = alloc.total_budget_us();

        // Run many epochs with StorageWrite over-budget and PtyCapture under-budget.
        for epoch in 0..20 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 1.5, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("epoch-{}", epoch));
        }

        // Conservation invariant.
        let sum: f64 = alloc.lanes().iter().map(|l| l.current_p95_us).sum();
        assert!(
            (sum - total_before).abs() < 1.0, // allow small float drift
            "budget conservation violated: {} vs {}",
            sum,
            total_before
        );

        // StorageWrite should have more budget than its default.
        let sw = alloc.allocation(LatencyStage::StorageWrite).unwrap();
        assert!(
            sw.current_p95_us >= sw.default_p95_us,
            "StorageWrite should have received slack"
        );
    }

    #[test]
    fn test_allocator_respects_floor() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_budget_pct: 0.50,
            max_adjustment_pct: 0.50, // allow big adjustments to test floor
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Many epochs pushing donors hard.
        for epoch in 0..100 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::ApiResponse {
                        StagePressure::compute(l.stage, l.current_p95_us * 3.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("floor-{}", epoch));
        }

        // No lane should drop below 50% of its default.
        for lane in alloc.lanes() {
            assert!(
                lane.current_p95_us >= lane.default_p95_us * 0.50 - 1e-6,
                "{} dropped below floor: {} < {}",
                lane.stage,
                lane.current_p95_us,
                lane.default_p95_us * 0.50
            );
        }
    }

    #[test]
    fn test_allocator_respects_ceiling() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            max_budget_pct: 2.0,
            max_adjustment_pct: 0.50,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        for epoch in 0..100 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::DeltaExtraction {
                        StagePressure::compute(l.stage, l.current_p95_us * 5.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("ceil-{}", epoch));
        }

        for lane in alloc.lanes() {
            assert!(
                lane.current_p95_us <= lane.default_p95_us * 2.0 + 1e-6,
                "{} exceeded ceiling: {} > {}",
                lane.stage,
                lane.current_p95_us,
                lane.default_p95_us * 2.0
            );
        }
    }

    #[test]
    fn test_allocator_reset_restores_defaults() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Do some redistribution.
        for epoch in 0..10 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("pre-reset-{}", epoch));
        }

        let d = alloc.reset();
        assert_eq!(d.reason, AllocationReason::ResetToDefaults);

        for lane in alloc.lanes() {
            assert!(
                (lane.current_p95_us - lane.default_p95_us).abs() < 1e-6,
                "{} not reset: {} vs {}",
                lane.stage,
                lane.current_p95_us,
                lane.default_p95_us
            );
        }
    }

    #[test]
    fn test_allocator_deterministic_replay() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let budgets = default_budgets();

        // Run sequence once.
        let mut alloc1 = AdaptiveAllocator::new(&budgets, cfg.clone());
        let pressures_seq: Vec<Vec<StagePressure>> = (0..10)
            .map(|i| {
                alloc1
                    .lanes()
                    .iter()
                    .map(|l| {
                        let factor = if l.stage == LatencyStage::StorageWrite {
                            1.5 + (i as f64) * 0.1
                        } else {
                            0.5
                        };
                        StagePressure::compute(l.stage, l.current_p95_us * factor, l.current_p95_us)
                    })
                    .collect()
            })
            .collect();

        let mut decisions1 = Vec::new();
        for (i, p) in pressures_seq.iter().enumerate() {
            decisions1.push(alloc1.allocate(p, &format!("run-{}", i)));
        }

        // Replay with fresh allocator.
        let mut alloc2 = AdaptiveAllocator::new(&budgets, cfg);
        let mut decisions2 = Vec::new();
        for (i, p) in pressures_seq.iter().enumerate() {
            decisions2.push(alloc2.allocate(p, &format!("run-{}", i)));
        }

        // Decisions should be identical.
        assert_eq!(decisions1.len(), decisions2.len());
        for (d1, d2) in decisions1.iter().zip(decisions2.iter()) {
            assert_eq!(d1.epoch, d2.epoch);
            assert_eq!(d1.reason, d2.reason);
            assert_eq!(d1.adjustments.len(), d2.adjustments.len());
        }

        // Final allocations should be identical.
        for (l1, l2) in alloc1.lanes().iter().zip(alloc2.lanes().iter()) {
            assert!(
                (l1.current_p95_us - l2.current_p95_us).abs() < 1e-6,
                "replay diverged for {}: {} vs {}",
                l1.stage,
                l1.current_p95_us,
                l2.current_p95_us
            );
        }
    }

    #[test]
    fn test_allocator_no_donors_when_all_pressured() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.15,
            pressure_alpha: 0.3,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // Run many epochs with all stages over-budget so EWMA headroom goes negative.
        for i in 0..20 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us))
                .collect();
            alloc.allocate(&pressures, &format!("all-pressure-{}", i));
        }
        // After enough epochs, smoothed headroom should be negative for all lanes.
        let d = alloc.recent_decisions(1)[0].clone();
        assert_eq!(d.reason, AllocationReason::NoDonors);
    }

    #[test]
    fn test_allocator_snapshot_serialization() {
        let alloc = AdaptiveAllocator::with_defaults();
        let snap = alloc.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: AllocatorSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap.epoch, back.epoch);
        assert_eq!(snap.lanes.len(), back.lanes.len());
        assert!((snap.total_budget_us - back.total_budget_us).abs() < 1e-6);
    }

    #[test]
    fn test_allocator_status_line_nominal() {
        let alloc = AdaptiveAllocator::with_defaults();
        let s = alloc.status_line();
        assert!(s.starts_with("allocator=NOMINAL"));
    }

    #[test]
    fn test_allocator_status_line_redistribution() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // Make StorageWrite over-budget so its smoothed headroom goes negative.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| {
                if l.stage == LatencyStage::StorageWrite {
                    StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us)
                } else {
                    StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                }
            })
            .collect();
        alloc.allocate(&pressures, "status-test");
        let s = alloc.status_line();
        assert!(s.contains("REDISTRIBUTING") || s.contains("NOMINAL"));
    }

    #[test]
    fn test_allocation_reason_display() {
        assert_eq!(format!("{}", AllocationReason::Warmup), "WARMUP");
        assert_eq!(
            format!("{}", AllocationReason::AllWithinBudget),
            "ALL_WITHIN_BUDGET"
        );
        assert_eq!(format!("{}", AllocationReason::NoDonors), "NO_DONORS");
        assert_eq!(
            format!(
                "{}",
                AllocationReason::SlackRedistributed {
                    donor_count: 3,
                    receiver_count: 1
                }
            ),
            "SLACK_REDISTRIBUTED donors=3 receivers=1"
        );
        assert_eq!(
            format!("{}", AllocationReason::ResetToDefaults),
            "RESET_TO_DEFAULTS"
        );
    }

    #[test]
    fn test_allocator_recent_decisions() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        for i in 0..5 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 0.5, l.current_p95_us))
                .collect();
            alloc.allocate(&pressures, &format!("d-{}", i));
        }
        let recent = alloc.recent_decisions(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].epoch, 3);
        assert_eq!(recent[2].epoch, 5);
    }

    #[test]
    fn test_lane_allocation_serde() {
        let lane = LaneAllocation::new(LatencyStage::PtyCapture, 10000.0);
        let json = serde_json::to_string(&lane).expect("serialize");
        let back: LaneAllocation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(lane.stage, back.stage);
        assert!((lane.default_p95_us - back.default_p95_us).abs() < 1e-10);
    }

    #[test]
    fn test_allocation_decision_serde() {
        let d = AllocationDecision {
            epoch: 42,
            correlation_id: "test-serde".into(),
            adjustments: vec![StageAdjustment {
                stage: LatencyStage::StorageWrite,
                before_p95_us: 5000.0,
                after_p95_us: 5500.0,
                delta_us: 500.0,
                rate_clamped: false,
                bound_clamped: false,
            }],
            slack_pool_before_us: 100.0,
            slack_pool_after_us: 50.0,
            warmup: false,
            reason: AllocationReason::SlackRedistributed {
                donor_count: 2,
                receiver_count: 1,
            },
        };
        let json = serde_json::to_string(&d).expect("serialize");
        let back: AllocationDecision = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(d.epoch, back.epoch);
        assert_eq!(d.reason, back.reason);
        assert_eq!(d.adjustments.len(), back.adjustments.len());
    }

    #[test]
    fn test_stage_pressure_serde() {
        let p = StagePressure::compute(LatencyStage::EventEmission, 1500.0, 2000.0);
        let json = serde_json::to_string(&p).expect("serialize");
        let back: StagePressure = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(p.stage, back.stage);
        assert!((p.headroom - back.headroom).abs() < 1e-10);
    }

    #[test]
    fn test_allocator_bounded_rate() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            max_adjustment_pct: 0.10,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg.clone());

        // Single epoch with huge pressure on one stage.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| {
                if l.stage == LatencyStage::StorageWrite {
                    StagePressure::compute(l.stage, l.current_p95_us * 10.0, l.current_p95_us)
                } else {
                    StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                }
            })
            .collect();
        let d = alloc.allocate(&pressures, "bounded-rate-test");

        // Each donor should have donated at most max_adjustment_pct of its default.
        for adj in &d.adjustments {
            if adj.delta_us < 0.0 {
                let lane = alloc
                    .lanes()
                    .iter()
                    .find(|l| l.stage == adj.stage)
                    .unwrap();
                let max_donate = lane.default_p95_us * cfg.max_adjustment_pct;
                assert!(
                    (-adj.delta_us) <= max_donate + 1e-6,
                    "{} donated too much: {} > {}",
                    adj.stage,
                    -adj.delta_us,
                    max_donate
                );
            }
        }
    }

    #[test]
    fn test_allocator_over_budget_epoch_count() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        for _epoch in 0..5 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::PatternDetection {
                        StagePressure::compute(l.stage, l.current_p95_us * 1.5, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.5, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, "epoch-count-test");
        }

        let pd = alloc.allocation(LatencyStage::PatternDetection).unwrap();
        assert_eq!(pd.over_budget_epochs, 5);
    }

    // ── A4 Impl: Bridge, Degradation, Logging ──

    #[test]
    fn test_pressures_from_enforcer_snapshot() {
        let enforcer = BudgetEnforcer::with_defaults();
        let snap = enforcer.snapshot();
        let pressures = AdaptiveAllocator::pressures_from_snapshot(&snap);
        // Should have one pressure per non-aggregate stage.
        assert_eq!(pressures.len(), 8);
        for p in &pressures {
            assert!(!p.stage.is_aggregate());
        }
    }

    #[test]
    fn test_pressures_from_snapshot_headroom_with_data() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Record some low-latency observations for PtyCapture.
        for _ in 0..10 {
            enforcer.record(LatencyStage::PtyCapture, 1000.0, "test");
        }
        let snap = enforcer.snapshot();
        let pressures = AdaptiveAllocator::pressures_from_snapshot(&snap);
        let pty = pressures.iter().find(|p| p.stage == LatencyStage::PtyCapture).unwrap();
        // PtyCapture budget is 10000 p95, observed ~1000 → headroom > 0.
        assert!(pty.headroom > 0.0, "expected positive headroom: {}", pty.headroom);
    }

    #[test]
    fn test_adjusted_budgets_default_is_identity() {
        let alloc = AdaptiveAllocator::with_defaults();
        let adjusted = alloc.adjusted_budgets();
        let defaults = default_budgets();
        for adj in &adjusted {
            let orig = defaults.iter().find(|b| b.stage == adj.stage).unwrap();
            assert!(
                (adj.p95_us - orig.p95_us).abs() < 1e-6,
                "{}: adjusted p95={} vs default p95={}",
                adj.stage,
                adj.p95_us,
                orig.p95_us
            );
        }
    }

    #[test]
    fn test_adjusted_budgets_proportional_scaling() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Run epochs with StorageWrite over-budget to trigger redistribution.
        for i in 0..20 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 1.5, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("adj-{}", i));
        }

        let adjusted = alloc.adjusted_budgets();
        for budget in &adjusted {
            // Monotonic invariant: p50 <= p95 <= p99 <= p999.
            assert!(
                budget.p50_us <= budget.p95_us + 1e-6,
                "{}: p50={} > p95={}",
                budget.stage,
                budget.p50_us,
                budget.p95_us
            );
            assert!(
                budget.p95_us <= budget.p99_us + 1e-6,
                "{}: p95={} > p99={}",
                budget.stage,
                budget.p95_us,
                budget.p99_us
            );
            assert!(
                budget.p99_us <= budget.p999_us + 1e-6,
                "{}: p99={} > p999={}",
                budget.stage,
                budget.p99_us,
                budget.p999_us
            );
        }
    }

    #[test]
    fn test_allocator_degradation_healthy() {
        let alloc = AdaptiveAllocator::with_defaults();
        assert_eq!(alloc.current_degradation(), AllocatorDegradation::Healthy);
        assert!(alloc.is_healthy());
    }

    #[test]
    fn test_allocator_degradation_display() {
        assert_eq!(format!("{}", AllocatorDegradation::Healthy), "HEALTHY");
        assert!(format!("{}", AllocatorDegradation::Oscillating { lane_count: 5 })
            .contains("OSCILLATING"));
        assert!(
            format!(
                "{}",
                AllocatorDegradation::ConservationDrift { drift_us: 1.5 }
            )
            .contains("CONSERVATION_DRIFT")
        );
        assert!(
            format!("{}", AllocatorDegradation::FloorSaturation { lane_count: 4 })
                .contains("FLOOR_SATURATION")
        );
    }

    #[test]
    fn test_allocator_log_entry_generation() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // Nominal epoch.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 0.5, l.current_p95_us))
            .collect();
        alloc.allocate(&pressures, "log-test");

        let entry = alloc.last_log_entry().unwrap();
        assert_eq!(entry.epoch, 1);
        assert_eq!(entry.correlation_id, "log-test");
        assert_eq!(entry.reason, "ALL_WITHIN_BUDGET");
        assert_eq!(entry.adjustment_count, 0);
    }

    #[test]
    fn test_allocator_log_entry_redistribution() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Multiple epochs to get headroom negative for StorageWrite.
        for i in 0..10 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.2, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("log-redist-{}", i));
        }

        let entry = alloc.last_log_entry().unwrap();
        assert!(entry.reason.contains("SLACK_REDISTRIBUTED"));
        assert!(entry.adjustment_count > 0);
        assert!(entry.total_donated_us > 0.0);
        assert!(entry.total_received_us > 0.0);
    }

    #[test]
    fn test_allocator_log_entry_serde() {
        let entry = AllocationLogEntry {
            epoch: 10,
            correlation_id: "serde-test".into(),
            reason: "WARMUP".into(),
            adjustment_count: 0,
            total_donated_us: 0.0,
            total_received_us: 0.0,
            conservation_error_us: 0.001,
            degradation: AllocatorDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: AllocationLogEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry.epoch, back.epoch);
        assert_eq!(entry.reason, back.reason);
    }

    #[test]
    fn test_allocator_degradation_serde() {
        let cases = vec![
            AllocatorDegradation::Healthy,
            AllocatorDegradation::Oscillating { lane_count: 3 },
            AllocatorDegradation::ConservationDrift { drift_us: 1.23 },
            AllocatorDegradation::FloorSaturation { lane_count: 5 },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).expect("serialize");
            let back: AllocatorDegradation = serde_json::from_str(&json).expect("deserialize");
            // For f64 variant, use tolerance.
            match (&case, &back) {
                (
                    AllocatorDegradation::ConservationDrift { drift_us: a },
                    AllocatorDegradation::ConservationDrift { drift_us: b },
                ) => assert!((a - b).abs() < 1e-10),
                _ => assert_eq!(case, back),
            }
        }
    }

    // ── B1: Three-Lane Scheduler ──

    #[test]
    fn test_scheduler_lane_priority_order() {
        assert!(SchedulerLane::Input < SchedulerLane::Control);
        assert!(SchedulerLane::Control < SchedulerLane::Bulk);
        assert_eq!(SchedulerLane::Input.priority(), 0);
        assert_eq!(SchedulerLane::Control.priority(), 1);
        assert_eq!(SchedulerLane::Bulk.priority(), 2);
    }

    #[test]
    fn test_scheduler_lane_all_complete() {
        assert_eq!(SchedulerLane::ALL.len(), 3);
    }

    #[test]
    fn test_scheduler_lane_display() {
        assert_eq!(format!("{}", SchedulerLane::Input), "input");
        assert_eq!(format!("{}", SchedulerLane::Control), "control");
        assert_eq!(format!("{}", SchedulerLane::Bulk), "bulk");
    }

    #[test]
    fn test_stage_to_lane_mapping() {
        assert_eq!(stage_to_lane(LatencyStage::PtyCapture), SchedulerLane::Input);
        assert_eq!(stage_to_lane(LatencyStage::DeltaExtraction), SchedulerLane::Input);
        assert_eq!(stage_to_lane(LatencyStage::ApiResponse), SchedulerLane::Input);
        assert_eq!(stage_to_lane(LatencyStage::EventEmission), SchedulerLane::Control);
        assert_eq!(stage_to_lane(LatencyStage::WorkflowDispatch), SchedulerLane::Control);
        assert_eq!(stage_to_lane(LatencyStage::ActionExecution), SchedulerLane::Control);
        assert_eq!(stage_to_lane(LatencyStage::StorageWrite), SchedulerLane::Bulk);
        assert_eq!(stage_to_lane(LatencyStage::PatternDetection), SchedulerLane::Bulk);
    }

    #[test]
    fn test_scheduler_config_default_valid() {
        let cfg = LaneSchedulerConfig::default();
        let errors = cfg.validate();
        assert!(errors.is_empty(), "default config should be valid: {:?}", errors);
    }

    #[test]
    fn test_scheduler_config_cpu_share_overflow() {
        let cfg = LaneSchedulerConfig {
            input_cpu_share: 0.5,
            control_cpu_share: 0.4,
            bulk_cpu_share: 0.3,
            ..Default::default()
        };
        let errors = cfg.validate();
        assert!(!errors.is_empty());
        assert!(errors[0].contains("CPU shares"));
    }

    #[test]
    fn test_scheduler_admit_basic() {
        let mut sched = LaneScheduler::with_defaults();
        let (item, decision) = sched.admit(
            LatencyStage::PtyCapture,
            100.0,
            "test-1",
            0,
            1000,
        );
        assert_eq!(item.lane, SchedulerLane::Input);
        assert_eq!(decision, AdmissionDecision::Admitted);
        assert_eq!(sched.lane_state(SchedulerLane::Input).depth, 1);
    }

    #[test]
    fn test_scheduler_bulk_shed_under_input_pressure() {
        let cfg = LaneSchedulerConfig {
            input_queue_capacity: 4,
            input_pressure_threshold: 0.75,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        // Fill input to 3/4 capacity (75%) = at threshold.
        for i in 0..3 {
            sched.admit(LatencyStage::PtyCapture, 10.0, &format!("inp-{}", i), 0, 0);
        }
        assert!(sched.input_under_pressure());

        // Bulk item should be shed.
        let (_item, decision) = sched.admit(
            LatencyStage::StorageWrite,
            1000.0,
            "bulk-shed",
            0,
            0,
        );
        assert_eq!(decision, AdmissionDecision::Shed);
    }

    #[test]
    fn test_scheduler_input_never_shed() {
        let cfg = LaneSchedulerConfig {
            input_queue_capacity: 2,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        // Fill input to capacity.
        sched.admit(LatencyStage::PtyCapture, 10.0, "a", 0, 0);
        sched.admit(LatencyStage::PtyCapture, 10.0, "b", 0, 0);

        // Next input item should be deferred, not shed.
        let (_item, decision) = sched.admit(LatencyStage::PtyCapture, 10.0, "c", 0, 0);
        assert_eq!(decision, AdmissionDecision::Deferred);
    }

    #[test]
    fn test_scheduler_bulk_queue_full_shed() {
        let cfg = LaneSchedulerConfig {
            bulk_queue_capacity: 2,
            input_pressure_threshold: 0.99, // Don't trigger pressure shedding.
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        sched.admit(LatencyStage::StorageWrite, 100.0, "b1", 0, 0);
        sched.admit(LatencyStage::StorageWrite, 100.0, "b2", 0, 0);

        // Queue full — bulk items shed.
        let (_item, decision) = sched.admit(LatencyStage::StorageWrite, 100.0, "b3", 0, 0);
        assert_eq!(decision, AdmissionDecision::Shed);
    }

    #[test]
    fn test_scheduler_deadline_promotion() {
        let cfg = LaneSchedulerConfig {
            enable_deadline_promotion: true,
            deadline_promotion_fraction: 0.25,
            input_pressure_threshold: 0.99,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        // Bulk item with tight deadline: now=900, deadline=1000, remaining=100 < 250 (25% of 1000).
        let (_item, decision) = sched.admit(
            LatencyStage::PatternDetection,
            50.0,
            "promoted-1",
            1000,
            900,
        );
        assert_eq!(
            decision,
            AdmissionDecision::Promoted {
                from: SchedulerLane::Bulk,
                to: SchedulerLane::Control,
            }
        );
        // Control queue should have the item.
        assert_eq!(sched.lane_state(SchedulerLane::Control).depth, 1);
    }

    #[test]
    fn test_scheduler_complete_decrements() {
        let mut sched = LaneScheduler::with_defaults();
        sched.admit(LatencyStage::PtyCapture, 100.0, "c1", 0, 0);
        assert_eq!(sched.lane_state(SchedulerLane::Input).depth, 1);

        sched.complete(SchedulerLane::Input, 95.0);
        assert_eq!(sched.lane_state(SchedulerLane::Input).depth, 0);
        assert_eq!(sched.lane_state(SchedulerLane::Input).total_completed, 1);
        assert!((sched.lane_state(SchedulerLane::Input).cpu_used_us - 95.0).abs() < 1e-6);
    }

    #[test]
    fn test_scheduler_begin_epoch_resets_cpu() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        let input = sched.lane_state(SchedulerLane::Input);
        assert!((input.cpu_budget_us - 5000.0).abs() < 1e-6); // 50% of 10000
        let control = sched.lane_state(SchedulerLane::Control);
        assert!((control.cpu_budget_us - 3000.0).abs() < 1e-6); // 30%
        let bulk = sched.lane_state(SchedulerLane::Bulk);
        assert!((bulk.cpu_budget_us - 2000.0).abs() < 1e-6); // 20%
    }

    #[test]
    fn test_scheduler_snapshot_serde() {
        let mut sched = LaneScheduler::with_defaults();
        sched.admit(LatencyStage::PtyCapture, 100.0, "snap", 0, 0);
        let snap = sched.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: SchedulerSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap.epoch, back.epoch);
        assert_eq!(snap.lanes.len(), back.lanes.len());
    }

    #[test]
    fn test_scheduler_status_line() {
        let sched = LaneScheduler::with_defaults();
        let s = sched.status_line();
        assert!(s.contains("scheduler"));
        assert!(s.contains("input=0/256"));
        assert!(s.contains("control=0/128"));
        assert!(s.contains("bulk=0/1024"));
    }

    #[test]
    fn test_scheduler_recent_events() {
        let mut sched = LaneScheduler::with_defaults();
        for i in 0..5 {
            sched.admit(LatencyStage::PtyCapture, 10.0, &format!("ev-{}", i), 0, 0);
        }
        let events = sched.recent_events(3);
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn test_admission_decision_display() {
        assert_eq!(format!("{}", AdmissionDecision::Admitted), "ADMITTED");
        assert_eq!(format!("{}", AdmissionDecision::Deferred), "DEFERRED");
        assert_eq!(format!("{}", AdmissionDecision::Shed), "SHED");
        assert_eq!(
            format!(
                "{}",
                AdmissionDecision::Promoted {
                    from: SchedulerLane::Bulk,
                    to: SchedulerLane::Control,
                }
            ),
            "PROMOTED bulk→control"
        );
    }

    #[test]
    fn test_lane_state_utilization() {
        let mut state = LaneState::new(SchedulerLane::Input, 100);
        assert_eq!(state.utilization(), 0.0);
        state.depth = 50;
        assert!((state.utilization() - 0.5).abs() < 1e-6);
        state.depth = 100;
        assert!((state.utilization() - 1.0).abs() < 1e-6);
        assert!(state.is_full());
    }

    #[test]
    fn test_default_stages_cover_all_pipeline() {
        let mut covered: Vec<LatencyStage> = Vec::new();
        for &lane in SchedulerLane::ALL {
            covered.extend_from_slice(lane.default_stages());
        }
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                covered.contains(&stage),
                "stage {} not covered by any lane",
                stage
            );
        }
    }

    #[test]
    fn test_work_item_serde() {
        let item = WorkItem {
            id: 42,
            lane: SchedulerLane::Input,
            stage: LatencyStage::PtyCapture,
            estimated_cost_us: 500.0,
            correlation_id: "serde-test".into(),
            deadline_us: 0,
        };
        let json = serde_json::to_string(&item).expect("serialize");
        let back: WorkItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item.id, back.id);
        assert_eq!(item.lane, back.lane);
    }

    #[test]
    fn test_scheduling_event_serde() {
        let event = SchedulingEvent {
            item_id: 1,
            lane: SchedulerLane::Bulk,
            stage: LatencyStage::StorageWrite,
            decision: AdmissionDecision::Shed,
            queue_depth_before: 1024,
            queue_depth_after: 1024,
            correlation_id: "shed-test".into(),
            reason_code: Some("QUEUE_OVERFLOW".into()),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let back: SchedulingEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event.item_id, back.item_id);
        assert_eq!(event.decision, back.decision);
    }

    // ── B1 Impl: CPU Budget, Fairness, Degradation ──

    #[test]
    fn test_scheduler_has_cpu_budget() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        assert!(sched.has_cpu_budget(SchedulerLane::Input));
        assert!((sched.remaining_cpu_us(SchedulerLane::Input) - 5000.0).abs() < 1e-6);
    }

    #[test]
    fn test_scheduler_cpu_budget_exhaustion() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "x", 0, 0);
        sched.complete(SchedulerLane::Input, 5001.0);
        assert!(!sched.has_cpu_budget(SchedulerLane::Input));
        assert_eq!(sched.remaining_cpu_us(SchedulerLane::Input), 0.0);
    }

    #[test]
    fn test_scheduler_next_lane_priority() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::StorageWrite, 100.0, "bulk", 0, 0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "input", 0, 0);
        assert_eq!(sched.next_lane(), Some(SchedulerLane::Input));
    }

    #[test]
    fn test_scheduler_next_lane_fallthrough() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::StorageWrite, 100.0, "bulk", 0, 0);
        assert_eq!(sched.next_lane(), Some(SchedulerLane::Bulk));
    }

    #[test]
    fn test_scheduler_next_lane_empty() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        assert_eq!(sched.next_lane(), None);
    }

    #[test]
    fn test_scheduler_fairness_ratios_no_work() {
        let sched = LaneScheduler::with_defaults();
        let ratios = sched.fairness_ratios();
        assert_eq!(ratios.len(), 3);
        for (_lane, ratio) in &ratios {
            assert!((*ratio - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn test_scheduler_fairness_ratios_with_work() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "f1", 0, 0);
        sched.complete(SchedulerLane::Input, 5000.0);
        let ratios = sched.fairness_ratios();
        let input_ratio = ratios
            .iter()
            .find(|(l, _)| *l == SchedulerLane::Input)
            .unwrap()
            .1;
        assert!((input_ratio - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_scheduler_degradation_healthy() {
        let sched = LaneScheduler::with_defaults();
        assert_eq!(sched.current_degradation(), SchedulerDegradation::Healthy);
        assert!(sched.is_healthy());
    }

    #[test]
    fn test_scheduler_degradation_display() {
        assert_eq!(format!("{}", SchedulerDegradation::Healthy), "HEALTHY");
        let inp = SchedulerDegradation::InputStarvation {
            depth: 10,
            deferred: 50,
        };
        assert!(format!("{}", inp).contains("INPUT_STARVATION"));
        let bulk = SchedulerDegradation::BulkStarvation {
            shed_count: 100,
            completed_count: 5,
        };
        assert!(format!("{}", bulk).contains("BULK_STARVATION"));
        let ctrl = SchedulerDegradation::ControlBacklog {
            depth: 70,
            capacity: 128,
        };
        assert!(format!("{}", ctrl).contains("CONTROL_BACKLOG"));
    }

    #[test]
    fn test_scheduler_log_entry() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "log", 0, 0);
        let entry = sched.log_entry();
        assert_eq!(entry.epoch, 1);
        assert_eq!(entry.depths.len(), 3);
        assert!(!entry.input_pressure);
    }

    #[test]
    fn test_scheduler_log_entry_serde() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        let entry = sched.log_entry();
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: SchedulerLogEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry.epoch, back.epoch);
        assert_eq!(entry.depths.len(), back.depths.len());
    }

    #[test]
    fn test_scheduler_degradation_serde() {
        let cases = vec![
            SchedulerDegradation::Healthy,
            SchedulerDegradation::InputStarvation {
                depth: 5,
                deferred: 20,
            },
            SchedulerDegradation::BulkStarvation {
                shed_count: 50,
                completed_count: 2,
            },
            SchedulerDegradation::ControlBacklog {
                depth: 70,
                capacity: 128,
            },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).expect("serialize");
            let back: SchedulerDegradation = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(case, back);
        }
    }

    // ── B2: Bounded Input Ring ──

    #[test]
    fn test_input_ring_basic_enqueue_dequeue() {
        let mut ring = InputRing::with_defaults();
        assert!(ring.is_empty());
        let seq = ring.enqueue(LatencyStage::PtyCapture, 100.0, "basic", 1000, 0).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(ring.len(), 1);
        let item = ring.dequeue(1100).unwrap();
        assert_eq!(item.seq, 1);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_input_ring_fifo_order() {
        let mut ring = InputRing::with_defaults();
        for i in 0..5 {
            ring.enqueue(LatencyStage::PtyCapture, 10.0, &format!("fifo-{}", i), i * 100, 0).unwrap();
        }
        for i in 0..5 {
            let item = ring.dequeue(1000).unwrap();
            assert_eq!(item.seq, i as u64 + 1);
        }
    }

    #[test]
    fn test_input_ring_full_rejects() {
        let cfg = InputRingConfig {
            capacity: 3,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "a", 0, 0).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "b", 0, 0).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "c", 0, 0).unwrap();
        assert!(ring.is_full());
        let result = ring.enqueue(LatencyStage::PtyCapture, 10.0, "d", 0, 0);
        assert_eq!(result, Err(RingBackpressure::Full));
    }

    #[test]
    fn test_input_ring_backpressure_signals() {
        let cfg = InputRingConfig {
            capacity: 4,
            high_water_mark: 0.75,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        assert_eq!(ring.backpressure(), RingBackpressure::Accept);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp1", 0, 0).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp2", 0, 0).unwrap();
        assert_eq!(ring.backpressure(), RingBackpressure::Accept);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp3", 0, 0).unwrap();
        // 3/4 = 0.75 >= high_water_mark → SlowDown
        assert_eq!(ring.backpressure(), RingBackpressure::SlowDown);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp4", 0, 0).unwrap();
        assert_eq!(ring.backpressure(), RingBackpressure::Full);
    }

    #[test]
    fn test_input_ring_wraparound() {
        let cfg = InputRingConfig {
            capacity: 3,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w1", 0, 0).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w2", 0, 0).unwrap();
        ring.dequeue(100).unwrap(); // remove w1
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w3", 0, 0).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w4", 0, 0).unwrap();
        assert_eq!(ring.len(), 3);
        // Should be w2, w3, w4 in FIFO order.
        assert_eq!(ring.dequeue(200).unwrap().seq, 2);
        assert_eq!(ring.dequeue(200).unwrap().seq, 3);
        assert_eq!(ring.dequeue(200).unwrap().seq, 4);
    }

    #[test]
    fn test_input_ring_peek() {
        let mut ring = InputRing::with_defaults();
        assert!(ring.peek().is_none());
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "peek", 100, 0).unwrap();
        let peeked = ring.peek().unwrap();
        assert_eq!(peeked.seq, 1);
        assert_eq!(ring.len(), 1); // Peek doesn't remove.
    }

    #[test]
    fn test_input_ring_sojourn_tracking() {
        let cfg = InputRingConfig {
            track_sojourn: true,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "soj", 1000, 0).unwrap();
        ring.dequeue(1500).unwrap(); // sojourn = 500us
        assert!((ring.mean_sojourn_us().unwrap() - 500.0).abs() < 1e-6);
    }

    #[test]
    fn test_input_ring_snapshot() {
        let mut ring = InputRing::with_defaults();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "snap", 100, 0).unwrap();
        let snap = ring.snapshot();
        assert_eq!(snap.len, 1);
        assert_eq!(snap.total_enqueued, 1);
        assert_eq!(snap.total_dequeued, 0);
        assert_eq!(snap.total_dropped, 0);
    }

    #[test]
    fn test_input_ring_snapshot_serde() {
        let ring = InputRing::with_defaults();
        let snap = ring.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: InputRingSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap.capacity, back.capacity);
        assert_eq!(snap.len, back.len);
    }

    #[test]
    fn test_input_ring_status_line() {
        let ring = InputRing::with_defaults();
        let s = ring.status_line();
        assert!(s.contains("input_ring"));
        assert!(s.contains("len=0/256"));
    }

    #[test]
    fn test_input_ring_accounting() {
        let cfg = InputRingConfig {
            capacity: 2,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "a", 0, 0).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "b", 0, 0).unwrap();
        let _ = ring.enqueue(LatencyStage::PtyCapture, 10.0, "c", 0, 0); // dropped
        ring.dequeue(100).unwrap();
        // Invariant: enqueued = dequeued + len (dropped are separate rejection count)
        assert_eq!(ring.total_enqueued, ring.total_dequeued + ring.len() as u64);
        assert_eq!(ring.total_dropped, 1);
    }

    #[test]
    fn test_ring_backpressure_display() {
        assert_eq!(format!("{}", RingBackpressure::Accept), "ACCEPT");
        assert_eq!(format!("{}", RingBackpressure::SlowDown), "SLOW_DOWN");
        assert_eq!(format!("{}", RingBackpressure::Full), "FULL");
    }

    #[test]
    fn test_input_ring_item_serde() {
        let item = InputRingItem {
            seq: 42,
            stage: LatencyStage::PtyCapture,
            estimated_cost_us: 100.0,
            correlation_id: "serde-item".into(),
            arrived_us: 1000,
            deadline_us: 0,
        };
        let json = serde_json::to_string(&item).expect("serialize");
        let back: InputRingItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item.seq, back.seq);
        assert_eq!(item.stage, back.stage);
    }

    // ── B2 Impl: Drain, Expiry, Utilization ──

    #[test]
    fn test_input_ring_drain() {
        let mut ring = InputRing::with_defaults();
        for i in 0..10 {
            ring.enqueue(LatencyStage::PtyCapture, 10.0, &format!("d-{}", i), i * 100, 0)
                .unwrap();
        }
        let items = ring.drain(5, 2000);
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].seq, 1);
        assert_eq!(items[4].seq, 5);
        assert_eq!(ring.len(), 5);
    }

    #[test]
    fn test_input_ring_drain_more_than_available() {
        let mut ring = InputRing::with_defaults();
        for i in 0..3 {
            ring.enqueue(LatencyStage::PtyCapture, 10.0, &format!("dm-{}", i), 0, 0)
                .unwrap();
        }
        let items = ring.drain(100, 1000);
        assert_eq!(items.len(), 3);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_input_ring_drain_expired() {
        let mut ring = InputRing::with_defaults();
        // Item with deadline=500, item with deadline=2000, item with no deadline.
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "exp", 100, 500).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "ok", 200, 2000).unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "nodeadline", 300, 0).unwrap();

        let expired = ring.drain_expired(1000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].correlation_id, "exp");
        // Remaining ring should have 2 items.
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn test_input_ring_utilization() {
        let cfg = InputRingConfig {
            capacity: 10,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        assert!((ring.utilization() - 0.0).abs() < 1e-6);
        for i in 0..5 {
            ring.enqueue(LatencyStage::PtyCapture, 10.0, &format!("u-{}", i), 0, 0)
                .unwrap();
        }
        assert!((ring.utilization() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_input_ring_capacity() {
        let cfg = InputRingConfig {
            capacity: 42,
            ..Default::default()
        };
        let ring = InputRing::new(cfg);
        assert_eq!(ring.capacity(), 42);
    }

    // ── B3: Priority Inheritance & Lock-Order ──

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Critical > Priority::Elevated);
        assert!(Priority::Elevated > Priority::Normal);
        assert!(Priority::Normal > Priority::Background);
    }

    #[test]
    fn test_priority_display() {
        assert_eq!(format!("{}", Priority::Critical), "CRITICAL");
        assert_eq!(format!("{}", Priority::Background), "BACKGROUND");
    }

    #[test]
    fn test_priority_all_covers_four() {
        assert_eq!(Priority::ALL.len(), 4);
    }

    #[test]
    fn test_stage_to_priority_mapping() {
        assert_eq!(stage_to_priority(LatencyStage::PtyCapture), Priority::Critical);
        assert_eq!(stage_to_priority(LatencyStage::DeltaExtraction), Priority::Critical);
        assert_eq!(stage_to_priority(LatencyStage::EventEmission), Priority::Elevated);
        assert_eq!(stage_to_priority(LatencyStage::StorageWrite), Priority::Background);
    }

    #[test]
    fn test_resource_lock_order_is_canonical() {
        for w in Resource::LOCK_ORDER.windows(2) {
            assert!(w[0].order_index() < w[1].order_index());
        }
    }

    #[test]
    fn test_resource_display() {
        assert_eq!(format!("{}", Resource::StorageLock), "storage");
        assert_eq!(format!("{}", Resource::WorkflowLock), "workflow");
    }

    #[test]
    fn test_pi_acquire_free_lock() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 100);
        assert_eq!(result, LockResult::Acquired);
        assert!(tracker.is_held_by(Resource::StorageLock, "task-1"));
    }

    #[test]
    fn test_pi_reentrant_acquire() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 100);
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 200);
        assert_eq!(result, LockResult::Acquired);
    }

    #[test]
    fn test_pi_inheritance_on_contention() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::PatternLock, "low", Priority::Background, 100);

        let result = tracker.acquire(Resource::PatternLock, "high", Priority::Critical, 200);
        match result {
            LockResult::AcquiredAfterInheritance { boosted_holder } => {
                assert_eq!(boosted_holder, "low");
            }
            other => panic!("Expected AcquiredAfterInheritance, got {:?}", other),
        }

        // The holder's effective priority should now be Critical.
        assert_eq!(
            tracker.effective_priority("low"),
            Some(Priority::Critical)
        );
    }

    #[test]
    fn test_pi_release_reverts_priority() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "low", Priority::Background, 100);
        tracker.acquire(Resource::StorageLock, "high", Priority::Critical, 200);

        let promoted = tracker.release(Resource::StorageLock, "low", 300);
        assert_eq!(promoted, vec!["high".to_string()]);
        assert!(tracker.is_held_by(Resource::StorageLock, "high"));
        assert!(!tracker.is_held_by(Resource::StorageLock, "low"));
    }

    #[test]
    fn test_pi_lock_order_violation() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        // Acquire WorkflowLock (index 3) first.
        tracker.acquire(Resource::WorkflowLock, "task-1", Priority::Normal, 100);

        // Try to acquire StorageLock (index 0) — violates canonical order.
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 200);
        match result {
            LockResult::OrderViolation { requested, held_after } => {
                assert_eq!(requested, Resource::StorageLock);
                assert_eq!(held_after, Resource::WorkflowLock);
            }
            other => panic!("Expected OrderViolation, got {:?}", other),
        }
    }

    #[test]
    fn test_pi_lock_order_valid_ascending() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let r1 = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 100);
        assert_eq!(r1, LockResult::Acquired);
        let r2 = tracker.acquire(Resource::PatternLock, "task-1", Priority::Normal, 200);
        assert_eq!(r2, LockResult::Acquired);
        let r3 = tracker.acquire(Resource::EventBusLock, "task-1", Priority::Normal, 300);
        assert_eq!(r3, LockResult::Acquired);

        assert!(tracker.check_lock_order("task-1").is_empty());
    }

    #[test]
    fn test_pi_snapshot_reflects_state() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        tracker.acquire(Resource::StorageLock, "t2", Priority::Critical, 200);

        let snap = tracker.snapshot();
        assert_eq!(snap.held_locks.len(), 1);
        assert_eq!(snap.total_inheritance_events, 1);
        assert_eq!(snap.active_chains, 1);
    }

    #[test]
    fn test_pi_status_line() {
        let tracker = PriorityInheritanceTracker::with_defaults();
        let line = tracker.status_line();
        assert!(line.contains("pi_tracker"));
        assert!(line.contains("held=0"));
    }

    #[test]
    fn test_pi_release_nonexistent() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let promoted = tracker.release(Resource::StorageLock, "nobody", 100);
        assert!(promoted.is_empty());
    }

    #[test]
    fn test_pi_release_wrong_holder() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "owner", Priority::Normal, 100);
        let promoted = tracker.release(Resource::StorageLock, "impostor", 200);
        assert!(promoted.is_empty());
        assert!(tracker.is_held_by(Resource::StorageLock, "owner"));
    }

    #[test]
    fn test_pi_waiter_promotion_order() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::PatternLock, "holder", Priority::Background, 100);
        tracker.acquire(Resource::PatternLock, "low", Priority::Normal, 200);
        tracker.acquire(Resource::PatternLock, "high", Priority::Critical, 300);

        // Release: highest priority waiter (high) should be promoted first.
        let promoted = tracker.release(Resource::PatternLock, "holder", 400);
        assert_eq!(promoted, vec!["high".to_string()]);
        assert!(tracker.is_held_by(Resource::PatternLock, "high"));
    }

    #[test]
    fn test_pi_effective_priority_across_locks() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Background, 100);
        tracker.acquire(Resource::PatternLock, "t1", Priority::Normal, 200);

        // Effective priority should be the max across all held locks.
        assert_eq!(
            tracker.effective_priority("t1"),
            Some(Priority::Normal)
        );
    }

    #[test]
    fn test_inheritance_event_serde() {
        let event = InheritanceEvent {
            holder_id: "h".to_string(),
            waiter_id: "w".to_string(),
            resource: Resource::StorageLock,
            original_priority: Priority::Background,
            inherited_priority: Priority::Critical,
            applied_us: 100,
            released_us: Some(200),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: InheritanceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn test_lock_result_serde() {
        let results = vec![
            LockResult::Acquired,
            LockResult::AcquiredAfterInheritance {
                boosted_holder: "x".to_string(),
            },
            LockResult::OrderViolation {
                requested: Resource::StorageLock,
                held_after: Resource::WorkflowLock,
            },
        ];
        for r in &results {
            let json = serde_json::to_string(r).unwrap();
            let back: LockResult = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_priority_serde() {
        for p in &Priority::ALL {
            let json = serde_json::to_string(p).unwrap();
            let back: Priority = serde_json::from_str(&json).unwrap();
            assert_eq!(*p, back);
        }
    }

    #[test]
    fn test_resource_serde() {
        for r in &Resource::LOCK_ORDER {
            let json = serde_json::to_string(r).unwrap();
            let back: Resource = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_inheritance_snapshot_serde() {
        let snap = InheritanceSnapshot {
            held_locks: vec![],
            total_inheritance_events: 5,
            total_order_violations: 2,
            active_chains: 1,
            max_chain_depth_observed: 3,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: InheritanceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_pi_config_default() {
        let cfg = PriorityInheritanceConfig::default();
        assert_eq!(cfg.max_chain_depth, 4);
        assert!(cfg.enforce_lock_order);
        assert_eq!(cfg.max_inheritance_duration_us, 50_000);
    }

    #[test]
    fn test_pi_no_order_violation_when_disabled() {
        let config = PriorityInheritanceConfig {
            enforce_lock_order: false,
            ..Default::default()
        };
        let mut tracker = PriorityInheritanceTracker::new(config);
        tracker.acquire(Resource::WorkflowLock, "task-1", Priority::Normal, 100);
        // With lock-order enforcement disabled, this should succeed.
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 200);
        assert_eq!(result, LockResult::Acquired);
    }

    // ── B3 Impl: Bridge methods ──

    #[test]
    fn test_pi_release_all() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        tracker.acquire(Resource::PatternLock, "t1", Priority::Normal, 200);
        tracker.acquire(Resource::EventBusLock, "t1", Priority::Normal, 300);

        let released = tracker.release_all("t1", 400);
        assert_eq!(released.len(), 3);
        assert_eq!(tracker.held_count(), 0);
    }

    #[test]
    fn test_pi_expire_stale_inheritance() {
        let config = PriorityInheritanceConfig {
            max_inheritance_duration_us: 100,
            ..Default::default()
        };
        let mut tracker = PriorityInheritanceTracker::new(config);
        tracker.acquire(Resource::StorageLock, "low", Priority::Background, 0);
        tracker.acquire(Resource::StorageLock, "high", Priority::Critical, 50);

        // Before expiry.
        assert_eq!(
            tracker.effective_priority("low"),
            Some(Priority::Critical)
        );

        // After expiry (200us > 100us max).
        let expired = tracker.expire_stale_inheritance(200);
        assert_eq!(expired, 1);
        assert_eq!(
            tracker.effective_priority("low"),
            Some(Priority::Background)
        );
    }

    #[test]
    fn test_pi_held_count() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        assert_eq!(tracker.held_count(), 0);
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        assert_eq!(tracker.held_count(), 1);
        tracker.acquire(Resource::PatternLock, "t2", Priority::Normal, 200);
        assert_eq!(tracker.held_count(), 2);
    }

    #[test]
    fn test_pi_total_waiters() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "holder", Priority::Background, 100);
        tracker.acquire(Resource::StorageLock, "w1", Priority::Normal, 200);
        tracker.acquire(Resource::StorageLock, "w2", Priority::Elevated, 300);
        assert_eq!(tracker.total_waiters(), 2);
    }

    #[test]
    fn test_pi_degradation_healthy() {
        let tracker = PriorityInheritanceTracker::with_defaults();
        assert_eq!(tracker.detect_degradation(), InheritanceDegradation::Healthy);
    }

    #[test]
    fn test_pi_degradation_excessive_inheritance() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        // Create 3 locks each with inheritance (>2 threshold).
        for (i, resource) in [Resource::StorageLock, Resource::PatternLock, Resource::EventBusLock].iter().enumerate() {
            tracker.acquire(*resource, &format!("low-{}", i), Priority::Background, i as u64 * 100);
            tracker.acquire(*resource, &format!("high-{}", i), Priority::Critical, i as u64 * 100 + 50);
        }
        let degradation = tracker.detect_degradation();
        let is_excessive = matches!(degradation, InheritanceDegradation::ExcessiveInheritance { .. });
        assert!(is_excessive, "Expected ExcessiveInheritance, got {:?}", degradation);
    }

    #[test]
    fn test_pi_degradation_order_violation_spike() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        // Generate >10 order violations.
        for _ in 0..11 {
            tracker.acquire(Resource::WorkflowLock, "task", Priority::Normal, 100);
            let _ = tracker.acquire(Resource::StorageLock, "task", Priority::Normal, 200);
            tracker.release(Resource::WorkflowLock, "task", 300);
        }
        let degradation = tracker.detect_degradation();
        let is_spike = matches!(degradation, InheritanceDegradation::OrderViolationSpike { .. });
        assert!(is_spike, "Expected OrderViolationSpike, got {:?}", degradation);
    }

    #[test]
    fn test_pi_log_entry() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        let entry = tracker.log_entry(500);
        assert_eq!(entry.timestamp_us, 500);
        assert_eq!(entry.held_locks, 1);
        assert_eq!(entry.degradation, InheritanceDegradation::Healthy);
    }

    #[test]
    fn test_inheritance_degradation_serde() {
        let variants = vec![
            InheritanceDegradation::Healthy,
            InheritanceDegradation::ExcessiveInheritance { active_chains: 3, threshold: 2 },
            InheritanceDegradation::HighContention { total_waiters: 10, threshold: 8 },
            InheritanceDegradation::OrderViolationSpike { total_violations: 15, threshold: 10 },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: InheritanceDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_inheritance_log_entry_serde() {
        let entry = InheritanceLogEntry {
            timestamp_us: 1000,
            held_locks: 2,
            total_inheritance_events: 5,
            total_order_violations: 1,
            active_chains: 1,
            degradation: InheritanceDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: InheritanceLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_inheritance_degradation_display() {
        assert_eq!(format!("{}", InheritanceDegradation::Healthy), "HEALTHY");
        let exc = InheritanceDegradation::ExcessiveInheritance { active_chains: 3, threshold: 2 };
        assert!(format!("{}", exc).contains("3/2"));
    }

    // ── B4: Starvation Prevention & Fairness ──

    #[test]
    fn test_starvation_config_default() {
        let cfg = StarvationConfig::default();
        assert_eq!(cfg.max_starved_epochs, 5);
        assert_eq!(cfg.fairness_window, 20);
        assert!(cfg.enable_aging);
    }

    #[test]
    fn test_starvation_tracker_initial_state() {
        let tracker = StarvationTracker::with_defaults();
        assert_eq!(tracker.epoch(), 0);
        assert!(!tracker.any_starving());
        let snap = tracker.snapshot();
        assert_eq!(snap.lanes.len(), 3);
        assert_eq!(snap.total_starvation_events, 0);
    }

    #[test]
    fn test_starvation_no_starvation_when_all_served() {
        let mut tracker = StarvationTracker::with_defaults();
        for _ in 0..10 {
            let promoted = tracker.observe_epoch(&[5, 3, 2], &[0.5, 0.3, 0.2]);
            assert!(promoted.is_empty());
        }
        assert!(!tracker.any_starving());
    }

    #[test]
    fn test_starvation_detected_after_threshold() {
        let config = StarvationConfig {
            max_starved_epochs: 3,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        // Bulk lane gets zero completions for 3 epochs.
        for i in 0..3 {
            let promoted = tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
            if i < 2 {
                assert!(promoted.is_empty());
            } else {
                assert_eq!(promoted, vec![SchedulerLane::Bulk]);
            }
        }
        assert!(tracker.any_starving());
        assert!(tracker.lane_state(SchedulerLane::Bulk).force_promoted);
    }

    #[test]
    fn test_starvation_clears_on_completion() {
        let config = StarvationConfig {
            max_starved_epochs: 2,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        // Starve bulk for 2 epochs.
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        assert!(tracker.any_starving());

        // Bulk gets completions — starvation clears.
        tracker.observe_epoch(&[5, 3, 1], &[0.4, 0.3, 0.1]);
        assert!(!tracker.lane_state(SchedulerLane::Bulk).force_promoted);
    }

    #[test]
    fn test_gini_coefficient_equal_shares() {
        let mut tracker = StarvationTracker::with_defaults();
        // Equal shares → Gini ~= 0.
        for _ in 0..5 {
            tracker.observe_epoch(&[3, 3, 3], &[0.333, 0.333, 0.334]);
        }
        let gini = tracker.gini_coefficient();
        assert!(gini < 0.01, "Gini {} should be near 0 for equal shares", gini);
    }

    #[test]
    fn test_gini_coefficient_unequal_shares() {
        let mut tracker = StarvationTracker::with_defaults();
        // Very unequal shares → higher Gini.
        for _ in 0..5 {
            tracker.observe_epoch(&[10, 0, 0], &[0.9, 0.05, 0.05]);
        }
        let gini = tracker.gini_coefficient();
        assert!(gini > 0.3, "Gini {} should be higher for unequal shares", gini);
    }

    #[test]
    fn test_starvation_snapshot_serde() {
        let snap = FairnessSnapshot {
            lanes: vec![LaneFairnessState {
                lane: SchedulerLane::Input,
                starved_epochs: 0,
                windowed_share: 0.5,
                windowed_completions: 10,
                windowed_deferred: 2,
                force_promoted: false,
            }],
            gini_coefficient: 0.15,
            total_starvation_events: 3,
            any_starving: false,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: FairnessSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.total_starvation_events, back.total_starvation_events);
        assert_eq!(snap.any_starving, back.any_starving);
        assert_eq!(snap.lanes.len(), back.lanes.len());
    }

    #[test]
    fn test_starvation_event_serde() {
        let event = StarvationEvent {
            epoch: 10,
            lane: SchedulerLane::Bulk,
            starved_epochs: 5,
            cpu_share: 0.01,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: StarvationEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn test_starvation_status_line() {
        let tracker = StarvationTracker::with_defaults();
        let line = tracker.status_line();
        assert!(line.contains("fairness"));
        assert!(line.contains("gini="));
        assert!(line.contains("epoch=0"));
    }

    #[test]
    fn test_starvation_config_serde() {
        let cfg = StarvationConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: StarvationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn test_lane_fairness_state_serde() {
        let state = LaneFairnessState {
            lane: SchedulerLane::Control,
            starved_epochs: 2,
            windowed_share: 0.3,
            windowed_completions: 5,
            windowed_deferred: 1,
            force_promoted: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: LaneFairnessState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn test_starvation_epoch_monotonic() {
        let mut tracker = StarvationTracker::with_defaults();
        for i in 1..=5 {
            tracker.observe_epoch(&[1, 1, 1], &[0.33, 0.33, 0.34]);
            assert_eq!(tracker.epoch(), i);
        }
    }

    #[test]
    fn test_starvation_multiple_lanes_starve() {
        let config = StarvationConfig {
            max_starved_epochs: 2,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        // Both Control and Bulk starve.
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        let promoted = tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        assert_eq!(promoted.len(), 2);
        assert!(promoted.contains(&SchedulerLane::Control));
        assert!(promoted.contains(&SchedulerLane::Bulk));
    }

    // ── B4 Impl: Bridge methods ──

    #[test]
    fn test_starvation_reset() {
        let config = StarvationConfig {
            max_starved_epochs: 2,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        assert!(tracker.any_starving());

        tracker.reset();
        assert_eq!(tracker.epoch(), 0);
        assert!(!tracker.any_starving());
        assert_eq!(tracker.snapshot().total_starvation_events, 0);
    }

    #[test]
    fn test_starvation_recent_events() {
        let config = StarvationConfig {
            max_starved_epochs: 1,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        let recent = tracker.recent_events(10);
        assert_eq!(recent.len(), 2); // Control and Bulk both starved.
    }

    #[test]
    fn test_starvation_is_force_promoted() {
        let config = StarvationConfig {
            max_starved_epochs: 1,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        assert!(!tracker.is_force_promoted(SchedulerLane::Bulk));
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        assert!(tracker.is_force_promoted(SchedulerLane::Bulk));
        assert!(!tracker.is_force_promoted(SchedulerLane::Input));
    }

    #[test]
    fn test_fairness_degradation_healthy() {
        let tracker = StarvationTracker::with_defaults();
        assert_eq!(tracker.detect_degradation(), FairnessDegradation::Healthy);
    }

    #[test]
    fn test_fairness_degradation_starvation() {
        let config = StarvationConfig {
            max_starved_epochs: 1,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        let degradation = tracker.detect_degradation();
        let is_starvation = matches!(degradation, FairnessDegradation::LaneStarvation { .. });
        assert!(is_starvation, "Expected LaneStarvation, got {:?}", degradation);
    }

    #[test]
    fn test_fairness_log_entry() {
        let mut tracker = StarvationTracker::with_defaults();
        tracker.observe_epoch(&[5, 3, 2], &[0.5, 0.3, 0.2]);
        let entry = tracker.log_entry();
        assert_eq!(entry.epoch, 1);
        assert_eq!(entry.shares.len(), 3);
        assert_eq!(entry.starved_epochs.len(), 3);
    }

    #[test]
    fn test_fairness_degradation_serde() {
        let variants = vec![
            FairnessDegradation::Healthy,
            FairnessDegradation::LaneStarvation { starving_lanes: vec![SchedulerLane::Bulk] },
            FairnessDegradation::SevereUnfairness { gini: 0.7, threshold: 0.5 },
            FairnessDegradation::PromotionStorm { events_in_window: 10, threshold: 5 },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: FairnessDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_fairness_log_entry_serde() {
        let entry = FairnessLogEntry {
            epoch: 5,
            shares: vec![0.5, 0.3, 0.2],
            starved_epochs: vec![0, 0, 0],
            gini_coefficient: 0.1,
            any_starving: false,
            degradation: FairnessDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: FairnessLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_fairness_degradation_display() {
        assert_eq!(format!("{}", FairnessDegradation::Healthy), "HEALTHY");
        let storm = FairnessDegradation::PromotionStorm { events_in_window: 10, threshold: 5 };
        assert!(format!("{}", storm).contains("10/5"));
    }

    // ── C1: Memory Ownership Graph & Pool ──

    #[test]
    fn test_memory_domain_all_covers_eight() {
        assert_eq!(MemoryDomain::ALL.len(), 8);
    }

    #[test]
    fn test_memory_domain_display() {
        assert_eq!(format!("{}", MemoryDomain::PtyCapture), "pty_capture");
        assert_eq!(format!("{}", MemoryDomain::Shared), "shared");
    }

    #[test]
    fn test_stage_to_domain_mapping() {
        assert_eq!(stage_to_domain(LatencyStage::PtyCapture), MemoryDomain::PtyCapture);
        assert_eq!(stage_to_domain(LatencyStage::StorageWrite), MemoryDomain::StorageWrite);
        assert_eq!(stage_to_domain(LatencyStage::EventEmission), MemoryDomain::EventBus);
        assert_eq!(stage_to_domain(LatencyStage::ApiResponse), MemoryDomain::Shared);
    }

    #[test]
    fn test_pool_alloc_from_free_list() {
        let mut pool = MemoryPool::with_defaults();
        let result = pool.allocate();
        let is_from_free = matches!(result, AllocResult::FromFreeList { .. });
        assert!(is_from_free, "Expected FromFreeList, got {:?}", result);
        assert_eq!(pool.in_use(), 1);
    }

    #[test]
    fn test_pool_alloc_grow() {
        let config = PoolConfig {
            initial_blocks: 0,
            max_blocks: 10,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        let result = pool.allocate();
        let is_grown = matches!(result, AllocResult::Grown { .. });
        assert!(is_grown, "Expected Grown, got {:?}", result);
    }

    #[test]
    fn test_pool_alloc_exhausted() {
        let config = PoolConfig {
            initial_blocks: 1,
            max_blocks: 1,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        let result = pool.allocate();
        assert_eq!(result, AllocResult::PoolExhausted);
    }

    #[test]
    fn test_pool_free_returns_to_free_list() {
        let mut pool = MemoryPool::with_defaults();
        let block_id = match pool.allocate() {
            AllocResult::FromFreeList { block_id } => block_id,
            other => panic!("Expected FromFreeList, got {:?}", other),
        };
        assert_eq!(pool.in_use(), 1);
        pool.free(block_id);
        assert_eq!(pool.in_use(), 0);
        assert_eq!(pool.free_count(), 64); // 64 initial - 1 alloc + 1 free
    }

    #[test]
    fn test_pool_utilization() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 4,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        assert!((pool.utilization() - 0.0).abs() < 1e-10);
        pool.allocate();
        assert!((pool.utilization() - 0.25).abs() < 1e-10);
        pool.allocate();
        assert!((pool.utilization() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_pool_under_pressure() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 4,
            high_water_mark: 0.75,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        pool.allocate();
        assert!(!pool.under_pressure()); // 50% < 75%
        pool.allocate();
        assert!(pool.under_pressure()); // 75% >= 75%
    }

    #[test]
    fn test_pool_snapshot_invariant() {
        let mut pool = MemoryPool::with_defaults();
        pool.allocate();
        pool.allocate();
        let snap = pool.snapshot();
        assert_eq!(snap.in_use + snap.free_count, snap.total_blocks);
        assert_eq!(snap.total_allocs, snap.total_frees + snap.in_use as u64);
    }

    #[test]
    fn test_pool_status_line() {
        let pool = MemoryPool::with_defaults();
        let line = pool.status_line();
        assert!(line.contains("pool[shared]"));
        assert!(line.contains("0/64"));
    }

    #[test]
    fn test_pool_config_default() {
        let cfg = PoolConfig::default();
        assert_eq!(cfg.block_size, 4096);
        assert_eq!(cfg.initial_blocks, 64);
        assert_eq!(cfg.max_blocks, 1024);
    }

    #[test]
    fn test_pool_snapshot_serde() {
        let snap = PoolSnapshot {
            domain: MemoryDomain::PtyCapture,
            block_size: 4096,
            total_blocks: 64,
            in_use: 10,
            free_count: 54,
            max_blocks: 1024,
            total_allocs: 20,
            total_frees: 10,
            total_exhausted: 0,
            utilization: 0.15625,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PoolSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.domain, back.domain);
        assert_eq!(snap.in_use, back.in_use);
        assert_eq!(snap.total_allocs, back.total_allocs);
    }

    #[test]
    fn test_alloc_result_serde() {
        let results = vec![
            AllocResult::FromFreeList { block_id: 42 },
            AllocResult::Grown { block_id: 99 },
            AllocResult::PoolExhausted,
        ];
        for r in &results {
            let json = serde_json::to_string(r).unwrap();
            let back: AllocResult = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_memory_domain_serde() {
        for d in &MemoryDomain::ALL {
            let json = serde_json::to_string(d).unwrap();
            let back: MemoryDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(*d, back);
        }
    }

    #[test]
    fn test_pool_config_serde() {
        let cfg = PoolConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: PoolConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    // ── C1 Impl: Pool bridge methods ──

    #[test]
    fn test_pool_shrink() {
        let mut pool = MemoryPool::with_defaults();
        assert_eq!(pool.free_count(), 64);
        let reclaimed = pool.shrink(10);
        assert_eq!(reclaimed, 54);
        assert_eq!(pool.free_count(), 10);
        assert_eq!(pool.total_blocks(), 10);
    }

    #[test]
    fn test_pool_shrink_no_excess() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 10,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        let reclaimed = pool.shrink(10);
        assert_eq!(reclaimed, 0);
    }

    #[test]
    fn test_pool_reset() {
        let mut pool = MemoryPool::with_defaults();
        pool.allocate();
        pool.allocate();
        pool.allocate();
        assert_eq!(pool.in_use(), 3);

        pool.reset();
        assert_eq!(pool.in_use(), 0);
        assert_eq!(pool.total_blocks(), 64);
        assert_eq!(pool.free_count(), 64);
    }

    #[test]
    fn test_pool_degradation_healthy() {
        let pool = MemoryPool::with_defaults();
        assert_eq!(pool.detect_degradation(), PoolDegradation::Healthy);
    }

    #[test]
    fn test_pool_degradation_exhausted() {
        let config = PoolConfig {
            initial_blocks: 1,
            max_blocks: 1,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        pool.allocate(); // exhausted
        let degradation = pool.detect_degradation();
        let is_exhausted = matches!(degradation, PoolDegradation::Exhausted { .. });
        assert!(is_exhausted, "Expected Exhausted, got {:?}", degradation);
    }

    #[test]
    fn test_pool_degradation_high_util() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 4,
            high_water_mark: 0.5,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        pool.allocate();
        pool.allocate();
        let degradation = pool.detect_degradation();
        let is_high = matches!(degradation, PoolDegradation::HighUtilization { .. });
        assert!(is_high, "Expected HighUtilization, got {:?}", degradation);
    }

    #[test]
    fn test_pool_log_entry() {
        let mut pool = MemoryPool::with_defaults();
        pool.allocate();
        let entry = pool.log_entry();
        assert_eq!(entry.domain, MemoryDomain::Shared);
        assert_eq!(entry.in_use, 1);
        assert_eq!(entry.degradation, PoolDegradation::Healthy);
    }

    #[test]
    fn test_pool_degradation_serde() {
        let variants = vec![
            PoolDegradation::Healthy,
            PoolDegradation::HighUtilization { utilization: 0.9, threshold: 0.85 },
            PoolDegradation::Exhausted { total_exhausted: 5 },
            PoolDegradation::Fragmented { total_blocks: 100, free_count: 60 },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: PoolDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_pool_log_entry_serde() {
        let entry = PoolLogEntry {
            domain: MemoryDomain::PtyCapture,
            utilization: 0.5,
            in_use: 32,
            total_blocks: 64,
            degradation: PoolDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: PoolLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_pool_degradation_display() {
        assert_eq!(format!("{}", PoolDegradation::Healthy), "HEALTHY");
        let exhausted = PoolDegradation::Exhausted { total_exhausted: 5 };
        assert!(format!("{}", exhausted).contains("5"));
    }

    // ── C2: Zero-Copy Ingestion Parser ──

    #[test]
    fn test_ingest_parser_complete_line() {
        let mut parser = IngestParser::with_defaults();
        let result = parser.feed(b"hello world\n");
        assert_eq!(
            result,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 12,
            }
        );
        assert_eq!(parser.snapshot().total_lines, 1);
    }

    #[test]
    fn test_ingest_parser_partial_then_complete() {
        let mut parser = IngestParser::with_defaults();
        let r1 = parser.feed(b"hello ");
        let is_partial = matches!(r1, ParseResult::Partial { .. });
        assert!(is_partial);

        let r2 = parser.feed(b"world\n");
        assert_eq!(
            r2,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 12,
            }
        );
    }

    #[test]
    fn test_ingest_parser_multiple_lines() {
        let mut parser = IngestParser::with_defaults();
        let result = parser.feed(b"line1\nline2\nline3\n");
        assert_eq!(
            result,
            ParseResult::Complete {
                lines: 3,
                bytes_consumed: 18,
            }
        );
    }

    #[test]
    fn test_ingest_parser_zero_copy_ratio() {
        let mut parser = IngestParser::with_defaults();
        // Feed complete line — zero copy.
        parser.feed(b"complete line\n");
        let ratio = parser.zero_copy_ratio();
        assert!((ratio - 1.0).abs() < 1e-10, "Expected 1.0, got {}", ratio);
    }

    #[test]
    fn test_ingest_parser_flush() {
        let mut parser = IngestParser::with_defaults();
        parser.feed(b"incomplete");
        let result = parser.flush();
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(
            r,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 10,
            }
        );
        assert_eq!(parser.buffered_bytes(), 0);
    }

    #[test]
    fn test_ingest_parser_flush_empty() {
        let mut parser = IngestParser::with_defaults();
        assert!(parser.flush().is_none());
    }

    #[test]
    fn test_ingest_parser_max_line_reject() {
        let config = IngestParserConfig {
            max_line_bytes: 10,
            ..Default::default()
        };
        let mut parser = IngestParser::new(config);
        let result = parser.feed(b"this is a very long line without newline");
        let is_invalid = matches!(result, ParseResult::Invalid { .. });
        assert!(is_invalid, "Expected Invalid, got {:?}", result);
    }

    #[test]
    fn test_ingest_parser_snapshot_serde() {
        let snap = IngestParserSnapshot {
            total_bytes: 1000,
            total_lines: 50,
            total_chunks: 10,
            total_invalid_bytes: 0,
            buffered_bytes: 5,
            zero_copy_ratio: 0.8,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: IngestParserSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.total_bytes, back.total_bytes);
        assert_eq!(snap.total_lines, back.total_lines);
    }

    #[test]
    fn test_ingest_parser_config_serde() {
        let cfg = IngestParserConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: IngestParserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn test_ingest_parser_config_default() {
        let cfg = IngestParserConfig::default();
        assert_eq!(cfg.max_line_bytes, 16384);
        assert_eq!(cfg.max_buffered_chunks, 64);
        assert!(cfg.checksum);
    }

    #[test]
    fn test_parse_result_serde() {
        let results = vec![
            ParseResult::Complete {
                lines: 3,
                bytes_consumed: 30,
            },
            ParseResult::Partial { bytes_buffered: 10 },
            ParseResult::Invalid {
                bytes_skipped: 5,
                reason: "corrupt".to_string(),
            },
        ];
        for r in &results {
            let json = serde_json::to_string(r).unwrap();
            let back: ParseResult = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_ingest_chunk_serde() {
        let chunk = IngestChunk {
            pane_id: 1,
            offset: 100,
            length: 50,
            line_aligned: true,
            captured_us: 5000,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let back: IngestChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(chunk, back);
    }

    #[test]
    fn test_ingest_parser_status_line() {
        let parser = IngestParser::with_defaults();
        let line = parser.status_line();
        assert!(line.contains("ingest"));
        assert!(line.contains("bytes=0"));
    }

    #[test]
    fn test_memchr_newline() {
        assert_eq!(memchr_newline(b"hello\nworld"), Some(5));
        assert_eq!(memchr_newline(b"no newline"), None);
        assert_eq!(memchr_newline(b"\n"), Some(0));
    }

    #[test]
    fn test_count_newlines() {
        assert_eq!(count_newlines(b"a\nb\nc\n"), 3);
        assert_eq!(count_newlines(b"no newlines"), 0);
    }

    #[test]
    fn test_ingest_parser_line_with_remainder() {
        let mut parser = IngestParser::with_defaults();
        let result = parser.feed(b"line1\npartial");
        assert_eq!(
            result,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 6,
            }
        );
        assert_eq!(parser.buffered_bytes(), 7); // "partial"
    }

    // ── C2 Impl: Parser bridge methods ──

    #[test]
    fn test_ingest_parser_reset() {
        let mut parser = IngestParser::with_defaults();
        parser.feed(b"hello\n");
        parser.feed(b"world");
        assert!(parser.total_bytes() > 0);

        parser.reset();
        assert_eq!(parser.total_bytes(), 0);
        assert_eq!(parser.total_lines(), 0);
        assert_eq!(parser.total_chunks(), 0);
        assert_eq!(parser.buffered_bytes(), 0);
    }

    #[test]
    fn test_ingest_degradation_healthy() {
        let parser = IngestParser::with_defaults();
        assert_eq!(parser.detect_degradation(), IngestDegradation::Healthy);
    }

    #[test]
    fn test_ingest_degradation_high_buffer() {
        let config = IngestParserConfig {
            max_line_bytes: 20,
            ..Default::default()
        };
        let mut parser = IngestParser::new(config);
        // Feed data > 75% of max_line_bytes (15 bytes) without a newline.
        parser.feed(b"0123456789abcdef");
        let degradation = parser.detect_degradation();
        let is_high = matches!(degradation, IngestDegradation::HighBufferPressure { .. });
        assert!(is_high, "Expected HighBufferPressure, got {:?}", degradation);
    }

    #[test]
    fn test_ingest_log_entry() {
        let mut parser = IngestParser::with_defaults();
        parser.feed(b"test line\n");
        let entry = parser.log_entry();
        assert_eq!(entry.total_lines, 1);
        assert_eq!(entry.degradation, IngestDegradation::Healthy);
    }

    #[test]
    fn test_ingest_degradation_serde() {
        let variants = vec![
            IngestDegradation::Healthy,
            IngestDegradation::HighBufferPressure { buffered_bytes: 100, max_line_bytes: 120 },
            IngestDegradation::DataCorruption { invalid_bytes: 10, total_bytes: 200 },
            IngestDegradation::LowZeroCopy { ratio: 0.3, threshold: 0.5 },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: IngestDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_ingest_log_entry_serde() {
        let entry = IngestLogEntry {
            total_bytes: 1000,
            total_lines: 50,
            zero_copy_ratio: 0.8,
            buffered_bytes: 5,
            degradation: IngestDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: IngestLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_ingest_degradation_display() {
        assert_eq!(format!("{}", IngestDegradation::Healthy), "HEALTHY");
        let buf = IngestDegradation::HighBufferPressure { buffered_bytes: 100, max_line_bytes: 120 };
        assert!(format!("{}", buf).contains("100/120"));
    }

    // ── C3: Tiered Scrollback Tests ────────────────────────────────

    #[test]
    fn test_scrollback_tier_rank() {
        assert_eq!(ScrollbackTier::Hot.rank(), 0);
        assert_eq!(ScrollbackTier::Warm.rank(), 1);
        assert_eq!(ScrollbackTier::Cold.rank(), 2);
    }

    #[test]
    fn test_scrollback_tier_demote() {
        assert_eq!(ScrollbackTier::Hot.demote(), Some(ScrollbackTier::Warm));
        assert_eq!(ScrollbackTier::Warm.demote(), Some(ScrollbackTier::Cold));
        assert_eq!(ScrollbackTier::Cold.demote(), None);
    }

    #[test]
    fn test_scrollback_tier_display() {
        assert_eq!(format!("{}", ScrollbackTier::Hot), "HOT");
        assert_eq!(format!("{}", ScrollbackTier::Warm), "WARM");
        assert_eq!(format!("{}", ScrollbackTier::Cold), "COLD");
    }

    #[test]
    fn test_scrollback_tier_all() {
        assert_eq!(ScrollbackTier::ALL.len(), 3);
        for (i, tier) in ScrollbackTier::ALL.iter().enumerate() {
            assert_eq!(tier.rank(), i);
        }
    }

    #[test]
    fn test_tier_config_default() {
        let config = TierConfig::default();
        assert_eq!(config.tier, ScrollbackTier::Hot);
        assert!(config.max_bytes > 0);
        assert_eq!(config.compression_ratio, 1.0);
    }

    #[test]
    fn test_migration_policy_default() {
        let policy = TierMigrationPolicy::default();
        assert!(policy.hot_to_warm_age_us > 0);
        assert!(policy.warm_to_cold_age_us > policy.hot_to_warm_age_us);
        assert!(policy.pressure_threshold > 0.0 && policy.pressure_threshold < 1.0);
        assert!(policy.max_concurrent_migrations > 0);
    }

    #[test]
    fn test_tiered_scrollback_ingest() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        let id = mgr.ingest(1, 1024, 10, 1000);
        assert_eq!(id, 0);
        assert_eq!(mgr.segment_count(), 1);
        assert_eq!(mgr.total_bytes(), 1024);
        let seg = mgr.segment(id).unwrap();
        assert_eq!(seg.tier, ScrollbackTier::Hot);
        assert_eq!(seg.pane_id, 1);
    }

    #[test]
    fn test_tiered_scrollback_touch() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        let id = mgr.ingest(1, 1024, 10, 1000);
        mgr.touch(id, 5000);
        let seg = mgr.segment(id).unwrap();
        assert_eq!(seg.last_accessed_us, 5000);
    }

    #[test]
    fn test_tiered_scrollback_migrate_age() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 1000,
            warm_to_cold_age_us: 5000,
            min_segment_bytes: 100,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1_000_000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 1_000_000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10_000_000, target_latency_us: 10000, compression_ratio: 0.25 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);

        mgr.ingest(1, 500, 5, 0);
        // Not old enough — no migration
        assert_eq!(mgr.migrate(500), 0);
        // Old enough → hot→warm
        let migrated = mgr.migrate(2000);
        assert_eq!(migrated, 1);
        assert_eq!(mgr.segment(0).unwrap().tier, ScrollbackTier::Warm);
        assert_eq!(mgr.total_migrations, 1);
    }

    #[test]
    fn test_tiered_scrollback_migrate_warm_to_cold() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 100,
            warm_to_cold_age_us: 500,
            min_segment_bytes: 100,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1_000_000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 1_000_000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10_000_000, target_latency_us: 10000, compression_ratio: 0.5 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);

        mgr.ingest(1, 1000, 10, 0);
        mgr.migrate(200);  // hot→warm
        assert_eq!(mgr.segment(0).unwrap().tier, ScrollbackTier::Warm);

        mgr.migrate(800);  // warm→cold
        let seg = mgr.segment(0).unwrap();
        assert_eq!(seg.tier, ScrollbackTier::Cold);
        assert!(seg.compressed);
        // 1000 * 0.5 = 500
        assert_eq!(seg.byte_size, 500);
    }

    #[test]
    fn test_tiered_scrollback_conservation() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        mgr.ingest(2, 2000, 20, 0);
        // Before migration, all in hot
        let snap = mgr.snapshot();
        assert_eq!(snap.total_bytes, 3000);
        assert_eq!(snap.hot_bytes, 3000);
        assert_eq!(snap.warm_bytes, 0);
        assert_eq!(snap.cold_bytes, 0);
    }

    #[test]
    fn test_tiered_scrollback_utilization() {
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 5000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10000, target_latency_us: 10000, compression_ratio: 0.25 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, TierMigrationPolicy::default());

        mgr.ingest(1, 500, 5, 0);
        assert!((mgr.hot_utilization() - 0.5).abs() < 0.001);
        assert_eq!(mgr.warm_utilization(), 0.0);
    }

    #[test]
    fn test_tiered_scrollback_evict_pane() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        mgr.ingest(2, 2000, 20, 0);
        mgr.ingest(1, 500, 5, 0);

        mgr.evict_pane(1);
        assert_eq!(mgr.segment_count(), 1);
        assert_eq!(mgr.total_bytes(), 2000);
    }

    #[test]
    fn test_tiered_scrollback_reset() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        mgr.reset();
        assert_eq!(mgr.segment_count(), 0);
        assert_eq!(mgr.total_bytes(), 0);
        assert_eq!(mgr.total_migrations, 0);
    }

    #[test]
    fn test_tiered_scrollback_snapshot_serde() {
        let snap = TieredScrollbackSnapshot {
            hot_bytes: 100,
            warm_bytes: 200,
            cold_bytes: 300,
            hot_segments: 1,
            warm_segments: 2,
            cold_segments: 3,
            total_migrations: 5,
            total_bytes: 600,
            hot_utilization: 0.5,
            warm_utilization: 0.3,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TieredScrollbackSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_scrollback_degradation_healthy() {
        let mgr = TieredScrollbackManager::with_defaults();
        assert_eq!(mgr.detect_degradation(), ScrollbackDegradation::Healthy);
    }

    #[test]
    fn test_scrollback_degradation_hot_pressure() {
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 10000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 100000, target_latency_us: 10000, compression_ratio: 0.25 };
        let policy = TierMigrationPolicy { pressure_threshold: 0.8, ..Default::default() };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 900, 10, 0);
        let is_pressure = matches!(mgr.detect_degradation(), ScrollbackDegradation::HotPressure { .. });
        assert!(is_pressure, "Expected HotPressure, got {:?}", mgr.detect_degradation());
    }

    #[test]
    fn test_scrollback_degradation_display() {
        assert_eq!(format!("{}", ScrollbackDegradation::Healthy), "HEALTHY");
        let hot = ScrollbackDegradation::HotPressure { utilization: 0.9, threshold: 0.85 };
        assert!(format!("{}", hot).contains("90.0%"));
    }

    #[test]
    fn test_scrollback_log_entry() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1024, 10, 0);
        let entry = mgr.log_entry();
        assert_eq!(entry.hot_bytes, 1024);
        assert_eq!(entry.total_segments, 1);
        assert_eq!(entry.degradation, ScrollbackDegradation::Healthy);
    }

    #[test]
    fn test_scrollback_log_entry_serde() {
        let entry = ScrollbackLogEntry {
            hot_bytes: 100,
            warm_bytes: 200,
            cold_bytes: 300,
            total_segments: 6,
            total_migrations: 3,
            degradation: ScrollbackDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ScrollbackLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_scrollback_degradation_serde() {
        let variants = vec![
            ScrollbackDegradation::Healthy,
            ScrollbackDegradation::HotPressure { utilization: 0.9, threshold: 0.85 },
            ScrollbackDegradation::WarmPressure { utilization: 0.88, threshold: 0.85 },
            ScrollbackDegradation::MigrationBacklog { pending: 10, max_concurrent: 4 },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: ScrollbackDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_tiered_scrollback_status_line() {
        let mgr = TieredScrollbackManager::with_defaults();
        let line = mgr.status_line();
        assert!(line.contains("scrollback"));
        assert!(line.contains("migrations=0"));
    }

    #[test]
    fn test_tiered_scrollback_pressure_migration() {
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 10000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 100000, target_latency_us: 10000, compression_ratio: 0.25 };
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 1_000_000_000, // Very long — won't trigger by age
            warm_to_cold_age_us: 1_000_000_000,
            min_segment_bytes: 100,
            pressure_threshold: 0.8,
            max_concurrent_migrations: 10,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        // Fill hot tier past 80%
        mgr.ingest(1, 500, 5, 0);
        mgr.ingest(2, 400, 4, 0);
        // 900/1000 = 90% > 80% threshold → pressure migration
        let migrated = mgr.migrate(1);
        assert!(migrated > 0, "Expected pressure-driven migration");
    }

    #[test]
    fn test_tiered_scrollback_max_concurrent() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 0, // Always migrate
            warm_to_cold_age_us: 1_000_000_000,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 2,
        };
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1_000_000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 1_000_000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10_000_000, target_latency_us: 10000, compression_ratio: 0.25 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        for i in 0..5 {
            mgr.ingest(i, 100, 1, 0);
        }
        let migrated = mgr.migrate(1);
        assert_eq!(migrated, 2, "Should respect max_concurrent_migrations");
    }

    #[test]
    fn test_tiered_scrollback_min_segment_filter() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 0,
            warm_to_cold_age_us: 0,
            min_segment_bytes: 500,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1_000_000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 1_000_000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10_000_000, target_latency_us: 10000, compression_ratio: 0.25 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 100, 1, 0); // Too small
        mgr.ingest(2, 600, 5, 0); // Large enough
        let migrated = mgr.migrate(1);
        assert_eq!(migrated, 1);
        assert_eq!(mgr.segment(0).unwrap().tier, ScrollbackTier::Hot);
        assert_eq!(mgr.segment(1).unwrap().tier, ScrollbackTier::Warm);
    }

    #[test]
    fn test_tier_migration_event_serde() {
        let evt = TierMigrationEvent {
            segment_id: 42,
            from_tier: ScrollbackTier::Hot,
            to_tier: ScrollbackTier::Warm,
            bytes_migrated: 1024,
            duration_us: 50,
            timestamp_us: 99999,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: TierMigrationEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn test_scrollback_segment_serde() {
        let seg = ScrollbackSegment {
            segment_id: 1,
            pane_id: 2,
            tier: ScrollbackTier::Warm,
            byte_size: 4096,
            line_count: 100,
            created_us: 1000,
            last_accessed_us: 2000,
            compressed: false,
        };
        let json = serde_json::to_string(&seg).unwrap();
        let back: ScrollbackSegment = serde_json::from_str(&json).unwrap();
        assert_eq!(seg, back);
    }

    // ── C3 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_tiered_scrollback_ingest_bulk() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        let items = vec![(1, 100, 10), (2, 200, 20), (3, 300, 30)];
        let ids = mgr.ingest_bulk(&items, 0);
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(mgr.segment_count(), 3);
        assert_eq!(mgr.total_bytes(), 600);
    }

    #[test]
    fn test_tiered_scrollback_segments_for_pane() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 100, 10, 0);
        mgr.ingest(2, 200, 20, 0);
        mgr.ingest(1, 300, 30, 0);
        let pane1 = mgr.segments_for_pane(1);
        assert_eq!(pane1.len(), 2);
        assert_eq!(pane1[0].byte_size, 100);
        assert_eq!(pane1[1].byte_size, 300);
    }

    #[test]
    fn test_tiered_scrollback_tier_bytes() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        assert_eq!(mgr.tier_bytes(ScrollbackTier::Hot), 1000);
        assert_eq!(mgr.tier_bytes(ScrollbackTier::Warm), 0);
        assert_eq!(mgr.tier_bytes(ScrollbackTier::Cold), 0);
    }

    #[test]
    fn test_tiered_scrollback_total_lines() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 100, 10, 0);
        mgr.ingest(2, 200, 25, 0);
        assert_eq!(mgr.total_lines(), 35);
    }

    #[test]
    fn test_tiered_scrollback_evict_hot_to_target() {
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 10000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 100000, target_latency_us: 10000, compression_ratio: 0.25 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, TierMigrationPolicy::default());

        mgr.ingest(1, 300, 10, 100);
        mgr.ingest(2, 300, 10, 200);
        mgr.ingest(3, 300, 10, 300);
        // 900/1000 = 90%. Evict to 50%.
        let freed = mgr.evict_hot_to_target(0.5);
        assert!(freed >= 400, "Should have freed enough to reach 50%: freed={}", freed);
        assert!(mgr.hot_utilization() <= 0.51);
    }

    #[test]
    fn test_tiered_scrollback_oldest_hot() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 100, 10, 1000);
        mgr.ingest(2, 200, 20, 2000);
        let oldest = mgr.oldest_hot_segment().unwrap();
        assert_eq!(oldest.created_us, 1000);
        assert_eq!(mgr.oldest_hot_age_us(5000), 4000);
    }

    #[test]
    fn test_tiered_scrollback_oldest_hot_empty() {
        let mgr = TieredScrollbackManager::with_defaults();
        assert!(mgr.oldest_hot_segment().is_none());
        assert_eq!(mgr.oldest_hot_age_us(1000), 0);
    }

    #[test]
    fn test_tiered_scrollback_active_pane_ids() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(3, 100, 10, 0);
        mgr.ingest(1, 200, 20, 0);
        mgr.ingest(3, 300, 30, 0);
        mgr.ingest(2, 400, 40, 0);
        let ids = mgr.active_pane_ids();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_tiered_scrollback_cold_utilization() {
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 5000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10000, target_latency_us: 10000, compression_ratio: 0.5 };
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 10,
            warm_to_cold_age_us: 100,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 2000, 20, 0);
        mgr.migrate(50);   // hot→warm
        mgr.migrate(200);  // warm→cold
        // 2000 * 0.5 = 1000 cold bytes, util = 1000/10000 = 0.1
        assert!((mgr.cold_utilization() - 0.1).abs() < 0.01);
    }

    #[test]
    fn test_tiered_scrollback_migration_events_recorded() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 0,
            warm_to_cold_age_us: 1_000_000,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig { tier: ScrollbackTier::Hot, max_bytes: 1_000_000, target_latency_us: 10, compression_ratio: 1.0 };
        let warm = TierConfig { tier: ScrollbackTier::Warm, max_bytes: 1_000_000, target_latency_us: 500, compression_ratio: 1.0 };
        let cold = TierConfig { tier: ScrollbackTier::Cold, max_bytes: 10_000_000, target_latency_us: 10000, compression_ratio: 0.25 };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 500, 5, 0);
        mgr.ingest(2, 600, 6, 0);
        mgr.migrate(1);
        let events = mgr.recent_migrations();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].from_tier, ScrollbackTier::Hot);
        assert_eq!(events[0].to_tier, ScrollbackTier::Warm);
    }
}
